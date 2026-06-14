//! Kubernetes-backend driver. Self-provisions its own minikube cluster + Rise
//! Helm release (minikube start + helm + the JFrog/Vault registry stack), so
//! scenarios run the same way as on Docker. The in-cluster Rise server and Dex
//! are reached via background `kubectl port-forward`s (killed on teardown/drop),
//! and the CLI runs from the image on the host network.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use super::{Backend, BackendKind, CliAuth, SampleApp};
use crate::cli::{self, CliOutput};
use crate::dex::DexEndpoint;
use crate::http::{self, HttpResponse};
use crate::token;

// Matches helm/rise/values-ci.yaml (config.server.jwt_signing_secret).
const SECRET_B64: &str = "dGVzdC1qd3Qtc2VjcmV0LWtleS1mb3ItY2ktdGVzdGluZy1vbmx5LW5vdC1zZWN1cmU=";
const NAMESPACE: &str = "rise-ci";
const RELEASE: &str = "rise-ci";
// Release `rise-ci` + chart name `chart` → workloads/services are `rise-ci-chart*`.
const SERVER_SVC: &str = "rise-ci-chart";
const SERVER_FQDN: &str = "rise-ci-chart.rise-ci.svc.cluster.local:3000";
const DEX_SVC: &str = "rise-ci-chart-dex";
// What Dex stamps in `iss` (in-cluster DNS), regardless of the host we mint from.
const DEX_ISSUER: &str = "http://rise-ci-chart-dex.rise-ci.svc.cluster.local:5556/dex";
const DEFAULT_PUBLIC_URL: &str = "http://rise.local";
const RISE_URL: &str = "http://127.0.0.1:3000";
const DEX_LOCAL_URL: &str = "http://127.0.0.1:5556/dex";
// Local port for the per-app reach port-forward (scenarios run serially).
const APP_LOCAL_PORT: u16 = 18080;
// jfrog-vault mode: deployed app pods reach the issuer at the in-cluster FQDN,
// so the CI token's iss/aud + server public_url use it (not http://rise.local).
const JFROG_PUBLIC_URL: &str = "http://rise-ci-chart.rise-ci.svc.cluster.local:3000";
// The docker network the compose stack + minikube node share (jfrog-vault).
const COMPOSE_NETWORK: &str = "rise_default";
const VAULT_ROLE_URL: &str = "http://127.0.0.1:8200/v1/artifactory/roles/rise";

#[derive(Clone, Copy, PartialEq, Eq)]
enum RegistryMode {
    OciClientAuth,
    JfrogVault,
}

impl RegistryMode {
    fn from_env() -> Self {
        match std::env::var("RISE_E2E_REGISTRY_MODE").ok().as_deref() {
            Some("jfrog-vault") => Self::JfrogVault,
            _ => Self::OciClientAuth,
        }
    }
}

/// A `kubectl port-forward` running in the background, killed on drop.
struct PortForward {
    child: Child,
}

impl PortForward {
    fn spawn(namespace: &str, target: &str, ports: &str, what: &str) -> Result<Self> {
        let child = Command::new("kubectl")
            .args(["-n", namespace, "port-forward", target, ports])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("spawn kubectl port-forward for {what}"))?;
        Ok(Self { child })
    }
}

impl Drop for PortForward {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub struct MinikubeBackend {
    image_repository: String,
    image_tag: String,
    /// `repo:tag` for the CLI image (defaults to the server image).
    cli_image: String,
    registry_mode: RegistryMode,
    cpus: String,
    memory: String,
    ci_token: String,
    dex: DexEndpoint,
    /// Repo root (the chart + values live here). `cargo test` runs the test binary
    /// with CWD = the crate dir, so all repo paths must be absolute.
    repo_root: PathBuf,
    /// Long-lived forwards (server, Dex) kept alive for the whole run.
    forwards: Vec<PortForward>,
}

impl MinikubeBackend {
    pub fn new() -> Result<Self> {
        let image_repository = std::env::var("RISE_IMAGE_REPOSITORY")
            .unwrap_or_else(|_| "ghcr.io/rise-deploy/rise".to_string());
        let image_tag = std::env::var("RISE_IMAGE_TAG")
            .context("RISE_IMAGE_TAG must be set for the minikube backend")?;
        let cli_repo =
            std::env::var("RISE_CLI_IMAGE_REPOSITORY").unwrap_or_else(|_| image_repository.clone());
        let registry_mode = RegistryMode::from_env();
        // The CI token's iss/aud must equal the server's public_url, which differs
        // by mode (jfrog-vault deploys must reach the issuer from inside the cluster).
        let public_url = std::env::var("RISE_PUBLIC_URL").unwrap_or_else(|_| match registry_mode {
            RegistryMode::JfrogVault => JFROG_PUBLIC_URL.to_string(),
            RegistryMode::OciClientAuth => DEFAULT_PUBLIC_URL.to_string(),
        });
        let ci_token = token::mint_ci_token(SECRET_B64, &public_url)?;
        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .canonicalize()
            .context("resolve repo root from CARGO_MANIFEST_DIR")?;
        Ok(Self {
            cli_image: format!("{cli_repo}:{image_tag}"),
            image_repository,
            image_tag,
            registry_mode,
            cpus: std::env::var("MINIKUBE_CPUS").unwrap_or_else(|_| "4".to_string()),
            memory: std::env::var("MINIKUBE_MEMORY").unwrap_or_else(|_| "6144".to_string()),
            ci_token,
            dex: DexEndpoint {
                token_url: format!("{DEX_LOCAL_URL}/token"),
                client_id: "rise-backend".to_string(),
                client_secret: "rise-backend-secret".to_string(),
                issuer: DEX_ISSUER.to_string(),
            },
            repo_root,
            forwards: Vec::new(),
        })
    }

    /// Absolute path to a repo file/dir (CWD is the crate dir under `cargo test`).
    fn repo_path(&self, rel: &str) -> String {
        self.repo_root.join(rel).to_string_lossy().into_owned()
    }

    /// The `helm upgrade --install` flags, including the jfrog-vault registry
    /// overrides when that mode is selected.
    fn helm_args(&self) -> Vec<String> {
        let mut args = vec![
            "--namespace".into(),
            NAMESPACE.into(),
            "--create-namespace".into(),
            "--values".into(),
            self.repo_path("helm/rise/values-ci.yaml"),
            "--set".into(),
            format!("image.repository={}", self.image_repository),
            "--set".into(),
            format!("image.tag={}", self.image_tag),
            "--set".into(),
            "image.pullPolicy=Always".into(),
            "--set-string".into(),
            format!("config.deployment_controller.auth_backend_url=http://{SERVER_FQDN}"),
            "--set-string".into(),
            "config.deployment_controller.auth_signin_url=http://rise.local".into(),
        ];
        if self.registry_mode == RegistryMode::JfrogVault {
            let pairs = [
                ("config.server.public_url", JFROG_PUBLIC_URL),
                ("config.registry.type", "jfrog"),
                ("config.registry.registry_host", "rise-jfrog:8082"),
                ("config.registry.client_registry_url", "localhost:3082"),
                ("config.registry.docker_repo_key", "rise-docker-local"),
                ("config.registry.token_provider.type", "vault"),
                (
                    "config.registry.token_provider.vault_addr",
                    "http://host.minikube.internal:8200",
                ),
                ("config.registry.token_provider.vault_token", "root"),
                (
                    "config.registry.token_provider.vault_mount_path",
                    "artifactory",
                ),
                ("config.registry.token_provider.vault_role", "rise"),
            ];
            for (k, v) in pairs {
                args.push("--set-string".into());
                args.push(format!("{k}={v}"));
            }
            args.push("--set".into());
            args.push("config.registry.mint_pull_secrets=true".into());
        }
        args
    }

    /// `docker compose --profile jfrog-vault <args>` run from the repo root.
    fn compose(&self, extra: &[&str]) -> Command {
        let mut c = Command::new("docker");
        c.current_dir(&self.repo_root)
            .args(["compose", "--profile", "jfrog-vault"])
            .args(extra);
        c
    }

    /// Bring up the JFrog + Vault services and wait for Vault to mint the
    /// Artifactory role (the registry token provider depends on it).
    fn start_jfrog_vault(&self) -> Result<()> {
        // Start from a clean slate — a prior crashed run can leave a populated
        // vault_plugins volume / Artifactory state that would be silently reused.
        let _ = cli::run(self.compose(&["down", "-v"]));
        cli::run_checked(self.compose(&["up", "-d", "jfrog", "vault"]))
            .context("docker compose up jfrog vault")?;
        http::poll(
            Duration::from_secs(600),
            Duration::from_secs(5),
            "Vault Artifactory role to be ready",
            || {
                Ok(
                    http::get_auth_header(VAULT_ROLE_URL, "X-Vault-Token", "root")
                        .map(|r| r.status == 200)
                        .unwrap_or(false),
                )
            },
        )
    }

    /// Wire the minikube node's containerd to pull from the JFrog registry over
    /// the shared docker network.
    fn configure_jfrog_registry(&self) -> Result<()> {
        let nodes = cli::run_checked({
            let mut c = Command::new("minikube");
            c.arg("node").arg("list");
            c
        })
        .context("minikube node list")?;
        for line in nodes.stdout.lines() {
            let Some(node) = line.split_whitespace().next() else {
                continue;
            };
            // Best-effort: connect the node container to the compose network.
            let _ = cli::run({
                let mut c = Command::new("docker");
                c.args(["network", "connect", COMPOSE_NETWORK, node]);
                c
            });
            cli::run_checked({
                let mut c = Command::new("docker");
                c.args([
                    "exec",
                    node,
                    "mkdir",
                    "-p",
                    "/etc/containerd/certs.d/rise-jfrog:8082",
                ]);
                c
            })
            .context("create containerd certs.d dir on node")?;
            // Write hosts.toml into the node via `docker exec -i ... cp /dev/stdin`.
            let mut c = Command::new("docker");
            c.args([
                "exec",
                "-i",
                node,
                "cp",
                "/dev/stdin",
                "/etc/containerd/certs.d/rise-jfrog:8082/hosts.toml",
            ]);
            c.stdin(std::process::Stdio::piped());
            let mut child = c.spawn().context("spawn docker exec cp hosts.toml")?;
            use std::io::Write as _;
            child
                .stdin
                .take()
                .context("child stdin")?
                .write_all(b"[host.\"http://rise-jfrog:8082\"]\n")
                .context("write hosts.toml")?;
            let status = child.wait().context("docker exec cp hosts.toml")?;
            anyhow::ensure!(status.success(), "writing hosts.toml to node {node} failed");
        }
        Ok(())
    }

    /// Discover the single app Service (name + first port) in `ns`, polling until
    /// the controller has created it.
    fn find_app_svc(&self, ns: &str) -> Result<(String, u16)> {
        let mut found = None;
        http::poll(
            Duration::from_secs(120),
            Duration::from_secs(2),
            &format!("app Service in namespace {ns}"),
            || {
                let mut c = Command::new("kubectl");
                c.args([
                    "get",
                    "svc",
                    "-n",
                    ns,
                    "-o",
                    "jsonpath={.items[0].metadata.name} {.items[0].spec.ports[0].port}",
                ]);
                let out = cli::run(c)?;
                let mut it = out.stdout.split_whitespace();
                if let (Some(name), Some(port)) = (it.next(), it.next()) {
                    if let Ok(port) = port.parse::<u16>() {
                        found = Some((name.to_string(), port));
                        return Ok(true);
                    }
                }
                Ok(false)
            },
        )?;
        found.context("app Service not found")
    }

    /// Run the CLI from the image on the host network (reaches the port-forward),
    /// with the repo root as the workdir so deploy-from-source paths resolve.
    /// `mount_docker` also bind-mounts the docker socket for `--backend docker:build`.
    fn run_cli(
        &self,
        args: &[&str],
        auth: Option<CliAuth<'_>>,
        mount_docker: bool,
    ) -> Result<CliOutput> {
        let pwd_str = self.repo_root.to_string_lossy().to_string();
        let mut c = Command::new("docker");
        c.args(["run", "--rm", "--network", "host"])
            .args(["-e", &format!("RISE_URL={RISE_URL}")])
            .args(["-e", &format!("MISE_TRUSTED_CONFIG_PATHS={pwd_str}")])
            .args(["-v", &format!("{pwd_str}:{pwd_str}")])
            .args(["-w", &pwd_str]);
        if mount_docker {
            c.args(["-v", "/var/run/docker.sock:/var/run/docker.sock"])
                .args(["-e", "DOCKER_CONFIG=/tmp/rise-docker-config"]);
        }
        match &auth {
            Some(a) => {
                c.args(["-e", &format!("RISE_TOKEN={}", a.token)]);
                if let Some(id) = a.identity {
                    c.args(["-e", &format!("RISE_IDENTITY={id}")]);
                }
            }
            None => {
                c.args(["-e", &format!("RISE_TOKEN={}", self.ci_token)]);
            }
        }
        c.args(["--entrypoint", "/usr/local/bin/rise", &self.cli_image]);
        c.args(args);
        cli::run(c)
    }
}

impl Backend for MinikubeBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Minikube
    }

    fn bring_up(&mut self) -> Result<()> {
        let jfrog = self.registry_mode == RegistryMode::JfrogVault;

        // Clean slate (a fresh cluster exercises chart bootstrap from scratch).
        let mut del = Command::new("minikube");
        del.arg("delete");
        let _ = cli::run(del);

        if jfrog {
            self.start_jfrog_vault()?;
        }

        let mut start = Command::new("minikube");
        start.args([
            "start",
            "--driver=docker",
            &format!("--cpus={}", self.cpus),
            &format!("--memory={}", self.memory),
        ]);
        if jfrog {
            start.arg("--insecure-registry=rise-jfrog:8082");
        }
        cli::run_checked(start).context("minikube start")?;

        let mut ingress = Command::new("minikube");
        ingress.args(["addons", "enable", "ingress"]);
        cli::run_checked(ingress).context("minikube addons enable ingress")?;

        if jfrog {
            self.configure_jfrog_registry()?;
        }

        let mut helm = Command::new("helm");
        helm.args([
            "upgrade",
            "--install",
            RELEASE,
            &self.repo_path("helm/rise"),
        ]);
        helm.args(self.helm_args());
        cli::run_checked(helm).context("helm upgrade --install")?;

        let mut wait_dep = Command::new("kubectl");
        wait_dep.args([
            "wait",
            "--namespace",
            NAMESPACE,
            "--for=condition=Available",
            "deployment",
            "-l",
            &format!("app.kubernetes.io/instance={RELEASE}"),
            "--timeout=10m",
        ]);
        cli::run_checked(wait_dep).context("kubectl wait deployments Available")?;

        let mut wait_pod = Command::new("kubectl");
        wait_pod.args([
            "wait",
            "--namespace",
            NAMESPACE,
            "--for=condition=Ready",
            "pod",
            "-l",
            &format!("app.kubernetes.io/instance={RELEASE}"),
            "--timeout=10m",
        ]);
        cli::run_checked(wait_pod).context("kubectl wait pods Ready")?;

        // Forward the server and Dex for the whole run (killed in tear_down/drop).
        self.forwards.push(PortForward::spawn(
            NAMESPACE,
            &format!("svc/{SERVER_SVC}"),
            "3000:3000",
            "rise server",
        )?);
        self.forwards.push(PortForward::spawn(
            NAMESPACE,
            &format!("svc/{DEX_SVC}"),
            "5556:5556",
            "dex",
        )?);

        // The forwards take a moment to establish — swallow connection errors.
        http::poll(
            Duration::from_secs(60),
            Duration::from_secs(2),
            "rise server /health (port-forward)",
            || {
                Ok(http::get(&format!("{RISE_URL}/health"), None)
                    .map(|r| r.status == 200)
                    .unwrap_or(false))
            },
        )?;
        http::poll(
            Duration::from_secs(60),
            Duration::from_secs(2),
            "dex discovery (port-forward)",
            || {
                Ok(http::get(
                    &format!("{DEX_LOCAL_URL}/.well-known/openid-configuration"),
                    None,
                )
                .map(|r| r.status == 200)
                .unwrap_or(false))
            },
        )?;
        Ok(())
    }

    fn tear_down(&mut self) {
        // Drop the port-forwards (kills the kubectl children), then nuke the cluster.
        self.forwards.clear();
        let mut del = Command::new("minikube");
        del.arg("delete");
        let _ = cli::run(del);
        if self.registry_mode == RegistryMode::JfrogVault {
            // `down -v` (not `rm`) so the named volumes + the rise_default network
            // are removed too, leaving no state for the next run. The node was
            // already removed by `minikube delete` above, so the network is free.
            let _ = cli::run(self.compose(&["down", "-v"]));
        }
    }

    fn rise_cli(&self, args: &[&str], auth: Option<CliAuth<'_>>) -> Result<CliOutput> {
        self.run_cli(args, auth, false)
    }

    fn rise_cli_build(&self, args: &[&str], auth: Option<CliAuth<'_>>) -> Result<CliOutput> {
        // Source builds need the host docker socket inside the CLI container.
        self.run_cli(args, auth, true)
    }

    fn reach_app(&self, project: &str, path: &str) -> Result<Option<HttpResponse>> {
        // Rise namespaces an app as `rise-<project>`; forward its Service locally.
        let ns = format!("rise-{project}");
        let (svc, port) = self.find_app_svc(&ns)?;
        let _pf = PortForward::spawn(
            &ns,
            &format!("svc/{svc}"),
            &format!("{APP_LOCAL_PORT}:{port}"),
            &format!("app {project}"),
        )?;

        let url = format!("http://127.0.0.1:{APP_LOCAL_PORT}{path}");
        let mut resp = None;
        http::poll(
            Duration::from_secs(60),
            Duration::from_secs(2),
            &format!("app {project} reachable"),
            || match http::get(&url, None) {
                // Forward warmup → connection refused; route/pod warmup → 404/5xx.
                Ok(r) => {
                    let ready = r.status != 404 && r.status < 500;
                    resp = Some(r);
                    Ok(ready)
                }
                Err(_) => Ok(false),
            },
        )?;
        Ok(resp)
    }

    fn dex(&self) -> Option<&DexEndpoint> {
        Some(&self.dex)
    }

    fn sample_app(&self) -> SampleApp {
        // K8s app pods run with runAsNonRoot, so use a non-root image on a high port.
        SampleApp {
            image: "nginxinc/nginx-unprivileged:alpine",
            http_port: "8080",
            body_marker: Some("nginx"),
        }
    }

    fn api_base(&self) -> &str {
        RISE_URL
    }

    fn ci_bearer(&self) -> &str {
        &self.ci_token
    }

    fn supports_source_build(&self) -> bool {
        self.registry_mode == RegistryMode::JfrogVault
    }

    fn reapply_chart(&self) -> Result<()> {
        // Re-run `helm upgrade` (no --install) with the same args; asserts the
        // chart applies cleanly a second time.
        let mut helm = Command::new("helm");
        helm.args(["upgrade", RELEASE, &self.repo_path("helm/rise")]);
        helm.args(self.helm_args());
        cli::run_checked(helm).context("helm upgrade (idempotency)")?;
        Ok(())
    }

    fn wait_workload_removed(&self, project: &str) -> Result<()> {
        // Stronger than the default: the app namespace must have zero pods, which
        // proves a post-stop /logs query is served by Loki, not live kubelet.
        let ns = format!("rise-{project}");
        http::poll(
            Duration::from_secs(120),
            Duration::from_secs(2),
            &format!("app pods in {ns} to be removed"),
            || {
                let mut c = Command::new("kubectl");
                c.args(["get", "pods", "-n", &ns, "--no-headers"]);
                let out = cli::run(c)?;
                // Only trust empty stdout when kubectl actually succeeded — on a
                // transient error kubectl exits non-zero with empty stdout (the
                // message goes to stderr), which must NOT read as "pods gone".
                // `kubectl` prints "No resources found" to stderr; stdout is empty.
                Ok(out.success() && out.stdout.trim().is_empty())
            },
        )
    }
}
