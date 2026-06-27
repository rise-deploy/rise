//! Kubernetes-backend driver. Self-provisions its own minikube cluster + Rise
//! Helm release (minikube start + helm + the JFrog/Vault registry stack), so
//! scenarios run the same way as on Docker. The in-cluster Rise server and Dex
//! are reached via background `kubectl port-forward`s (killed on teardown/drop),
//! and the CLI runs from the image on the host network.

use anyhow::{Context, Result};
use base64::Engine as _;
use std::io::Read;
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use super::{Backend, BackendKind, CliAuth, SampleApp};
use crate::cli::{self, CliOutput};
use crate::dex::DexEndpoint;
use crate::http::{self, HttpResponse};
use crate::report;
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
// Where released charts are published (matches CHART_REGISTRY in the CI workflow);
// the chart name is `chart` (helm/rise/Chart.yaml). Used by the upgrade flow to
// install the OLD released chart before upgrading to the in-repo one.
const OLD_CHART_REF: &str = "oci://ghcr.io/rise-deploy/chart";

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
        let local_port: u16 = ports
            .split(':')
            .next()
            .and_then(|p| p.parse().ok())
            .with_context(|| format!("port-forward spec '{ports}' has no local port"))?;
        let addr = SocketAddr::from(([127, 0, 0, 1], local_port));

        // Wait for the local port to be free before binding. A just-dropped
        // forward may not have released it yet, and binding over a stale listener
        // would let the liveness probe below pass against the wrong forward.
        // Clearing it first means a later successful connect can only be ours.
        let free_by = Instant::now() + Duration::from_secs(5);
        while TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            if Instant::now() >= free_by {
                anyhow::bail!(
                    "local port {local_port} still in use before starting \
                     port-forward for {what}"
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        let mut child = Command::new("kubectl")
            .args(["-n", namespace, "port-forward", target, ports])
            .stdin(Stdio::null())
            // Connection-handling logs go to stdout (high volume) → discard;
            // errors go to stderr (low volume) → capture so a failed bind can be
            // explained. The successful path drains stderr on a thread so a
            // long-lived forward never blocks on a full pipe.
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawn kubectl port-forward for {what}"))?;
        let mut stderr = child.stderr.take().expect("piped stderr");

        // `spawn` only proves the process launched; block until the local port
        // actually accepts a connection (or kubectl dies) so a bad forward
        // surfaces as a clear infra error here instead of a window of
        // connection-refused misread as the workload itself failing.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                anyhow::bail!(
                    "kubectl port-forward for {what} exited early ({status}) — the \
                     forward target may have no ready endpoints{}",
                    err_detail(&mut stderr)
                );
            }
            if TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok() {
                std::thread::spawn(move || {
                    let _ = std::io::copy(&mut stderr, &mut std::io::sink());
                });
                return Ok(Self { child });
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!(
                    "kubectl port-forward for {what} never began listening on \
                     127.0.0.1:{local_port} within 10s{}",
                    err_detail(&mut stderr)
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for PortForward {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Read whatever an *already-exited* child wrote to a captured stderr pipe, as a
/// `": <msg>"` suffix for an error (empty when there's nothing). Blocks only
/// until EOF, which the dead child has already produced.
fn err_detail(stderr: &mut impl Read) -> String {
    let mut s = String::new();
    let _ = stderr.read_to_string(&mut s);
    let s = s.trim();
    if s.is_empty() {
        String::new()
    } else {
        format!(": {s}")
    }
}

pub struct MinikubeBackend {
    image_repository: String,
    /// The image tag the release currently runs. In the upgrade flow this starts
    /// at the older `RISE_E2E_UPGRADE_FROM` version and `upgrade` flips it to the
    /// target; otherwise it's always the target (`RISE_IMAGE_TAG`).
    image_tag: String,
    /// Repository the CLI image is pulled from (used to recompute `cli_image` on
    /// upgrade).
    cli_repo: String,
    /// `repo:tag` for the CLI image (defaults to the server image).
    cli_image: String,
    /// The target tag to switch to on `upgrade`; `Some` only in the upgrade flow.
    upgrade_target: Option<String>,
    registry_mode: RegistryMode,
    cpus: String,
    memory: String,
    ci_token: String,
    dex: DexEndpoint,
    /// Repo root (the chart + values live here). `cargo test` runs the test binary
    /// with CWD = the crate dir, so all repo paths must be absolute.
    repo_root: PathBuf,
    /// The minikube node IP (`minikube ip`), set in `bring_up`. The nginx ingress
    /// addon listens on it at :80 — used for ingress-level reach.
    node_ip: String,
    /// Long-lived forwards (server, Dex) kept alive for the whole run.
    forwards: Vec<PortForward>,
}

impl MinikubeBackend {
    pub fn new() -> Result<Self> {
        let image_repository = std::env::var("RISE_IMAGE_REPOSITORY")
            .unwrap_or_else(|_| "ghcr.io/rise-deploy/rise".to_string());
        let target_tag = std::env::var("RISE_IMAGE_TAG")
            .context("RISE_IMAGE_TAG must be set for the minikube backend")?;
        // In the upgrade flow the release first comes up on the older version and
        // `upgrade` switches it to the target tag (and the in-repo chart).
        let upgrade_from = std::env::var("RISE_E2E_UPGRADE_FROM")
            .ok()
            .filter(|s| !s.is_empty());
        let (image_tag, upgrade_target) = match upgrade_from {
            Some(old) => (old, Some(target_tag)),
            None => (target_tag, None),
        };
        // Treat an empty value as unset (CI passes "" when not overriding it).
        let cli_repo = std::env::var("RISE_CLI_IMAGE_REPOSITORY")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| image_repository.clone());
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
            cli_repo,
            upgrade_target,
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
            node_ip: String::new(),
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

    /// Hold one `kubectl port-forward` to the project's app Service open for the
    /// duration of `f`, passing it the local base URL (`http://127.0.0.1:<port>`).
    /// Both the one-shot reach and the held-open poll go through here so the
    /// forward setup — and its liveness guarantee (see `PortForward::spawn`) —
    /// lives in exactly one place.
    fn with_app_forward<T>(&self, project: &str, f: impl FnOnce(&str) -> Result<T>) -> Result<T> {
        let ns = format!("rise-{project}");
        let (svc, port) = self.find_app_svc(&ns)?;
        let _pf = PortForward::spawn(
            &ns,
            &format!("svc/{svc}"),
            &format!("{APP_LOCAL_PORT}:{port}"),
            &format!("app {project}"),
        )?;
        f(&format!("http://127.0.0.1:{APP_LOCAL_PORT}"))
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

    /// Wait for the Rise release to roll out, then (re-)establish the long-lived
    /// server + Dex port-forwards and confirm both answer. Shared by `bring_up`
    /// and `upgrade` (after a `helm upgrade` the server pod is recreated, so the
    /// existing forwards are stale and must be replaced).
    fn await_control_plane(&mut self) -> Result<()> {
        report::step("wait deployments Available (≤10m)", || {
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
            cli::run_checked(wait_dep).context("kubectl wait deployments Available")
        })?;

        report::step("wait pods Ready (≤10m)", || {
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
            cli::run_checked(wait_pod).context("kubectl wait pods Ready")
        })?;

        // (Re-)forward the server and Dex for the whole run (killed in
        // tear_down/drop). Clearing first drops any stale forwards to a pod the
        // upgrade replaced.
        report::step("port-forward server :3000 + dex :5556", || {
            self.forwards.clear();
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
            Ok(())
        })?;

        // The forwards take a moment to establish — swallow connection errors.
        report::step_value("rise /health", || {
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
            Ok("200")
        })?;
        report::step_value("dex discovery", || {
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
            Ok("200")
        })?;
        Ok(())
    }
}

impl Backend for MinikubeBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Minikube
    }

    fn bring_up(&mut self) -> Result<()> {
        let jfrog = self.registry_mode == RegistryMode::JfrogVault;

        // Clean slate (a fresh cluster exercises chart bootstrap from scratch).
        report::step("minikube delete (clean slate)", || {
            let mut del = Command::new("minikube");
            del.arg("delete");
            let _ = cli::run(del);
            Ok(())
        })?;

        if jfrog {
            report::step("jfrog + vault up (compose)", || self.start_jfrog_vault())?;
        }

        report::step(
            &format!("minikube start ({} cpus, {} MB)", self.cpus, self.memory),
            || {
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
                cli::run_checked(start).context("minikube start")
            },
        )?;

        report::step("enable ingress addon", || {
            let mut ingress = Command::new("minikube");
            ingress.args(["addons", "enable", "ingress"]);
            cli::run_checked(ingress).context("minikube addons enable ingress")
        })?;

        // The node IP is where the nginx ingress addon listens (:80); used for
        // ingress-level reach (the private-ingress-auth scenario).
        self.node_ip = report::step_value("minikube ip", || {
            let mut c = Command::new("minikube");
            c.arg("ip");
            Ok(cli::run_checked(c)
                .context("minikube ip")?
                .stdout
                .trim()
                .to_string())
        })?;

        if jfrog {
            report::step("wire node containerd → jfrog registry", || {
                self.configure_jfrog_registry()
            })?;
        }

        report::step("helm upgrade --install", || {
            let mut helm = Command::new("helm");
            helm.args(["upgrade", "--install", RELEASE]);
            // Upgrade flow: come up on the OLD released chart (pulled from the OCI
            // registry, version == the old tag) before upgrading to the in-repo
            // chart. Otherwise install the in-repo chart directly.
            match &self.upgrade_target {
                Some(_) => {
                    helm.arg(OLD_CHART_REF);
                    helm.args(["--version", &self.image_tag]);
                }
                None => {
                    helm.arg(self.repo_path("helm/rise"));
                }
            }
            helm.args(self.helm_args());
            cli::run_checked(helm).context("helm upgrade --install")
        })?;

        self.await_control_plane()
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
        self.with_app_forward(project, |base| {
            let url = format!("{base}{path}");
            let mut resp = None;
            http::poll(
                Duration::from_secs(60),
                Duration::from_secs(2),
                &format!("app {project} reachable"),
                || match http::get(&url, None) {
                    // The forward itself is already verified live by
                    // `PortForward::spawn`; pod/route warmup can still 404/5xx.
                    Ok(r) => {
                        let ready = r.status != 404 && r.status < 500;
                        resp = Some(r);
                        Ok(ready)
                    }
                    Err(_) => Ok(false),
                },
            )?;
            Ok(resp)
        })
    }

    fn poll_app(
        &self,
        project: &str,
        path: &str,
        duration: Duration,
        interval: Duration,
        check: &mut dyn FnMut(&str) -> bool,
    ) -> Result<bool> {
        // Hold ONE forward open for the whole window (dense, reliable sampling —
        // no per-sample forward churn).
        self.with_app_forward(project, |base| {
            let url = format!("{base}{path}");
            let start = Instant::now();
            while start.elapsed() < duration {
                if let Ok(r) = http::get(&url, None) {
                    if r.status == 200 && check(&r.body) {
                        return Ok(true);
                    }
                }
                std::thread::sleep(interval);
            }
            Ok(false)
        })
    }

    fn minted_token_jti(&self, project: &str, filename: &str) -> Result<Option<String>> {
        // Read the per-deployment identity Secret the controller writes — the
        // source, before kubelet propagates it to the pod's mounted file. Find the
        // Secret in the app namespace carrying the `token-<filename>` key, base64-
        // decode the JWT, and return its jti so a scenario can observe re-minting
        // independent of mount propagation.
        let ns = format!("rise-{project}");
        let key = format!("token-{filename}");
        let mut c = Command::new("kubectl");
        c.args(["get", "secret", "-n", &ns, "-o", "json"]);
        let out = cli::run(c)?;
        if !out.success() {
            return Ok(None);
        }
        let v: serde_json::Value =
            serde_json::from_str(&out.stdout).unwrap_or(serde_json::Value::Null);
        for secret in v["items"].as_array().into_iter().flatten() {
            if let Some(b64) = secret["data"][&key].as_str() {
                let jwt = base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .ok()
                    .and_then(|bytes| String::from_utf8(bytes).ok());
                return Ok(jwt.as_deref().and_then(token::jwt_unverified_jti));
            }
        }
        Ok(None)
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

    fn app_host(&self, project: &str) -> String {
        // Default-group deploy → the chart's production ingress URL template.
        format!("{project}.apps.rise.local")
    }

    fn ingress_get(
        &self,
        project: &str,
        path: &str,
        follow_redirects: bool,
        cookie: Option<&str>,
    ) -> Result<http::Resp> {
        // Reach the nginx ingress controller on the node IP (:80), Host-routed —
        // this exercises the ingress auth, unlike reach_app's Service port-forward.
        let host = self.app_host(project);
        http::request(
            &format!("http://{}{path}", self.node_ip),
            Some(&host),
            cookie,
            follow_redirects,
        )
    }

    fn assert_ingress_auth_configured(&self, project: &str) -> Result<()> {
        // The controller stamps nginx auth annotations on the app's Ingress
        // (resource `default` in namespace `rise-<project>`). Poll until present.
        let ns = format!("rise-{project}");
        let mut last = String::new();
        http::poll(
            Duration::from_secs(60),
            Duration::from_secs(2),
            &format!("nginx auth annotations on ingress in {ns}"),
            || {
                let mut c = Command::new("kubectl");
                c.args(["get", "ingress", "-n", &ns, "-o", "json"]);
                let out = cli::run(c)?;
                if !out.success() {
                    return Ok(false);
                }
                last = out.stdout.clone();
                let v: serde_json::Value =
                    serde_json::from_str(&out.stdout).unwrap_or(serde_json::Value::Null);
                // Any Ingress in the namespace carrying the auth-url + response-headers
                // annotations confirms the Member access class was wired.
                let ok = v["items"].as_array().is_some_and(|items| {
                    items.iter().any(|i| {
                        let ann = &i["metadata"]["annotations"];
                        ann["nginx.ingress.kubernetes.io/auth-url"]
                            .as_str()
                            .is_some_and(|u| u.contains("/api/v1/auth/ingress"))
                            && ann["nginx.ingress.kubernetes.io/auth-response-headers"].is_string()
                    })
                });
                Ok(ok)
            },
        )
        .with_context(|| format!("ingress missing nginx auth annotations:\n{last}"))
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

    fn upgrade(&mut self) -> Result<()> {
        let target = self
            .upgrade_target
            .take()
            .context("minikube backend is not in upgrade mode (RISE_E2E_UPGRADE_FROM unset)")?;
        self.image_tag = target;
        self.cli_image = format!("{}:{}", self.cli_repo, self.image_tag);

        // Helm does not upgrade CRDs on `helm upgrade`, so apply the in-repo CRDs
        // first — the same step an operator runs to pick up new CRD fields.
        report::step("apply in-repo CRDs", || {
            let mut apply = Command::new("kubectl");
            apply.args(["apply", "-f", &self.repo_path("helm/rise/crds")]);
            cli::run_checked(apply).context("kubectl apply CRDs")
        })?;

        // Upgrade from the old released chart to the in-repo chart on the new image.
        report::step("helm upgrade (to in-repo chart + target image)", || {
            let mut helm = Command::new("helm");
            helm.args(["upgrade", RELEASE, &self.repo_path("helm/rise")]);
            helm.args(self.helm_args());
            cli::run_checked(helm).context("helm upgrade (target)")
        })?;

        self.await_control_plane()
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

    fn dump_diagnostics(&self) {
        let mk = |args: &[&str]| {
            let mut c = Command::new("minikube");
            c.args(args);
            c
        };
        // A client-side timeout so a wedged apiserver can't hang a kubectl call
        // (cli::dump's wall-clock cap is the outer backstop for minikube/docker).
        let kc = |args: &[&str]| {
            let mut c = Command::new("kubectl");
            c.arg("--request-timeout=20s");
            c.args(args);
            c
        };
        cli::dump("minikube status", mk(&["status"]));
        cli::dump("pods (all namespaces)", kc(&["get", "pods", "-A"]));
        cli::dump(
            "recent events",
            kc(&["get", "events", "-A", "--sort-by=.lastTimestamp"]),
        );
        cli::dump(
            &format!("describe rise pods (ns {NAMESPACE})"),
            kc(&[
                "describe",
                "pods",
                "-n",
                NAMESPACE,
                "-l",
                &format!("app.kubernetes.io/instance={RELEASE}"),
            ]),
        );
        cli::dump(
            "rise server logs (last 200 lines)",
            kc(&[
                "logs",
                "-n",
                NAMESPACE,
                &format!("deploy/{SERVER_SVC}"),
                "--tail=200",
            ]),
        );
        // Metacontroller drives the reconcile that re-mints workload-identity
        // Secrets; its logs show whether the periodic resync actually fired (the
        // decisive signal when the identity token stops refreshing).
        cli::dump(
            "metacontroller operator logs (last 200 lines)",
            kc(&[
                "logs",
                "-n",
                NAMESPACE,
                &format!("statefulset/{RELEASE}-metacontroller-operator"),
                "--tail=200",
            ]),
        );

        // Deployed app workloads live in `rise-<project>` namespaces; the control
        // plane above won't show why an app pod failed. Describe + log each app
        // namespace's pods (the `-A` lists give status/events; this adds the app's
        // own logs, which is often the actual cause of a scenario failure), plus
        // the identity Secret's `last-refresh` annotation — whether it advances
        // tells re-mint-stalled (controller) apart from propagation-stalled (kubelet).
        if let Ok(out) = cli::run(kc(&["get", "ns", "-o", "name"])) {
            let app_nss = out.stdout.lines().filter_map(|l| {
                l.strip_prefix("namespace/")
                    .filter(|ns| ns.starts_with("rise-") && *ns != NAMESPACE)
            });
            for ns in app_nss {
                cli::dump(
                    &format!("describe pods ({ns})"),
                    kc(&["describe", "pods", "-n", ns]),
                );
                cli::dump(
                    &format!("identity secret refresh ({ns})"),
                    kc(&[
                        "get",
                        "secret",
                        "-n",
                        ns,
                        "-o",
                        r#"jsonpath={range .items[*]}{.metadata.name}{"  last-refresh="}{.metadata.annotations.rise\.dev/last-refresh}{"\n"}{end}"#,
                    ]),
                );
                if let Ok(pods) = cli::run(kc(&["get", "pods", "-n", ns, "-o", "name"])) {
                    for pod in pods.stdout.lines().filter(|l| !l.trim().is_empty()) {
                        cli::dump(
                            &format!("logs {ns}/{pod}"),
                            kc(&["logs", "-n", ns, pod, "--all-containers", "--tail=200"]),
                        );
                    }
                }
            }
        }

        cli::dump("minikube logs (tail)", mk(&["logs", "--length=80"]));
        if self.registry_mode == RegistryMode::JfrogVault {
            cli::dump("jfrog/vault compose ps", self.compose(&["ps"]));
            cli::dump(
                "jfrog/vault compose logs (tail)",
                self.compose(&["logs", "--no-color", "--tail=100"]),
            );
        }
    }
}
