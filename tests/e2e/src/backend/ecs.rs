//! The Amazon ECS backend driver.
//!
//! Like the other two drivers, this one owns its environment's lifecycle — but
//! it owns only half of it. The AWS side is split:
//!
//! - **`tests/e2e/bootstrap`** is applied once, by hand, and never touched
//!   here: VPC, cluster, Cloud Map, Traefik, Dex, a Route 53 zone, and all the
//!   IAM. It is separate because CI runs under a role with no IAM-write.
//! - **`tests/e2e/run`** is applied and destroyed around every suite:
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
//! Rise owns, not Terraform, and destroying the per-run stack takes Rise and its
//! database with them. The next run's control plane does collect them — its
//! orphan sweep deletes any managed service whose deployment the database no
//! longer knows — but only once it is up, which is after the apply that needs
//! the Fargate quota those leftovers are holding. Sweeping first is what keeps a
//! previous run's debris from failing this one's apply, and it runs at bring-up
//! rather than only at teardown because a crashed run never reaches teardown.
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
/// Carries the controller class, which for this harness is the run scope.
const CONTROLLER_CLASS_TAG: &str = "rise.dev/controller-class";

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
    /// Isolates this run from every other one sharing the cluster: the DNS
    /// subtree, Rise's controller class and this run's Traefik constraint are
    /// all derived from it.
    scope: String,
}

/// Lowercase, and anything not `[a-z0-9-]` folded to `-`, so a login name can
/// be used as a DNS label.
fn sanitize_label(raw: &str) -> String {
    let s: String = raw
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    s.trim_matches('-').chars().take(40).collect()
}

fn is_dns_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && !s.starts_with('-')
        && !s.ends_with('-')
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// The persistent environment, read from the SSM parameter the bootstrap writes.
struct BootstrapEnv {
    cluster_name: String,
    log_group_name: String,
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
        // CI sets this per run -- `pr-<number>` on a pull request, `nightly-<id>`
        // on a schedule. Locally it defaults to one stable label per user, so
        // repeated local runs reuse a subtree instead of littering the zone.
        let scope = match std::env::var("RISE_E2E_SCOPE") {
            Ok(s) => s,
            Err(_) => {
                let who = std::env::var("USER")
                    .or_else(|_| std::env::var("USERNAME"))
                    .unwrap_or_else(|_| "local".to_string());
                format!("dev-{}", sanitize_label(&who))
            }
        };
        anyhow::ensure!(
            is_dns_label(&scope),
            "RISE_E2E_SCOPE must be a single lowercase DNS label, got {scope:?}"
        );
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
            scope,
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
        self.repo_root.join("tests").join("e2e").join("run")
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

    /// One output of the per-run stack.
    ///
    /// The values that describe *this run* -- its Dex issuer and token URL --
    /// are derived from the scope inside Terraform, so reading them back is
    /// better than recomputing the same string here and letting the two drift.
    fn run_output(&self, name: &str) -> Result<String> {
        let out = cli::run_checked(self.terraform(&["output", "-raw", name]))
            .with_context(|| format!("terraform output -raw {name}"))?;
        Ok(out.stdout.trim().to_string())
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
                    "read /{}/e2e/bootstrap. Has tests/e2e/bootstrap been applied \
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
            log_group_name: get("log_group_name")?,
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
    /// This run's Traefik address, waited for rather than sampled.
    ///
    /// Every stage of the lookup can legitimately be "not yet": the service was
    /// created seconds ago by the apply, a Fargate task takes tens of seconds to
    /// place and pull, its ENI is attached only once it leaves PROVISIONING, and
    /// the public IP is associated a moment after that. So the whole chain sits
    /// inside the poll, and any stage coming back empty just means try again.
    ///
    /// Note `--desired-status RUNNING` filters on *desired* status, which a
    /// PENDING task already satisfies -- listing on that alone returns a task
    /// whose ENI does not exist yet, and the lookup then fails on the literal
    /// string "None". The `lastStatus` check below is the one that matters.
    fn service_public_ip(&self, service: &str) -> Result<String> {
        let deadline = std::time::Instant::now() + Duration::from_secs(300);
        loop {
            match self.try_service_public_ip(service) {
                Some(ip) => return Ok(ip),
                None => {
                    anyhow::ensure!(
                        std::time::Instant::now() < deadline,
                        "{service} had no running task with a public address within 5 \
                         minutes. Its events and stopped-task reasons follow below -- \
                         a task that never starts is usually an image it cannot pull or an \
                         exhausted Fargate quota."
                    );
                    std::thread::sleep(Duration::from_secs(5));
                }
            }
        }
    }

    /// One attempt at the chain. `None` means "not yet", not "broken".
    fn try_service_public_ip(&self, service: &str) -> Option<String> {
        let cluster = &self.env().cluster_name;
        let value = |v: String| -> Option<String> {
            let v = v.trim().to_string();
            (!v.is_empty() && v != "None").then_some(v)
        };

        let task = value(
            self.aws(&[
                "ecs",
                "list-tasks",
                "--cluster",
                cluster,
                "--service-name",
                service,
                "--desired-status",
                "RUNNING",
                "--query",
                "taskArns[0]",
                "--output",
                "text",
            ])
            .ok()?,
        )?;

        let eni = value(
            self.aws(&[
                "ecs",
                "describe-tasks",
                "--cluster",
                cluster,
                "--tasks",
                &task,
                "--query",
                "tasks[?lastStatus=='RUNNING'] | [0].attachments[0].details[?name=='networkInterfaceId'].value | [0]",
                "--output",
                "text",
            ])
            .ok()?,
        )?;

        value(
            self.aws(&[
                "ec2",
                "describe-network-interfaces",
                "--network-interface-ids",
                &eni,
                "--query",
                "NetworkInterfaces[0].Association.PublicIp",
                "--output",
                "text",
            ])
            .ok()?,
        )
    }

    /// Point this run's scope at its Traefik, or remove it again.
    ///
    /// Both records are scoped: `<scope>.<zone>` and `*.<scope>.<zone>`. Runs
    /// therefore never contend for a name, which is what lets them overlap --
    /// a shared apex would have each run repointing the other's traffic.
    ///
    /// The wildcard is required: projects are served at
    /// `<project>.<scope>.<zone>`. One wildcard label is enough because groups
    /// and environments join the project name with a dash, not a dot.
    fn change_dns(&self, action: &str, ip: &str) -> Result<()> {
        let env = self.env();
        let records: Vec<serde_json::Value> = ["", "*."]
            .iter()
            .map(|prefix| {
                serde_json::json!({
                    "Action": action,
                    "ResourceRecordSet": {
                        "Name": format!("{prefix}{}.{}", self.scope, env.dns_zone_name),
                        "Type": "A",
                        "TTL": 60,
                        "ResourceRecords": [{ "Value": ip }],
                    }
                })
            })
            .collect();
        let changes = serde_json::json!({
            "Comment": format!("rise e2e {}", self.scope),
            "Changes": records,
        });

        let path = std::env::temp_dir().join(format!("rise-e2e-dns-{}.json", std::process::id()));
        std::fs::write(&path, serde_json::to_vec(&changes)?)
            .context("write the DNS change batch")?;

        let out = self.aws(&[
            "route53",
            "change-resource-record-sets",
            "--hosted-zone-id",
            &env.dns_zone_id,
            "--change-batch",
            &format!("file://{}", path.display()),
        ]);
        let _ = std::fs::remove_file(&path);
        out.map(|_| ()).with_context(|| {
            format!(
                "{action} {}.{} in {}",
                self.scope, env.dns_zone_name, env.dns_zone_id
            )
        })
    }

    /// Put this run's variables where Terraform finds them unprompted.
    ///
    /// `*.auto.tfvars` is loaded for every command in the workspace, and that is
    /// the point: `destroy` evaluates the configuration before it looks at
    /// state, so a required variable with no value fails it outright. Passing
    /// values only on the `apply` command line left all three teardown paths --
    /// the stale-state destroy at bring-up, the harness's own teardown, and the
    /// workflow's cancelled-job backstop -- unable to destroy anything, each
    /// failing with "No value for required variable" and each ignoring it.
    ///
    /// The file also lets the workflow tear down a run whose harness died,
    /// which it otherwise could not: it has no way to learn the bucket or the
    /// zone. `*.tfvars` is gitignored, and the run is ephemeral.
    fn write_run_vars(&self, client_cidr: &str) -> Result<()> {
        let q = |v: &str| serde_json::to_string(v).unwrap_or_else(|_| "\"\"".to_string());
        let env = self.env();
        let body = [
            format!("name               = {}", q(&self.env_name)),
            format!("region             = {}", q(&self.region)),
            format!("state_bucket       = {}", q(&env.state_bucket)),
            format!("rise_image         = {}", q(&self.image_repository)),
            format!("rise_image_tag     = {}", q(&self.image_tag)),
            format!("scope              = {}", q(&self.scope)),
            format!("dns_zone_name      = {}", q(&env.dns_zone_name)),
            format!("jwt_signing_secret = {}", q(&self.jwt_secret_b64)),
            format!("encryption_key     = {}", q(&self.jwt_secret_b64)),
            format!("authorized_cidrs   = [{}]", q(client_cidr)),
            String::new(),
        ]
        .join("\n");

        let path = self.run_dir().join("run.auto.tfvars");
        std::fs::write(&path, body).with_context(|| format!("write {}", path.display()))
    }

    /// This machine's address, as the single CIDR the run's edge admits.
    ///
    /// The group is created with the run and destroyed with it, so there is no
    /// rule to revoke afterwards and nothing a crashed run can leave open.
    fn client_cidr(&self) -> Result<String> {
        let ip = http::get("https://checkip.amazonaws.com", None)
            .context("determine this machine's public address")?
            .body
            .trim()
            .to_string();
        Ok(format!("{ip}/32"))
    }

    /// Remove what an earlier run *of this scope* left behind.
    ///
    /// Rise owns these, not Terraform, and the reconciler only visits services
    /// whose project is in its database — so once the per-run stack is
    /// destroyed, nothing else will ever collect them.
    ///
    /// Scoped deliberately. The cluster is shared with every other run, and
    /// their services carry the same `managed-by` tag; sweeping on that alone
    /// would delete a concurrent run's live workloads. The controller-class tag
    /// is the same token Rise's own orphan collector scopes to.
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
                let tag = |key: &str| -> Option<&str> {
                    svc["tags"].as_array()?.iter().find(|t| t["key"] == key)?["value"].as_str()
                };
                let managed = tag(MANAGED_BY_TAG).is_some();
                let ours = tag(CONTROLLER_CLASS_TAG) == Some(self.scope.as_str());
                let live = svc["status"] != serde_json::json!("INACTIVE");
                let Some(name) = svc["serviceName"].as_str() else {
                    continue;
                };
                if !managed || !ours || !live {
                    continue;
                }
                match self.aws(&[
                    "ecs",
                    "delete-service",
                    "--cluster",
                    cluster,
                    "--service",
                    name,
                    "--force",
                ]) {
                    Ok(_) => swept += 1,
                    // Counting the attempt would report cleanups that did not
                    // happen, and the run afterwards looks clean when it is not.
                    Err(e) => report::note(&format!("could not sweep service {name}: {e:#}")),
                }
            }
        }

        // Repositories are billed while they hold images, and `auto_remove` only
        // fires on a project delete that never happened.
        //
        // Under this scope's own segment of the shared prefix, matching
        // `local.scoped_ecr_repo_prefix` in the per-run root -- the bootstrap
        // prefix alone spans every run, and a concurrent one's images are live.
        let repo_prefix = format!("{}{}/", self.env().ecr_repo_prefix, self.scope);
        let repos: Vec<String> = serde_json::from_str(
            &self
                .aws(&[
                    "ecr",
                    "describe-repositories",
                    "--query",
                    &format!(
                        "repositories[?starts_with(repositoryName, '{repo_prefix}')].repositoryName"
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
                    // Likewise scoped, matching `ssm_parameter_prefix` in the
                    // per-run root: another run's secrets are in use.
                    "--path",
                    &format!("/{}/{}/", self.env_name, self.scope),
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
            // Never the bootstrap's own description of the environment, which a
            // scope literally named `e2e` would otherwise sweep out from under
            // every other run.
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

    /// Wait until Dex answers its discovery document through this run's Traefik.
    ///
    /// Rise itself needs no such wait: it fetches discovery lazily, on the first
    /// token it validates, so its task can start alongside Dex. The harness is
    /// the one that cannot proceed -- it mints a token before anything else.
    fn wait_dex_ready(&self) -> Result<()> {
        let issuer_host = format!("dex.{}", self.stack().domain);
        let url = format!("http://{issuer_host}/dex/.well-known/openid-configuration");
        let outcome = http::poll(
            Duration::from_secs(300),
            Duration::from_secs(5),
            "dex discovery through Traefik",
            || {
                Ok(http::get(&url, None)
                    .map(|r| r.status == 200)
                    .unwrap_or(false))
            },
        );
        if outcome.is_err() {
            // Three layers can fail this and the poll cannot tell them apart:
            // the scoped DNS record, Traefik discovering Dex, and Dex itself.
            // Ask each directly so the log says which.
            self.explain_unreachable(&issuer_host, "/dex/.well-known/openid-configuration");
        }
        outcome?;
        Ok(())
    }

    /// Say which layer is broken when a host behind Traefik will not answer.
    ///
    /// Bypassing DNS with an explicit `Host` header against Traefik's address
    /// separates "the name does not resolve" from "Traefik has not discovered
    /// this container", and Traefik's own router list separates that from "the
    /// container is not running".
    fn explain_unreachable(&self, host: &str, path: &str) {
        eprintln!("\n--- why {host} is unreachable ---");

        match std::net::ToSocketAddrs::to_socket_addrs(&(host, 80)) {
            Ok(addrs) => {
                let ips: Vec<String> = addrs.map(|a| a.ip().to_string()).collect();
                eprintln!(
                    "DNS: {host} resolves to {ips:?} (expected {})",
                    self.stack().traefik_ip
                );
            }
            Err(e) => eprintln!(
                "DNS: {host} does not resolve ({e}). Is the zone delegated from the \
                 registrar to this Route 53 zone? Nothing under it resolves until it is."
            ),
        }

        let direct = format!("http://{}{path}", self.stack().traefik_ip);
        match http::get(&direct, Some(host)) {
            Ok(r) => eprintln!(
                "Traefik (by IP, Host: {host}): HTTP {} -- so Traefik is up; a 404 here \
                 means it has not discovered the container, and a 502/503 means it has \
                 a router but no healthy server behind it.",
                r.status
            ),
            Err(e) => eprintln!("Traefik (by IP, Host: {host}): unreachable ({e})"),
        }
    }

    fn app_host(&self, project: &str) -> String {
        format!("{}.{}", project, self.stack().domain)
    }
}

/// 32 random bytes, base64. `/dev/urandom` rather than a crate: the harness has
/// no `rand` dependency and this is the only place it would need one.
/// 32 random bytes, base64.
///
/// Opened and read to a fixed buffer rather than with `fs::read`, which reads to
/// EOF -- and `/dev/urandom` has no EOF. That call never returns; it allocates
/// at the device's throughput until the machine dies, which on a CI runner
/// looks like the whole job being killed about a minute in with no output.
fn random_secret_b64() -> Result<String> {
    use base64::Engine;
    use std::io::Read;

    let mut f = std::fs::File::open("/dev/urandom").context("open /dev/urandom")?;
    let mut buf = [0u8; 32];
    f.read_exact(&mut buf).context("read random bytes")?;
    Ok(base64::engine::general_purpose::STANDARD.encode(buf))
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
                // Per scope: concurrent runs would otherwise overwrite each
                // other's state and destroy resources they do not own.
                &format!("-backend-config=key=run/{}/terraform.tfstate", self.scope),
                &format!("-backend-config=region={}", self.region),
            ]))
        })?;

        let client_cidr =
            report::step_value("determine this run's client address", || self.client_cidr())?;

        // Before the destroy below, not just before the apply: destroy needs the
        // variables too. See `write_run_vars`.
        report::step("write this run's terraform variables", || {
            self.write_run_vars(&client_cidr)
        })?;

        report::step("terraform destroy (stale per-run state)", || {
            let _ = cli::run(self.terraform(&["destroy", "-auto-approve", "-input=false"]));
            Ok(())
        })?;

        let domain = format!("{}.{}", self.scope, self.env().dns_zone_name);
        report::step("terraform apply (traefik, dex, postgres, rise)", || {
            cli::run_checked(self.terraform(&["apply", "-auto-approve", "-input=false"]))
        })?;

        // Traefik belongs to this run, so its address exists only now.
        let traefik_service = format!("{}-{}-traefik", self.env_name, self.scope);
        let traefik_ip = report::step_value("resolve this run's Traefik address", || {
            self.service_public_ip(&traefik_service)
        })?;

        report::step("point this run's DNS at its Traefik", || {
            self.change_dns("UPSERT", &traefik_ip)
        })?;

        let rise_url = format!("http://rise.{domain}");
        report::note(&format!(
            "scope={} traefik={traefik_ip} domain={domain}",
            self.scope
        ));

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
            // Public, through this run's Traefik: the harness runs outside the
            // VPC and mints tokens with the password grant.
            token_url: self.run_output("dex_token_url")?,
            client_id: "rise-backend".to_string(),
            client_secret: "rise-backend-secret".to_string(),
            // Dex stamps its Cloud Map address in `iss`; Rise validates against
            // exactly that and fetches JWKS over the cluster's private DNS. It
            // is never publicly resolvable, and does not need to be.
            issuer: self.run_output("dex_issuer")?,
        });

        // Dex is per-run now, so it is not already up. Waiting on the public
        // token endpoint checks the whole path the harness depends on at once:
        // the scoped DNS record, this run's Traefik discovering a container
        // through its controller-class constraint, and Dex itself serving.
        report::step("wait for Dex through this run's Traefik", || {
            self.wait_dex_ready()
        })?;

        self.wait_rise_healthy("rise /health")?;
        report::step("extract rise CLI from image", || self.extract_cli())
    }

    fn tear_down(&mut self) {
        if std::env::var("KEEP").is_ok() {
            report::note(
                "KEEP set — leaving the per-run stack up. Its DNS scope stays pointed at \
                 Traefik and its workloads keep consuming quota until you destroy it.",
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

        // The security group went with the stack, so there is no rule to
        // revoke -- but the DNS records live in the persistent zone and would
        // otherwise accumulate one dead subtree per run. Route 53 matches a
        // DELETE on the record's exact value, which is why this uses the
        // address recorded at bring-up rather than re-resolving a service that
        // no longer exists.
        if let Some(ip) = self.stack.as_ref().map(|s| s.traefik_ip.clone()) {
            let _ = self.change_dns("DELETE", &ip);
        }

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

    /// The ECS backend had none, so a failed run in the one environment you
    /// cannot look at directly captured nothing at all. These four answer the
    /// questions a failure actually raises: did the services place tasks, why
    /// did a task stop, what did the containers say, and did Traefik discover
    /// them.
    fn dump_diagnostics(&self) {
        let Some(env) = self.env.as_ref() else {
            return;
        };
        let cluster = env.cluster_name.clone();
        let services: Vec<String> = ["traefik", "dex", "postgres", "rise"]
            .iter()
            .map(|c| format!("{}-{}-{c}", self.env_name, self.scope))
            .collect();

        // Placement failures -- an unpullable image, an exhausted quota, a
        // subnet with no route out -- surface here and nowhere else.
        let mut ev = Command::new("aws");
        ev.args([
            "ecs",
            "describe-services",
            "--cluster",
            &cluster,
            "--services",
        ]);
        ev.args(&services);
        ev.args([
            "--query",
            "services[].{name:serviceName,running:runningCount,desired:desiredCount,events:events[:8].message}",
            "--output",
            "yaml",
        ]);
        cli::dump("ECS service events", ev);

        for service in &services {
            let arns = self
                .aws(&[
                    "ecs",
                    "list-tasks",
                    "--cluster",
                    &cluster,
                    "--service-name",
                    service,
                    "--desired-status",
                    "STOPPED",
                    "--query",
                    "taskArns[:3]",
                    "--output",
                    "text",
                ])
                .unwrap_or_default();
            let arns: Vec<String> = arns
                .split_whitespace()
                .filter(|a| !a.is_empty() && *a != "None")
                .map(str::to_string)
                .collect();
            if arns.is_empty() {
                continue;
            }
            let mut why = Command::new("aws");
            why.args(["ecs", "describe-tasks", "--cluster", &cluster, "--tasks"]);
            why.args(&arns);
            why.args([
                "--query",
                "tasks[].{stopped:stoppedReason,containers:containers[].{name:name,reason:reason,exit:exitCode}}",
                "--output",
                "yaml",
            ]);
            cli::dump(&format!("why {service} tasks stopped"), why);
        }

        for container in ["traefik", "dex", "postgres", "rise"] {
            let mut logs = Command::new("aws");
            logs.args([
                "logs",
                "tail",
                &env.log_group_name,
                "--log-stream-name-prefix",
                &format!("{container}-{}", self.scope),
                "--since",
                "20m",
                "--format",
                "short",
            ]);
            cli::dump(&format!("{container} logs"), logs);
        }

        // The routing question, answered directly: if Dex is missing from this
        // list, Traefik never discovered it -- most likely the constraint and
        // the container's controller-class label disagree.
        if let Some(stack) = self.stack.as_ref() {
            match http::get(&format!("{}/api/http/routers", stack.traefik_api), None) {
                Ok(r) => eprintln!("\n--- Traefik routers ---\n{}", r.body),
                Err(e) => eprintln!("\n--- Traefik routers ---\n(unreachable: {e})"),
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the shape, not just the output. The previous version reached for
    /// `fs::read("/dev/urandom")`, which reads to an EOF the device never sends:
    /// it allocated at ~370 MB/s until the machine died, so a CI job vanished
    /// about a minute in with nothing but its banner printed. A bounded read
    /// returns immediately, which is what the timeout here checks.
    #[test]
    fn a_secret_is_32_bytes_and_returns_promptly() {
        let started = std::time::Instant::now();
        let a = random_secret_b64().expect("generate");
        let elapsed = started.elapsed();

        use base64::Engine;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(&a)
            .expect("valid base64");
        assert_eq!(raw.len(), 32, "the signing key must be 32 bytes");

        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "reading 32 random bytes took {elapsed:?} -- an unbounded read of an \
             endless device is back"
        );

        let b = random_secret_b64().expect("generate");
        assert_ne!(a, b, "every run must mint its own key");
    }

    #[test]
    fn a_scope_must_be_a_single_dns_label() {
        assert!(is_dns_label("pr-457"));
        assert!(is_dns_label("nightly"));
        // A dot would silently widen the wildcard record a run writes.
        assert!(!is_dns_label("pr.457"));
        assert!(!is_dns_label("PR-457"));
        assert!(!is_dns_label("-pr"));
        assert!(!is_dns_label(""));
        assert_eq!(sanitize_label("Niklas R"), "niklas-r");
    }
}
