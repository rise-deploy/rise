//! The Amazon ECS backend driver.
//!
//! Like the other two drivers, this one owns its environment's lifecycle — but
//! it owns only half of it. The AWS side is split:
//!
//! - **`tests/e2e/aws/bootstrap`** is applied once, by hand, and never touched
//!   here: VPC, cluster, Cloud Map, Traefik, Dex, a Route 53 zone, and all the
//!   IAM. It is separate because CI runs under a role with no IAM-write.
//! - **`tests/e2e/aws/run`** is applied and destroyed around every suite:
//!   Postgres and the Rise control plane, so each run starts on a fresh database
//!   and the image under test.
//!
//! The whole Rise stack runs *inside* the cluster, which is what lets Traefik
//! reach Rise for the forwardAuth subrequest and therefore what lets the
//! ingress-auth scenarios run here rather than be declared skips.
//!
//! Two things this driver must do that the others do not:
//!
//! **Sweep before it starts.** Deployments the suite creates are ECS services
//! Rise owns, not Terraform. Destroying the per-run stack removes Rise *and its
//! database*, so the next run's Rise cannot collect them — the reconciler only
//! visits services whose project it can see. Left alone they accumulate until
//! the account's Fargate quota stops the suite mid-way. The sweep runs at
//! bring-up as well as teardown, because a crashed run never reaches teardown.
//!
//! **Point DNS at Traefik.** Fargate tasks cannot hold an Elastic IP, so
//! Traefik's address changes whenever its task is replaced. The zone makes the
//! domain stable instead: resolve the current address, UPSERT the records, and
//! everything downstream — `PUBLIC_URL`, the bearer's `iss`, project hostnames —
//! stops depending on the address at all.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use super::{Backend, BackendKind, CliAuth, SampleApp};
use crate::cli::{self, CliOutput};
use crate::dex::DexEndpoint;
use crate::http::{self, HttpResponse};
use crate::{report, token};

/// Tag every Rise-created resource carries. The sweep keys on it.
const MANAGED_BY_TAG: &str = "rise.dev/managed-by";

pub struct EcsBackend {
    repo_root: PathBuf,
    image_repository: String,
    image_tag: String,
    /// Environment name — the bootstrap's `name`, and the SSM path its outputs
    /// are published under.
    env_name: String,
    region: String,
    /// Minted fresh per run rather than taken from the shipped default, which is
    /// a constant in this repository: this environment is persistent and
    /// publicly addressed, so a well-known key would let anyone able to reach
    /// the API mint an admin bearer offline.
    jwt_secret_b64: String,
    ci_token: String,
    env: Option<BootstrapEnv>,
    stack: Option<StackState>,
    dex: Option<DexEndpoint>,
    cli_bin: Option<PathBuf>,
    extract_container: String,
    /// The address authorized on the edge group for this run, revoked at
    /// teardown.
    authorized_cidr: Option<String>,
}

/// The persistent environment, read from the SSM parameter the bootstrap writes.
struct BootstrapEnv {
    cluster_name: String,
    edge_security_group_id: String,
    traefik_service_name: String,
    dex_issuer: String,
    ecr_repo_prefix: String,
    dns_zone_id: String,
    dns_zone_name: String,
    state_bucket: String,
}

/// What this run resolved or created.
struct StackState {
    /// Traefik's current public address. Apps are reached here directly with an
    /// explicit `Host` header, so this never needs to resolve in DNS.
    traefik_ip: String,
    /// The stable domain. Only the `rise` CLI needs it to resolve, since it
    /// takes a URL and has no Host-header override.
    domain: String,
    rise_url: String,
    traefik_api: String,
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
        let env_name = std::env::var("RISE_E2E_ENV").unwrap_or_else(|_| "rise-e2e".to_string());
        let region = std::env::var("AWS_REGION")
            .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
            .context("AWS_REGION must be set for the ecs backend")?;

        Ok(Self {
            repo_root,
            image_repository,
            image_tag,
            env_name,
            region,
            jwt_secret_b64: random_secret_b64()?,
            ci_token: String::new(),
            env: None,
            stack: None,
            dex: None,
            cli_bin: None,
            extract_container: format!("rise-e2e-ecs-cli-{}", std::process::id()),
            authorized_cidr: None,
        })
    }

    fn env(&self) -> &BootstrapEnv {
        self.env
            .as_ref()
            .expect("bring_up must run before the environment is used")
    }

    fn stack(&self) -> &StackState {
        self.stack
            .as_ref()
            .expect("bring_up must run before the stack is used")
    }

    fn image(&self) -> String {
        format!("{}:{}", self.image_repository, self.image_tag)
    }

    fn run_dir(&self) -> PathBuf {
        self.repo_root
            .join("tests")
            .join("e2e")
            .join("aws")
            .join("run")
    }

    fn aws(&self, args: &[&str]) -> Result<String> {
        let mut c = Command::new("aws");
        c.env("AWS_REGION", &self.region).args(args);
        let out = cli::run(c)?;
        if !out.success() {
            anyhow::bail!("aws {:?} failed: {}", args, out.stderr);
        }
        Ok(out.stdout)
    }

    fn terraform(&self, args: &[&str]) -> Command {
        let mut c = Command::new("terraform");
        c.current_dir(self.run_dir())
            .env("AWS_REGION", &self.region)
            .env("TF_IN_AUTOMATION", "1")
            .args(args);
        c
    }

    /// Read the environment description the bootstrap publishes.
    fn read_bootstrap_env(&self) -> Result<BootstrapEnv> {
        let raw = self
            .aws(&[
                "ssm",
                "get-parameter",
                "--name",
                &format!("/{}/e2e/bootstrap", self.env_name),
                "--query",
                "Parameter.Value",
                "--output",
                "text",
            ])
            .with_context(|| {
                format!(
                    "read /{}/e2e/bootstrap. Has tests/e2e/aws/bootstrap been applied \
                     to this account and region?",
                    self.env_name
                )
            })?;

        let v: serde_json::Value =
            serde_json::from_str(raw.trim()).context("parse the bootstrap SSM parameter")?;
        let get = |key: &str| -> Result<String> {
            v.get(key)
                .and_then(|x| x.as_str())
                .map(str::to_string)
                .with_context(|| format!("{key} missing from the bootstrap parameter"))
        };

        Ok(BootstrapEnv {
            cluster_name: get("cluster_name")?,
            edge_security_group_id: get("edge_security_group_id")?,
            traefik_service_name: get("traefik_service_name")?,
            dex_issuer: get("dex_issuer")?,
            ecr_repo_prefix: get("ecr_repo_prefix")?,
            dns_zone_id: get("dns_zone_id")?,
            dns_zone_name: get("dns_zone_name")?,
            state_bucket: get("state_bucket")?,
        })
    }

    /// The ECS services Rise created for a project, by its bookkeeping tag.
    fn project_services(&self, project: &str) -> Result<Vec<String>> {
        let cluster = &self.env().cluster_name;
        let arns: Vec<String> = serde_json::from_str(&self.aws(&[
            "ecs",
            "list-services",
            "--cluster",
            cluster,
            "--query",
            "serviceArns[]",
            "--output",
            "json",
        ])?)
        .unwrap_or_default();
        if arns.is_empty() {
            return Ok(Vec::new());
        }

        let mut matching = Vec::new();
        for chunk in arns.chunks(10) {
            let mut args: Vec<String> = vec![
                "ecs".into(),
                "describe-services".into(),
                "--cluster".into(),
                cluster.clone(),
                "--include".into(),
                "TAGS".into(),
                "--services".into(),
            ];
            args.extend(chunk.iter().cloned());
            args.extend(["--output".to_string(), "json".to_string()]);
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            let parsed: serde_json::Value =
                serde_json::from_str(&self.aws(&refs)?).unwrap_or_default();
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

    /// The public address of the single task behind `service`.
    fn service_public_ip(&self, service: &str) -> Result<String> {
        let cluster = &self.env().cluster_name;
        let task = self
            .aws(&[
                "ecs",
                "list-tasks",
                "--cluster",
                cluster,
                "--service-name",
                service,
                "--query",
                "taskArns[0]",
                "--output",
                "text",
            ])?
            .trim()
            .to_string();
        anyhow::ensure!(
            !task.is_empty() && task != "None",
            "no running task for {service}. The persistent environment may be down; \
             check the service in the ECS console."
        );

        let eni = self
            .aws(&[
                "ecs",
                "describe-tasks",
                "--cluster",
                cluster,
                "--tasks",
                &task,
                "--query",
                "tasks[0].attachments[0].details[?name=='networkInterfaceId'].value | [0]",
                "--output",
                "text",
            ])?
            .trim()
            .to_string();

        let ip = self
            .aws(&[
                "ec2",
                "describe-network-interfaces",
                "--network-interface-ids",
                &eni,
                "--query",
                "NetworkInterfaces[0].Association.PublicIp",
                "--output",
                "text",
            ])?
            .trim()
            .to_string();
        anyhow::ensure!(
            !ip.is_empty() && ip != "None",
            "{service}'s task has no public address"
        );
        Ok(ip)
    }

    /// Point the apex and wildcard at Traefik's current address.
    ///
    /// The wildcard is not optional: projects are served at `<project>.<domain>`,
    /// and groups and environments add another label.
    fn upsert_dns(&self, ip: &str) -> Result<()> {
        let env = self.env();
        let records: Vec<serde_json::Value> = ["", "*."]
            .iter()
            .map(|prefix| {
                serde_json::json!({
                    "Action": "UPSERT",
                    "ResourceRecordSet": {
                        "Name": format!("{prefix}{}", env.dns_zone_name),
                        "Type": "A",
                        "TTL": 60,
                        "ResourceRecords": [{ "Value": ip }],
                    }
                })
            })
            .collect();
        let changes = serde_json::json!({
            "Comment": "rise e2e run",
            "Changes": records,
        });

        let path = std::env::temp_dir().join(format!("rise-e2e-dns-{}.json", std::process::id()));
        std::fs::write(&path, serde_json::to_vec(&changes)?)
            .context("write the DNS change batch")?;

        self.aws(&[
            "route53",
            "change-resource-record-sets",
            "--hosted-zone-id",
            &env.dns_zone_id,
            "--change-batch",
            &format!("file://{}", path.display()),
        ])?;
        let _ = std::fs::remove_file(&path);
        Ok(())
    }

    /// Open the edge for this run only. The group is closed between runs, which
    /// is what keeps a persistent, publicly addressed control plane defensible.
    fn authorize_edge(&mut self) -> Result<()> {
        let ip = http::get("https://checkip.amazonaws.com", None)
            .context("determine this machine's public address")?
            .body
            .trim()
            .to_string();
        let cidr = format!("{ip}/32");

        // Idempotent by intent: a rule left behind by a crashed run makes this a
        // duplicate, which is not an error worth failing the suite over.
        let _ = self.aws(&[
            "ec2",
            "authorize-security-group-ingress",
            "--group-id",
            &self.env().edge_security_group_id,
            "--protocol",
            "tcp",
            "--port",
            "80",
            "--cidr",
            &cidr,
        ]);
        self.authorized_cidr = Some(cidr);
        Ok(())
    }

    fn revoke_edge(&self) {
        let Some(cidr) = self.authorized_cidr.as_deref() else {
            return;
        };
        let _ = self.aws(&[
            "ec2",
            "revoke-security-group-ingress",
            "--group-id",
            &self.env().edge_security_group_id,
            "--protocol",
            "tcp",
            "--port",
            "80",
            "--cidr",
            cidr,
        ]);
    }

    /// Remove what a previous run's Rise created and its teardown could not.
    ///
    /// Rise owns these, not Terraform, and the reconciler only visits services
    /// whose project is in its database — so once the per-run stack is
    /// destroyed, nothing else will ever collect them.
    fn sweep(&self) -> Result<()> {
        let cluster = &self.env().cluster_name;

        let arns: Vec<String> = serde_json::from_str(&self.aws(&[
            "ecs",
            "list-services",
            "--cluster",
            cluster,
            "--query",
            "serviceArns[]",
            "--output",
            "json",
        ])?)
        .unwrap_or_default();

        let mut swept = 0u32;
        for chunk in arns.chunks(10) {
            let mut args: Vec<String> = vec![
                "ecs".into(),
                "describe-services".into(),
                "--cluster".into(),
                cluster.clone(),
                "--include".into(),
                "TAGS".into(),
                "--services".into(),
            ];
            args.extend(chunk.iter().cloned());
            args.extend(["--output".into(), "json".into()]);
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            let parsed: serde_json::Value =
                serde_json::from_str(&self.aws(&refs)?).unwrap_or_default();

            for svc in parsed["services"].as_array().unwrap_or(&vec![]) {
                let managed = svc["tags"]
                    .as_array()
                    .is_some_and(|tags| tags.iter().any(|t| t["key"] == MANAGED_BY_TAG));
                let live = svc["status"] != serde_json::json!("INACTIVE");
                let Some(name) = svc["serviceName"].as_str() else {
                    continue;
                };
                if !managed || !live {
                    continue;
                }
                let _ = self.aws(&[
                    "ecs",
                    "delete-service",
                    "--cluster",
                    cluster,
                    "--service",
                    name,
                    "--force",
                ]);
                swept += 1;
            }
        }

        // Repositories are billed while they hold images, and `auto_remove` only
        // fires on a project delete that never happened.
        let repos: Vec<String> = serde_json::from_str(
            &self
                .aws(&[
                    "ecr",
                    "describe-repositories",
                    "--query",
                    &format!(
                        "repositories[?starts_with(repositoryName, '{}')].repositoryName",
                        self.env().ecr_repo_prefix
                    ),
                    "--output",
                    "json",
                ])
                .unwrap_or_else(|_| "[]".into()),
        )
        .unwrap_or_default();
        for repo in &repos {
            let _ = self.aws(&[
                "ecr",
                "delete-repository",
                "--repository-name",
                repo,
                "--force",
            ]);
        }

        // Secret env vars a previous run's deployments wrote.
        let params: Vec<String> = serde_json::from_str(
            &self
                .aws(&[
                    "ssm",
                    "get-parameters-by-path",
                    "--path",
                    &format!("/{}/", self.env_name),
                    "--recursive",
                    "--query",
                    "Parameters[].Name",
                    "--output",
                    "json",
                ])
                .unwrap_or_else(|_| "[]".into()),
        )
        .unwrap_or_default();
        let stale: Vec<&str> = params
            .iter()
            .map(String::as_str)
            // Never the bootstrap's own description of the environment.
            .filter(|n| !n.ends_with("/e2e/bootstrap"))
            .collect();
        for chunk in stale.chunks(10) {
            let mut args = vec!["ssm", "delete-parameters", "--names"];
            args.extend(chunk);
            let _ = self.aws(&args);
        }

        if swept > 0 || !repos.is_empty() || !stale.is_empty() {
            report::note(&format!(
                "swept {swept} leaked service(s), {} repositor(ies), {} parameter(s)",
                repos.len(),
                stale.len()
            ));
        }
        Ok(())
    }

    /// Delete every project the suite made, so the workloads go with them.
    ///
    /// Preferred over the imperative sweep because it exercises project delete
    /// and the ECR `auto_remove` path rather than going around them.
    fn delete_projects(&self) {
        let Ok(out) = self.rise_cli(&["project", "list", "--output", "json"], None) else {
            return;
        };
        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&out.stdout) else {
            return;
        };
        for p in parsed.as_array().unwrap_or(&vec![]) {
            let Some(name) = p.get("name").and_then(|v| v.as_str()) else {
                continue;
            };
            let _ = self.rise_cli(&["project", "delete", name, "--yes"], None);
        }
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
        cli::run_checked(cp).context("docker cp (CLI extract)")?;

        let mut rm2 = Command::new("docker");
        rm2.args(["rm", "-f", &self.extract_container]);
        let _ = cli::run(rm2);

        self.cli_bin = Some(bin);
        Ok(())
    }

    fn wait_rise_healthy(&self, step: &str) -> Result<()> {
        let url = format!("{}/health", self.stack().rise_url);
        report::step_value(step, || {
            http::poll(
                Duration::from_secs(300),
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

    fn app_host(&self, project: &str) -> String {
        format!("{}.{}", project, self.stack().domain)
    }
}

/// 32 random bytes, base64. `/dev/urandom` rather than a crate: the harness has
/// no `rand` dependency and this is the only place it would need one.
fn random_secret_b64() -> Result<String> {
    use base64::Engine;
    let bytes = std::fs::read("/dev/urandom")
        .map(|b| b.into_iter().take(32).collect::<Vec<u8>>())
        .or_else(|_| -> Result<Vec<u8>> {
            let mut f = std::fs::File::open("/dev/urandom")?;
            use std::io::Read;
            let mut buf = [0u8; 32];
            f.read_exact(&mut buf)?;
            Ok(buf.to_vec())
        })
        .context("read random bytes")?;
    anyhow::ensure!(bytes.len() == 32, "short read from /dev/urandom");
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

impl Backend for EcsBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Ecs
    }

    fn bring_up(&mut self) -> Result<()> {
        let env = self.read_bootstrap_env()?;
        report::note(&format!(
            "environment '{}' — cluster={} domain={}",
            self.env_name, env.cluster_name, env.dns_zone_name
        ));
        self.env = Some(env);

        // Before anything else: a crashed run leaves workloads no later run can
        // collect, and they consume the account's Fargate quota until removed.
        report::step("sweep leaked workloads from earlier runs", || self.sweep())?;

        report::step("terraform init", || {
            cli::run_checked(self.terraform(&[
                "init",
                "-input=false",
                "-reconfigure",
                &format!("-backend-config=bucket={}", self.env().state_bucket),
                "-backend-config=key=run/terraform.tfstate",
                &format!("-backend-config=region={}", self.region),
            ]))
        })?;

        report::step("terraform destroy (stale per-run state)", || {
            let _ = cli::run(self.terraform(&["destroy", "-auto-approve", "-input=false"]));
            Ok(())
        })?;

        let traefik_ip = report::step_value("resolve Traefik's address", || {
            self.service_public_ip(&self.env().traefik_service_name.clone())
        })?;

        report::step("point the environment's DNS at Traefik", || {
            self.upsert_dns(&traefik_ip)
        })?;

        report::step("open the edge for this run", || self.authorize_edge())?;

        let domain = self.env().dns_zone_name.clone();
        report::step("terraform apply (postgres, rise)", || {
            cli::run_checked(self.terraform(&[
                "apply",
                "-auto-approve",
                "-input=false",
                &format!("-var=name={}", self.env_name),
                &format!("-var=region={}", self.region),
                &format!("-var=state_bucket={}", self.env().state_bucket),
                &format!("-var=rise_image={}", self.image_repository),
                &format!("-var=rise_image_tag={}", self.image_tag),
                &format!("-var=ingress_domain={domain}"),
                &format!("-var=jwt_signing_secret={}", self.jwt_secret_b64),
                &format!("-var=encryption_key={}", self.jwt_secret_b64),
            ]))
        })?;

        let rise_url = format!("http://rise.{domain}");
        report::note(&format!("traefik={traefik_ip} domain={domain}"));

        self.stack = Some(StackState {
            traefik_api: format!("http://{traefik_ip}:8080"),
            traefik_ip,
            domain,
            rise_url: rise_url.clone(),
        });

        // The bearer's `iss` must equal the server's public_url, and the secret
        // must be the one the per-run stack was just given.
        self.ci_token = token::mint_ci_token(&self.jwt_secret_b64, &rise_url)?;
        self.dex = Some(DexEndpoint {
            token_url: format!("http://dex.{}/dex/token", self.stack().domain),
            client_id: "rise-backend".to_string(),
            client_secret: "rise-backend-secret".to_string(),
            // Dex stamps its Cloud Map address in `iss`; Rise validates against
            // exactly that and fetches JWKS over the cluster's private DNS.
            issuer: self.env().dex_issuer.clone(),
        });

        self.wait_rise_healthy("rise /health")?;
        report::step("extract rise CLI from image", || self.extract_cli())
    }

    fn tear_down(&mut self) {
        if std::env::var("KEEP").is_ok() {
            report::note(
                "KEEP set — leaving the per-run stack up. The edge stays open and its \
                 workloads keep consuming quota until you destroy it.",
            );
            return;
        }

        // Through the API first: it exercises project delete and the ECR
        // auto-remove path, and it is the only thing that removes the workloads
        // before the control plane that owns them goes away.
        self.delete_projects();

        let _ = cli::run(self.terraform(&["destroy", "-auto-approve", "-input=false"]));

        // Belt and braces: anything project deletion missed.
        let _ = self.sweep();
        self.revoke_edge();

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

    fn wait_registry_ready(&self, project: &str) -> Result<()> {
        let repo = format!("{}{project}", self.env().ecr_repo_prefix);
        let deadline = std::time::Instant::now() + Duration::from_secs(90);
        loop {
            if self
                .aws(&["ecr", "describe-repositories", "--repository-names", &repo])
                .is_ok()
            {
                return Ok(());
            }
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "ECR repository {repo:?} was not provisioned within 90s. Rise's ECR \
                 controller creates it on a 10s poll under a leader lease — check the \
                 control plane's logs for AssumeRole or ecr:CreateRepository denials."
            );
            std::thread::sleep(Duration::from_secs(5));
        }
    }

    fn deployed_image(&self, project: &str) -> Result<Option<String>> {
        let services = self.project_services(project)?;
        let Some(service) = services.first() else {
            anyhow::bail!("no live ECS service tagged for project {project:?}");
        };
        let td = self
            .aws(&[
                "ecs",
                "describe-services",
                "--cluster",
                &self.env().cluster_name,
                "--services",
                service,
                "--query",
                "services[0].taskDefinition",
                "--output",
                "text",
            ])?
            .trim()
            .to_string();
        let image = self
            .aws(&[
                "ecs",
                "describe-task-definition",
                "--task-definition",
                &td,
                "--query",
                "taskDefinition.containerDefinitions[0].image",
                "--output",
                "text",
            ])?
            .trim()
            .to_string();
        Ok(Some(image))
    }

    fn cli_visible_path(&self, rel: &str) -> String {
        self.repo_root.join(rel).to_string_lossy().to_string()
    }

    fn reach_app(&self, project: &str, path: &str) -> Result<Option<HttpResponse>> {
        // Straight at Traefik with an explicit Host header, so app reach never
        // depends on DNS having propagated.
        let url = format!("http://{}{}", self.stack().traefik_ip, path);
        let host = self.app_host(project);
        let mut resp = None;
        // Fargate task start plus Traefik's ECS-provider poll means a freshly
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

    fn dex(&self) -> Option<&DexEndpoint> {
        self.dex.as_ref()
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

    /// Assert the ECS task definition carries the Traefik forwardAuth labels.
    ///
    /// The ECS analogue of inspecting container labels on Docker: the provider
    /// reads routing configuration from `dockerLabels` on the container
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
                        &self.env().cluster_name,
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
}
