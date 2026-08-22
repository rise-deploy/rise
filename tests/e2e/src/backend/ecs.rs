//! The Amazon ECS backend driver.
//!
//! Unlike the Docker and Minikube drivers, which provision a runtime and run the
//! Rise control plane locally, this one runs **the whole stack inside the ECS
//! cluster** — Traefik, Postgres, Dex and Rise, each as its own service (see
//! `tests/e2e/aws/ecs-stack.sh`). That is what lets Traefik reach Rise for the
//! forwardAuth subrequest, and therefore what lets the ingress-auth scenarios
//! run here rather than being declared skips.
//!
//! Everything the harness needs to talk to is reached through Traefik's single
//! public IP using `nip.io` hostnames, so the driver holds no port-forwards and
//! needs no AWS SDK: AWS work is delegated to the stack script and the `aws`
//! CLI, exactly as the other drivers delegate to `docker` and `kubectl`.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use super::{Backend, BackendKind, CliAuth, SampleApp};
use crate::cli::{self, CliOutput};
use crate::dex::DexEndpoint;
use crate::http::{self, HttpResponse};
use crate::{report, token};

/// Must match `config/ecs.yaml`'s default and the stack script's `JWT_SECRET`.
const SECRET_B64: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

pub struct EcsBackend {
    repo_root: PathBuf,
    image_repository: String,
    image_tag: String,
    ci_token: String,
    /// Populated by `bring_up` from the stack script's state file.
    stack: Option<StackState>,
    dex: Option<DexEndpoint>,
    cli_bin: Option<PathBuf>,
    extract_container: String,
}

/// What the AWS-side stack script reports once it is up.
struct StackState {
    /// Traefik's public IP — the single address everything is reached through.
    traefik_ip: String,
    /// `<traefik-ip>.nip.io`, the ingress domain apps are routed under.
    domain: String,
    /// Rise's URL through Traefik. Doubles as `public_url`, so the CI bearer's
    /// `iss` matches what the server validates against.
    rise_url: String,
    traefik_api: String,
    cluster: String,
    region: String,
}

impl EcsBackend {
    pub fn new() -> Result<Self> {
        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .canonicalize()
            .context("resolve repo root")?;
        let image_repository = std::env::var("RISE_IMAGE_REPOSITORY")
            .unwrap_or_else(|_| "ghcr.io/rise-deploy/rise".to_string());
        let image_tag = std::env::var("RISE_IMAGE_TAG")
            .context("RISE_IMAGE_TAG must be set for the ecs backend (ECS pulls it from GHCR)")?;

        // Minted locally with the same HS256 secret the server is configured
        // with, exactly as the Docker driver does. `iss` must equal the server's
        // public_url, which the stack script derives from Traefik's IP — so the
        // real token is minted in `bring_up`, once that IP is known.
        Ok(Self {
            repo_root,
            image_repository,
            image_tag,
            ci_token: String::new(),
            stack: None,
            dex: None,
            cli_bin: None,
            extract_container: format!("rise-e2e-ecs-cli-{}", std::process::id()),
        })
    }

    fn stack(&self) -> &StackState {
        self.stack
            .as_ref()
            .expect("bring_up must run before the stack is used")
    }

    fn image(&self) -> String {
        format!("{}:{}", self.image_repository, self.image_tag)
    }

    fn stack_script(&self, arg: &str) -> Command {
        let mut c = Command::new("bash");
        c.current_dir(&self.repo_root)
            .env("RISE_IMAGE_REPOSITORY", &self.image_repository)
            .env("RISE_IMAGE_TAG", &self.image_tag)
            .arg("tests/e2e/aws/ecs-stack.sh")
            .arg(arg);
        c
    }

    /// Read the `KEY="value"` exports the stack script writes.
    fn read_stack_state(&self) -> Result<StackState> {
        let path = std::env::var("RISE_E2E_STATE_FILE")
            .unwrap_or_else(|_| "/tmp/rise-e2e-stack.env".to_string());
        let body = std::fs::read_to_string(&path)
            .with_context(|| format!("read stack state file {path}"))?;
        let get = |key: &str| -> Result<String> {
            body.lines()
                .find_map(|line| {
                    let line = line.trim().strip_prefix("export ")?;
                    let (k, v) = line.split_once('=')?;
                    (k == key).then(|| v.trim_matches('"').to_string())
                })
                .with_context(|| format!("{key} missing from {path}"))
        };
        Ok(StackState {
            traefik_ip: get("RISE_E2E_TRAEFIK_IP")?,
            domain: get("RISE_E2E_DOMAIN")?,
            rise_url: get("RISE_E2E_RISE_URL")?,
            traefik_api: get("RISE_E2E_TRAEFIK_API")?,
            cluster: get("RISE_E2E_CLUSTER")?,
            region: get("RISE_E2E_REGION")?,
        })
    }

    /// Extract the `rise` CLI from the image under test, so the harness drives a
    /// CLI that exactly matches the running server.
    fn extract_cli(&mut self) -> Result<()> {
        let tmp = self
            .repo_root
            .join("target")
            .join(format!("e2e-ecs-cli-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).context("create CLI extract dir")?;
        let bin = tmp.join("rise");

        let mut rm = Command::new("docker");
        rm.args(["rm", "-f", &self.extract_container]);
        let _ = cli::run(rm);

        let mut create = Command::new("docker");
        create.args(["create", "--name", &self.extract_container, &self.image()]);
        cli::run_checked(create).context("docker create (CLI extract)")?;

        let mut cp = Command::new("docker");
        cp.args([
            "cp",
            &format!("{}:/usr/local/bin/rise", self.extract_container),
            bin.to_str().context("CLI path is not UTF-8")?,
        ]);
        cli::run_checked(cp).context("docker cp rise binary")?;

        let mut rm2 = Command::new("docker");
        rm2.args(["rm", "-f", &self.extract_container]);
        let _ = cli::run(rm2);

        self.cli_bin = Some(bin);
        Ok(())
    }

    /// Run an `aws` CLI command against the stack's cluster, returning stdout.
    fn aws(&self, args: &[&str]) -> Result<String> {
        let mut c = Command::new("aws");
        c.env("AWS_REGION", &self.stack().region).args(args);
        let out = cli::run(c)?;
        if !out.success() {
            anyhow::bail!("aws {:?} failed: {}", args, out.stderr);
        }
        Ok(out.stdout)
    }

    /// The ECS services Rise created for a project, by its bookkeeping tag.
    fn project_services(&self, project: &str) -> Result<Vec<String>> {
        let raw = self.aws(&[
            "ecs",
            "list-services",
            "--cluster",
            &self.stack().cluster,
            "--query",
            "serviceArns[]",
            "--output",
            "json",
        ])?;
        let arns: Vec<String> = serde_json::from_str(&raw).unwrap_or_default();
        if arns.is_empty() {
            return Ok(Vec::new());
        }

        let mut matching = Vec::new();
        for chunk in arns.chunks(10) {
            let mut args: Vec<String> = vec![
                "ecs".into(),
                "describe-services".into(),
                "--cluster".into(),
                self.stack().cluster.clone(),
                "--include".into(),
                "TAGS".into(),
                "--services".into(),
            ];
            args.extend(chunk.iter().cloned());
            args.extend(["--output".to_string(), "json".to_string()]);
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            let raw = self.aws(&refs)?;
            let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
            for svc in parsed["services"].as_array().unwrap_or(&vec![]) {
                let is_project = svc["tags"].as_array().is_some_and(|tags| {
                    tags.iter().any(|t| {
                        t["key"] == "rise.dev/project" && t["value"] == serde_json::json!(project)
                    })
                });
                // A service being torn down still appears here for a while.
                let live = svc["status"] != serde_json::json!("INACTIVE");
                if is_project && live {
                    if let Some(name) = svc["serviceName"].as_str() {
                        matching.push(name.to_string());
                    }
                }
            }
        }
        Ok(matching)
    }

    fn wait_rise_healthy(&self, step: &str) -> Result<()> {
        let url = format!("{}/health", self.stack().rise_url);
        report::step_value(step, || {
            http::poll(
                Duration::from_secs(180),
                Duration::from_secs(5),
                "rise /health through Traefik",
                || {
                    Ok(http::get(&url, None)
                        .map(|r| r.status == 200)
                        .unwrap_or(false))
                },
            )?;
            Ok("200".to_string())
        })?;
        Ok(())
    }
}

impl Backend for EcsBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Ecs
    }

    fn bring_up(&mut self) -> Result<()> {
        report::step("aws stack down (clean slate)", || {
            let _ = cli::run(self.stack_script("down"));
            Ok(())
        })?;
        report::step("aws stack up (traefik, postgres, dex, rise)", || {
            cli::run_checked(self.stack_script("up"))
        })?;

        let stack = self.read_stack_state()?;
        report::note(&format!(
            "traefik={} domain={}",
            stack.traefik_ip, stack.domain
        ));

        // The CI bearer's `iss` must equal the server's public_url, which is only
        // known once Traefik has an IP — hence minting here rather than in `new`.
        self.ci_token = token::mint_ci_token(SECRET_B64, &stack.rise_url)?;
        self.dex = Some(DexEndpoint {
            token_url: format!("http://dex.{}/dex/token", stack.domain),
            client_id: "rise-backend".to_string(),
            client_secret: "rise-backend-secret".to_string(),
            // Dex stamps the Cloud Map address in `iss`; Rise validates against
            // exactly that, and fetches JWKS over the cluster's private DNS.
            issuer: "http://dex.rise-e2e.local:5556/dex".to_string(),
        });
        self.stack = Some(stack);

        self.wait_rise_healthy("rise /health")?;
        report::step("extract rise CLI from image", || self.extract_cli())
    }

    fn tear_down(&mut self) {
        if std::env::var("KEEP").is_ok() {
            report::note("KEEP set — leaving the AWS stack up (remember to tear it down)");
            return;
        }
        let _ = cli::run(self.stack_script("down"));
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
            .env("RISE_URL", &self.stack().rise_url)
            .env_remove("RISE_IDENTITY");
        match &auth {
            Some(a) => {
                c.env("RISE_TOKEN", a.token);
                if let Some(identity) = a.identity {
                    c.env("RISE_IDENTITY", identity);
                }
            }
            None => {
                c.env("RISE_TOKEN", &self.ci_token);
            }
        }
        c.args(args);
        cli::run(c)
    }

    fn cli_visible_path(&self, rel: &str) -> String {
        self.repo_root.join(rel).to_string_lossy().to_string()
    }

    fn reach_app(&self, project: &str, path: &str) -> Result<Option<HttpResponse>> {
        let url = format!("http://{}{}", self.stack().traefik_ip, path);
        let host = self.app_host(project);
        let mut resp = None;
        // Fargate task start plus Traefik's 5s ECS-provider poll means a freshly
        // deployed app takes appreciably longer to route than on Docker.
        http::poll(
            Duration::from_secs(180),
            Duration::from_secs(3),
            &format!("Traefik route for {host}"),
            || {
                let r = http::get(&url, Some(&host))?;
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

    fn dex(&self) -> Option<&DexEndpoint> {
        self.dex.as_ref()
    }

    fn sample_app(&self) -> SampleApp {
        // Fargate runs the image's own user and imposes no non-root constraint,
        // so whoami on :80 works as it does on Docker.
        SampleApp {
            image: "traefik/whoami:v1.10",
            http_port: "80",
            body_marker: Some("Hostname:"),
        }
    }

    fn api_base(&self) -> &str {
        &self.stack().rise_url
    }

    fn ci_bearer(&self) -> &str {
        &self.ci_token
    }

    fn traefik_base(&self) -> Option<&str> {
        Some(&self.stack().traefik_ip)
    }

    fn traefik_api(&self, path: &str) -> Result<HttpResponse> {
        http::get(&format!("{}{}", self.stack().traefik_api, path), None)
    }

    fn app_host(&self, project: &str) -> String {
        format!("{}.{}", project, self.stack().domain)
    }

    fn ingress_get(
        &self,
        project: &str,
        path: &str,
        follow_redirects: bool,
        cookie: Option<&str>,
    ) -> Result<crate::http::Resp> {
        let url = format!("http://{}{}", self.stack().traefik_ip, path);
        let host = self.app_host(project);
        http::request(&url, Some(&host), cookie, follow_redirects)
    }

    /// Assert the ECS task definition carries the Traefik forwardAuth labels.
    ///
    /// The ECS analogue of inspecting container labels on Docker: the ECS
    /// provider reads routing configuration from `dockerLabels` on the container
    /// definition, so that is where the evidence lives.
    fn assert_ingress_auth_configured(&self, project: &str) -> Result<()> {
        let mut last = String::new();
        for _ in 0..30 {
            if let Ok(services) = self.project_services(project) {
                for svc in &services {
                    let raw = self.aws(&[
                        "ecs",
                        "describe-services",
                        "--cluster",
                        &self.stack().cluster,
                        "--services",
                        svc,
                        "--query",
                        "services[0].taskDefinition",
                        "--output",
                        "text",
                    ])?;
                    let td = raw.trim();
                    if td.is_empty() {
                        continue;
                    }
                    let labels = self.aws(&[
                        "ecs",
                        "describe-task-definition",
                        "--task-definition",
                        td,
                        "--query",
                        "taskDefinition.containerDefinitions[0].dockerLabels",
                        "--output",
                        "json",
                    ])?;
                    last = labels.clone();
                    if labels.contains("forwardauth.address")
                        && labels.contains(".routers.")
                        && labels.contains(".middlewares")
                    {
                        return Ok(());
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(3));
        }
        anyhow::bail!(
            "no forwardAuth middleware labels found on {project}'s task definition; last saw: {last}"
        )
    }

    /// Environment of each running app task, for asserting which revision is live
    /// after a cutover.
    fn app_container_envs(&self, project: &str) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for svc in self.project_services(project)? {
            let td = self
                .aws(&[
                    "ecs",
                    "describe-services",
                    "--cluster",
                    &self.stack().cluster,
                    "--services",
                    &svc,
                    "--query",
                    "services[0].taskDefinition",
                    "--output",
                    "text",
                ])?
                .trim()
                .to_string();
            if td.is_empty() {
                continue;
            }
            out.push(self.aws(&[
                "ecs",
                "describe-task-definition",
                "--task-definition",
                &td,
                "--query",
                "taskDefinition.containerDefinitions[0].environment",
                "--output",
                "json",
            ])?);
        }
        Ok(out)
    }

    fn wait_workload_removed(&self, project: &str) -> Result<()> {
        http::poll(
            Duration::from_secs(300),
            Duration::from_secs(5),
            &format!("ECS services for '{project}' to be removed"),
            || {
                Ok(self
                    .project_services(project)
                    .map(|s| s.is_empty())
                    .unwrap_or(false))
            },
        )
    }

    fn dump_diagnostics(&self) {
        let Some(stack) = self.stack.as_ref() else {
            return;
        };
        let mut services = Command::new("aws");
        services.env("AWS_REGION", &stack.region).args([
            "ecs",
            "describe-services",
            "--cluster",
            &stack.cluster,
            "--services",
            "rise-e2e-rise",
            "rise-e2e-traefik",
            "--query",
            "services[].{name:serviceName,running:runningCount,events:events[0:5].message}",
            "--output",
            "json",
        ]);
        cli::dump("ecs control-plane services", services);

        let mut tasks = Command::new("aws");
        tasks.env("AWS_REGION", &stack.region).args([
            "ecs",
            "list-tasks",
            "--cluster",
            &stack.cluster,
            "--output",
            "json",
        ]);
        cli::dump("ecs tasks", tasks);

        let mut routers = Command::new("curl");
        routers.args([
            "-s",
            "--max-time",
            "10",
            &format!("{}/api/http/routers", stack.traefik_api),
        ]);
        cli::dump("traefik routers", routers);
    }
}
