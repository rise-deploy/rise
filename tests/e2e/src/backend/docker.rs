//! Docker-backend driver: a self-contained compose stack handling bring-up, CLI
//! extraction, Traefik reach, and teardown.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use super::{Backend, BackendKind, CliAuth, SampleApp};
use crate::cli::{self, CliOutput};
use crate::dex::DexEndpoint;
use crate::http::{self, HttpResponse};
use crate::report;
use crate::token;

// Matches config/docker.yaml + the docker-compose.standalone.local.yaml overlay.
const SECRET_B64: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
const PUBLIC_URL: &str = "http://rise.localhost:3000";
const RISE_URL: &str = "http://localhost:3000";
const TRAEFIK_URL: &str = "http://localhost";

pub struct DockerBackend {
    repo_root: PathBuf,
    image_repository: String,
    image_tag: String,
    ci_token: String,
    dex: DexEndpoint,
    /// Where the CLI binary is extracted to (set in `bring_up`).
    cli_bin: Option<PathBuf>,
    extract_container: String,
}

impl DockerBackend {
    pub fn new() -> Result<Self> {
        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .canonicalize()
            .context("resolve repo root from CARGO_MANIFEST_DIR")?;
        let image_repository = std::env::var("RISE_IMAGE_REPOSITORY")
            .unwrap_or_else(|_| "ghcr.io/rise-deploy/rise".to_string());
        let image_tag = std::env::var("RISE_IMAGE_TAG")
            .context("RISE_IMAGE_TAG must be set for the docker backend")?;
        let ci_token = token::mint_ci_token(SECRET_B64, PUBLIC_URL)?;
        let dex = DexEndpoint {
            token_url: "http://127.0.0.1:5556/dex/token".to_string(),
            client_id: "rise-backend".to_string(),
            client_secret: "rise-backend-secret".to_string(),
            issuer: "http://rise-dex:5556/dex".to_string(),
        };
        Ok(Self {
            repo_root,
            image_repository,
            image_tag,
            ci_token,
            dex,
            cli_bin: None,
            extract_container: format!("rise-e2e-cli-{}", std::process::id()),
        })
    }

    fn image(&self) -> String {
        format!("{}:{}", self.image_repository, self.image_tag)
    }

    /// First app container name for a project (`rise_<project>…`), polled until present.
    fn app_container(&self, project: &str) -> Result<String> {
        let mut name = String::new();
        http::poll(
            Duration::from_secs(60),
            Duration::from_secs(2),
            &format!("app container for {project}"),
            || {
                let mut c = Command::new("docker");
                c.args([
                    "ps",
                    "--filter",
                    &format!("name=rise_{project}"),
                    "--format",
                    "{{.Names}}",
                ]);
                let out = cli::run(c)?;
                if let Some(first) = out.stdout.lines().find(|l| !l.trim().is_empty()) {
                    name = first.trim().to_string();
                    return Ok(true);
                }
                Ok(false)
            },
        )?;
        Ok(name)
    }

    /// `docker compose` against the standalone + local overlays, from the repo
    /// root, with the image env the compose files reference.
    fn compose(&self) -> Command {
        let mut c = Command::new("docker");
        c.current_dir(&self.repo_root)
            .env("RISE_IMAGE_REPOSITORY", &self.image_repository)
            .env("RISE_IMAGE_TAG", &self.image_tag)
            .args([
                "compose",
                "-f",
                "docker-compose.standalone.yaml",
                "-f",
                "docker-compose.standalone.local.yaml",
            ]);
        c
    }
}

impl Backend for DockerBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Docker
    }

    fn bring_up(&mut self) -> Result<()> {
        // Fresh volumes (a clean DB exercises org/controller-class bootstrap).
        report::step("docker compose down -v (clean slate)", || {
            let mut down = self.compose();
            down.args(["down", "-v"]);
            let _ = cli::run(down);
            Ok(())
        })?;

        report::step("docker compose up -d", || {
            let mut up = self.compose();
            up.args(["up", "-d"]);
            cli::run_checked(up).context("docker compose up")
        })?;

        report::step_value("rise /health", || {
            http::poll(
                Duration::from_secs(120),
                Duration::from_secs(2),
                "rise backend /health",
                || {
                    Ok(http::get(&format!("{RISE_URL}/health"), None)
                        .map(|r| r.status == 200)
                        .unwrap_or(false))
                },
            )?;
            Ok("200")
        })?;

        // Extract the CLI from the image for an exact version match.
        report::step("extract rise CLI from image", || {
            let tmp = self
                .repo_root
                .join("target")
                .join(format!("e2e-cli-{}", std::process::id()));
            std::fs::create_dir_all(&tmp).context("create CLI extract dir")?;
            let bin = tmp.join("rise");

            let mut create = Command::new("docker");
            create.args(["create", "--name", &self.extract_container, &self.image()]);
            cli::run_checked(create).context("docker create (CLI extract)")?;

            let mut cp = Command::new("docker");
            cp.arg("cp")
                .arg(format!("{}:/usr/local/bin/rise", self.extract_container));
            cp.arg(&bin);
            cli::run_checked(cp).context("docker cp rise binary")?;

            let mut rm = Command::new("docker");
            rm.args(["rm", "-f", &self.extract_container]);
            let _ = cli::run(rm);

            self.cli_bin = Some(bin);
            Ok(())
        })
    }

    fn tear_down(&mut self) {
        // Remove controller-managed app containers, then the stack + volumes.
        if let Ok(out) = cli::run({
            let mut c = Command::new("docker");
            c.args(["ps", "-aq", "--filter", "label=rise.dev/managed-by=rise"]);
            c
        }) {
            for id in out.stdout.split_whitespace() {
                let mut rm = Command::new("docker");
                rm.args(["rm", "-f", id]);
                let _ = cli::run(rm);
            }
        }
        let mut down = self.compose();
        down.args(["down", "-v"]);
        let _ = cli::run(down);
        let mut rm = Command::new("docker");
        rm.args(["rm", "-f", &self.extract_container]);
        let _ = cli::run(rm);
    }

    fn rise_cli(&self, args: &[&str], auth: Option<CliAuth<'_>>) -> Result<CliOutput> {
        let bin = self
            .cli_bin
            .as_ref()
            .context("CLI binary not extracted (bring_up not run?)")?;
        let mut c = Command::new(bin);
        c.current_dir(&self.repo_root)
            .env("RISE_URL", RISE_URL)
            .env_remove("RISE_IDENTITY");
        match &auth {
            Some(a) => {
                c.env("RISE_TOKEN", a.token);
                if let Some(id) = a.identity {
                    c.env("RISE_IDENTITY", id);
                }
            }
            None => {
                c.env("RISE_TOKEN", &self.ci_token);
            }
        }
        c.args(args);
        cli::run(c)
    }

    fn reach_app(&self, project: &str, path: &str) -> Result<Option<HttpResponse>> {
        let url = format!("{TRAEFIK_URL}{path}");
        let host = self.app_host(project);
        // The controller may not have wired the Traefik route yet → retry on 404.
        let mut resp = None;
        http::poll(
            Duration::from_secs(60),
            Duration::from_secs(2),
            &format!("Traefik route for {host}"),
            || {
                let r = http::get(&url, Some(&host))?;
                // Traefik can transiently return 5xx (502/503) while wiring a
                // freshly-deployed route — keep polling on those too, not just 404.
                let ready = r.status != 404 && r.status < 500;
                resp = Some(r);
                Ok(ready)
            },
        )?;
        Ok(resp)
    }

    fn poll_app(
        &self,
        project: &str,
        path: &str,
        duration: Duration,
        interval: Duration,
        check: &mut dyn FnMut(&str) -> bool,
    ) -> Result<bool> {
        // Traefik routes by Host; no port-forward to hold. Reuse `ingress_get` so
        // there's a single definition of "reach this app through the ingress".
        let start = Instant::now();
        while start.elapsed() < duration {
            if let Ok(r) = self.ingress_get(project, path, true, None) {
                if r.status == 200 && check(&r.body) {
                    return Ok(true);
                }
            }
            std::thread::sleep(interval);
        }
        Ok(false)
    }

    fn wait_workload_removed(&self, project: &str) -> Result<()> {
        // Provided for parity: no Docker scenario exercises this yet (log
        // retention runs on minikube), but the trait requires a real per-runtime
        // check rather than a status-string default. The app's container(s) must
        // be gone — proving a post-stop logs query is served by the store, not a
        // live container. `name=` is a substring match, which is the app's own
        // `rise_<project>…` container(s) in this single-stack test.
        http::poll(
            Duration::from_secs(120),
            Duration::from_secs(2),
            &format!("app container for {project} to be removed"),
            || {
                let mut c = Command::new("docker");
                c.args([
                    "ps",
                    "--filter",
                    &format!("name=rise_{project}"),
                    "--format",
                    "{{.Names}}",
                ]);
                let out = cli::run(c)?;
                Ok(out.success() && out.stdout.trim().is_empty())
            },
        )
    }

    fn dump_diagnostics(&self) {
        let mut ps = self.compose();
        ps.args(["ps", "-a"]);
        cli::dump("compose ps", ps);
        let mut logs = self.compose();
        logs.args(["logs", "--no-color", "--tail=200"]);
        cli::dump("compose logs (tail)", logs);
    }

    fn dex(&self) -> Option<&DexEndpoint> {
        Some(&self.dex)
    }

    fn sample_app(&self) -> SampleApp {
        // Docker runs as root, so whoami on :80 is fine and emits a stable body.
        SampleApp {
            image: "traefik/whoami",
            http_port: "80",
            body_marker: Some("Hostname:"),
        }
    }

    fn api_base(&self) -> &str {
        RISE_URL
    }

    fn ci_bearer(&self) -> &str {
        &self.ci_token
    }

    fn supports_source_build(&self) -> bool {
        // The docker CLI runs on the host, which has a docker daemon.
        true
    }

    fn traefik_base(&self) -> Option<&str> {
        Some(TRAEFIK_URL)
    }

    fn app_container_labels(&self, project: &str) -> Result<String> {
        let container = self.app_container(project)?;
        let mut c = Command::new("docker");
        c.args(["inspect", &container, "--format", "{{json .Config.Labels}}"]);
        Ok(cli::run_checked(c)?.stdout)
    }

    fn traefik_api(&self, path: &str) -> Result<HttpResponse> {
        // Reach the Traefik API from inside the compose network via the dex container.
        let mut c = self.compose();
        c.args([
            "exec",
            "-T",
            "dex",
            "wget",
            "-q",
            "-O",
            "-",
            &format!("http://rise-traefik:8080{path}"),
        ]);
        let out = cli::run(c)?;
        Ok(HttpResponse {
            status: if out.success() { 200 } else { 0 },
            body: out.stdout,
        })
    }

    fn app_host(&self, project: &str) -> String {
        format!("{project}.rise.localhost")
    }

    fn ingress_get(
        &self,
        project: &str,
        path: &str,
        follow_redirects: bool,
        cookie: Option<&str>,
    ) -> Result<http::Resp> {
        let host = self.app_host(project);
        http::request(
            &format!("{TRAEFIK_URL}{path}"),
            Some(&host),
            cookie,
            follow_redirects,
        )
    }

    fn assert_ingress_auth_configured(&self, project: &str) -> Result<()> {
        // Traefik forwardAuth: the controller stamps forwardauth + router-middleware
        // labels on the app container (it can lag the Healthy mark, so poll).
        let mut labels = String::new();
        http::poll(
            Duration::from_secs(60),
            Duration::from_secs(2),
            &format!("forwardAuth labels on {project}"),
            || {
                labels = self.app_container_labels(project)?;
                Ok(labels.contains("forwardauth.address")
                    && labels.contains(".routers.")
                    && labels.contains(".middlewares"))
            },
        )
        .with_context(|| format!("private app container missing forwardAuth labels:\n{labels}"))
    }

    fn app_container_envs(&self, project: &str) -> Result<Vec<String>> {
        let mut list = Command::new("docker");
        list.args([
            "ps",
            "--filter",
            &format!("name=rise_{project}"),
            "--format",
            "{{.Names}}",
        ]);
        let names = cli::run(list)?;
        let mut envs = Vec::new();
        for name in names.stdout.lines().filter(|l| !l.trim().is_empty()) {
            let mut c = Command::new("docker");
            c.args(["inspect", name.trim(), "--format", "{{json .Config.Env}}"]);
            envs.push(cli::run_checked(c)?.stdout);
        }
        Ok(envs)
    }

    fn prepare_workload_identity(&self) -> Result<()> {
        // Recreate the rise backend with the identity overlay (scoped registry_url +
        // short token TTL) so the workload-identity deploy/build + refresh apply.
        let mut up = self.compose();
        up.args([
            "-f",
            "docker-compose.standalone.identity.yaml",
            "up",
            "-d",
            "--no-deps",
            "--force-recreate",
            "rise",
        ]);
        cli::run_checked(up).context("recreate rise with identity overlay")?;
        http::poll(
            Duration::from_secs(120),
            Duration::from_secs(2),
            "rise /health after identity-overlay switch",
            || {
                Ok(http::get(&format!("{RISE_URL}/health"), None)
                    .map(|r| r.status == 200)
                    .unwrap_or(false))
            },
        )
    }
}
