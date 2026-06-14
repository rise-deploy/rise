//! Backend-agnostic scenarios. Each is written once against `dyn Backend` and
//! declares which backends it `applies_to`; an unsupported combo is a logged
//! `Skip(reason)` — a visible parity gap, never silent drift.

use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result};

use crate::backend::{Backend, BackendKind, CliAuth};
use crate::cli::CliOutput;
use crate::dex;
use crate::http;

pub enum Applicability {
    Run,
    Skip(&'static str),
}

pub trait Scenario {
    fn id(&self) -> &'static str;
    fn applies_to(&self, b: &dyn Backend) -> Applicability;
    fn run(&self, b: &dyn Backend) -> Result<()>;
}

/// The scenarios the harness runs. Workload-identity, health-rolling cutover and
/// private/forwardAuth are tracked as follow-ups (ROADMAP).
pub fn all() -> Vec<Box<dyn Scenario>> {
    vec![
        Box::new(PublicDeploy),
        Box::new(SaTokenExchange),
        Box::new(PrivateForwardAuth),
        Box::new(LokiLogRetention),
        Box::new(HelmIdempotency),
        // Last: on Docker it recreates the backend with the identity overlay, so
        // it must run after the scenarios that expect the default config.
        Box::new(WorkloadIdentity),
    ]
}

/// Run every scenario applicable to `b`, printing RUN/PASS/FAIL/SKIP lines, and
/// fail if any applicable scenario failed.
pub fn run_all(b: &dyn Backend) -> Result<()> {
    let mut failed: Vec<&'static str> = Vec::new();
    for s in all() {
        match s.applies_to(b) {
            Applicability::Skip(reason) => {
                eprintln!("[e2e] SKIP {} on {}: {reason}", s.id(), b.name())
            }
            Applicability::Run => {
                eprintln!("[e2e] RUN  {} on {}", s.id(), b.name());
                match s.run(b) {
                    Ok(()) => eprintln!("[e2e] PASS {} on {}", s.id(), b.name()),
                    Err(e) => {
                        eprintln!("[e2e] FAIL {} on {}: {e:#}", s.id(), b.name());
                        failed.push(s.id());
                    }
                }
            }
        }
    }
    anyhow::ensure!(failed.is_empty(), "scenarios failed: {failed:?}");
    Ok(())
}

/// Unique-enough project name per run (DNS-safe), so a stale stack can't collide.
fn unique(prefix: &str) -> String {
    // Process id + per-process atomic counter: two scenarios in one process can't
    // collide, and separate runs differ by pid. DNS-safe (starts with the letter
    // prefix, lowercase alphanumeric + hyphens).
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn expect_ok(out: CliOutput, what: &str) -> Result<CliOutput> {
    anyhow::ensure!(
        out.success(),
        "{what} failed (exit {:?}):\n{}",
        out.status,
        out.combined()
    );
    Ok(out)
}

// ---- (a) public deploy + reachable -----------------------------------------

struct PublicDeploy;

impl Scenario for PublicDeploy {
    fn id(&self) -> &'static str {
        "public-deploy"
    }

    fn applies_to(&self, _b: &dyn Backend) -> Applicability {
        Applicability::Run
    }

    fn run(&self, b: &dyn Backend) -> Result<()> {
        let project = unique("e2e-pub");
        let app = b.sample_app();
        expect_ok(
            b.rise_cli(
                &[
                    "project",
                    "create",
                    &project,
                    "--access-class",
                    "public",
                    "--no-rise-toml",
                ],
                None,
            )?,
            "project create",
        )?;
        expect_ok(
            b.rise_cli(
                &[
                    "deploy",
                    "--project",
                    &project,
                    "--image",
                    app.image,
                    "--http-port",
                    app.http_port,
                    "--replicas",
                    "1",
                ],
                None,
            )?,
            "deploy",
        )?;
        b.wait_healthy(&project)?;

        match b.reach_app(&project, "/")? {
            Some(resp) => {
                anyhow::ensure!(
                    resp.status == 200,
                    "expected 200 from app, got {}",
                    resp.status
                );
                if let Some(marker) = app.body_marker {
                    anyhow::ensure!(
                        resp.body.contains(marker),
                        "response body missing expected marker {marker:?}:\n{}",
                        resp.body
                    );
                }
            }
            None => eprintln!(
                "[e2e] {}: app-HTTP reach not wired for {} — asserted via Healthy only",
                self.id(),
                b.name()
            ),
        }
        Ok(())
    }
}

// ---- (c) SA token exchange via RISE_IDENTITY -------------------------------

struct SaTokenExchange;

impl Scenario for SaTokenExchange {
    fn id(&self) -> &'static str {
        "sa-token-exchange"
    }

    fn applies_to(&self, _b: &dyn Backend) -> Applicability {
        // Both backends now expose a reachable Dex (Docker via the compose overlay,
        // minikube via a `kubectl port-forward` to the in-cluster Dex).
        Applicability::Run
    }

    fn run(&self, b: &dyn Backend) -> Result<()> {
        let dexep = b.dex().context("backend exposes no reachable Dex")?;
        let project = unique("e2e-sa");

        expect_ok(
            b.rise_cli(
                &[
                    "project",
                    "create",
                    &project,
                    "--access-class",
                    "public",
                    "--no-rise-toml",
                ],
                None,
            )?,
            "project create",
        )?;

        // Register an SA on this project trusting Dex, keyed on the email claim.
        let created = expect_ok(
            b.rise_cli(
                &[
                    "service-account",
                    "create",
                    "--project",
                    &project,
                    "--issuer",
                    &dexep.issuer,
                    // The CLI requires an `aud` claim plus >=2 claims total; the
                    // `aud` value must equal the Dex OIDC client_id (`rise-backend`),
                    // which is the audience of the Dex id_token presented at exchange.
                    "--claim",
                    "aud=rise-backend",
                    "--claim",
                    "email=user@example.com",
                ],
                None,
            )?,
            "service-account create",
        )?;
        let sa_email = created
            .stdout
            .split_whitespace()
            .find(|t| t.ends_with("@sa.rise.local"))
            .context("could not find the SA email in `service-account create` output")?
            .to_string();
        eprintln!("[e2e] sa-token-exchange: assuming identity {sa_email}");

        // The proof: a real Dex OIDC token for the (non-admin) federated user.
        let oidc = dex::mint_password_token(dexep, "user@example.com", "password")
            .context("mint Dex OIDC token (password grant)")?;

        // Exchange: RISE_TOKEN=<dex id_token> + RISE_IDENTITY=<sa email>. The CLI's
        // ExchangingTokenSource POSTs /api/v1/auth/token; the access token then
        // carries the SA principal, so `project list` returns the SA's project.
        let listed = expect_ok(
            b.rise_cli(
                &["project", "list"],
                Some(CliAuth {
                    token: &oidc,
                    identity: Some(&sa_email),
                }),
            )?,
            "project list via SA token exchange",
        )?;
        anyhow::ensure!(
            listed.combined().contains(&project),
            "SA's project '{project}' not in `project list` output:\n{}",
            listed.combined()
        );

        // Negative: the same OIDC token WITHOUT RISE_IDENTITY is passed through
        // un-exchanged and must be rejected (an external token can't `project list`).
        let raw = b.rise_cli(
            &["project", "list"],
            Some(CliAuth {
                token: &oidc,
                identity: None,
            }),
        )?;
        anyhow::ensure!(
            !raw.success(),
            "expected the un-exchanged external token to be rejected, but it succeeded:\n{}",
            raw.combined()
        );
        Ok(())
    }
}

// ---- (d) Loki log retention -----------------------------------------------

struct LokiLogRetention;

impl Scenario for LokiLogRetention {
    fn id(&self) -> &'static str {
        "loki-log-retention"
    }

    fn applies_to(&self, b: &dyn Backend) -> Applicability {
        match b.kind() {
            BackendKind::Minikube => Applicability::Run,
            BackendKind::Docker => {
                Applicability::Skip("the docker compose stack has no Loki log backend")
            }
        }
    }

    fn run(&self, b: &dyn Backend) -> Result<()> {
        let project = unique("e2e-loki");
        let app = b.sample_app();
        expect_ok(
            b.rise_cli(
                &[
                    "project",
                    "create",
                    &project,
                    "--access-class",
                    "public",
                    "--no-rise-toml",
                ],
                None,
            )?,
            "project create",
        )?;
        expect_ok(
            b.rise_cli(
                &[
                    "deploy",
                    "--project",
                    &project,
                    "--image",
                    app.image,
                    "--http-port",
                    app.http_port,
                    "--replicas",
                    "1",
                ],
                None,
            )?,
            "deploy",
        )?;
        b.wait_healthy(&project)?;
        // Generate an access-log line, then give the log agent a window to scrape
        // before the pod is removed.
        let _ = b.reach_app(&project, "/")?;
        std::thread::sleep(std::time::Duration::from_secs(5));

        // Resolve the latest deployment id via the API.
        let deployments = b.api_get(&format!("/api/v1/projects/{project}/deployments"))?;
        anyhow::ensure!(
            deployments.status == 200,
            "deployments API returned {} :\n{}",
            deployments.status,
            deployments.body
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&deployments.body).context("parse deployments response")?;
        let deployment_id = parsed
            .get(0)
            .and_then(|d| d.get("deployment_id"))
            .and_then(|v| v.as_str())
            .context("no deployment_id in deployments response")?
            .to_string();

        // Stop the deployment and wait for the workload to actually be gone — with
        // the pod removed, logs can only come from Loki (not live kubelet).
        expect_ok(
            b.rise_cli(
                &[
                    "deployment",
                    "stop",
                    "--project",
                    &project,
                    "--group",
                    "default",
                ],
                None,
            )?,
            "deployment stop",
        )?;
        b.wait_workload_removed(&project)?;

        // Query the log-volume API over a window around now; retry while Loki
        // finishes ingesting.
        let now = chrono::Utc::now();
        let start = (now - chrono::Duration::minutes(10))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let end =
            (now + chrono::Duration::minutes(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let volume_path = format!(
            "/api/v1/projects/{project}/deployments/{deployment_id}/logs/volume\
             ?start={start}&end={end}&step_seconds=60"
        );
        let mut total: i64 = 0;
        let mut levels: usize = 0;
        let mut last = String::new();
        for _ in 0..30 {
            let resp = b.api_get(&volume_path)?;
            last = resp.body.clone();
            if resp.status == 200 {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&resp.body) {
                    let buckets = v["buckets"].as_array().cloned().unwrap_or_default();
                    total = buckets.iter().filter_map(|b| b["total"].as_i64()).sum();
                    let mut keys = std::collections::BTreeSet::new();
                    for bucket in &buckets {
                        if let Some(by_level) = bucket["by_level"].as_object() {
                            keys.extend(by_level.keys().cloned());
                        }
                    }
                    levels = keys.len();
                    if total > 0 && levels >= 1 {
                        break;
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(3));
        }
        anyhow::ensure!(
            total > 0,
            "expected /logs/volume total>0 after pod removal; last response:\n{last}"
        );
        anyhow::ensure!(
            levels >= 1,
            "expected /logs/volume to report >=1 level; last response:\n{last}"
        );

        // The SSE log stream (non-follow) must still return backlog from Loki.
        let logs = expect_ok(
            b.rise_cli(
                &[
                    "deployment",
                    "logs",
                    "--project",
                    &project,
                    &deployment_id,
                    "--tail",
                    "20",
                ],
                None,
            )?,
            "deployment logs",
        )?;
        anyhow::ensure!(
            !logs.stdout.trim().is_empty(),
            "expected `rise deployment logs` to print >=1 line after pod removal"
        );
        Ok(())
    }
}

// ---- (e) helm idempotency -------------------------------------------------

struct HelmIdempotency;

impl Scenario for HelmIdempotency {
    fn id(&self) -> &'static str {
        "helm-idempotency"
    }

    fn applies_to(&self, b: &dyn Backend) -> Applicability {
        match b.kind() {
            BackendKind::Minikube => Applicability::Run,
            BackendKind::Docker => Applicability::Skip("docker backend has no Helm release"),
        }
    }

    fn run(&self, b: &dyn Backend) -> Result<()> {
        // Re-applying the chart must succeed (no immutable-field / diff errors).
        b.reapply_chart()
    }
}

// ---- (b) workload identity (jfrog-vault registry mode only) ----------------

struct WorkloadIdentity;

impl Scenario for WorkloadIdentity {
    fn id(&self) -> &'static str {
        "workload-identity"
    }

    fn applies_to(&self, b: &dyn Backend) -> Applicability {
        // Builds the fixture from source, which needs a registry the runtime can
        // pull from: the host docker daemon (Docker) or minikube's jfrog-vault mode.
        if b.supports_source_build() {
            Applicability::Run
        } else {
            Applicability::Skip("requires a source-build registry (minikube jfrog-vault mode)")
        }
    }

    fn run(&self, b: &dyn Backend) -> Result<()> {
        // Docker recreates the backend with the identity compose overlay (short TTL
        // + scoped registry); minikube's values-ci already configures it (no-op).
        b.prepare_workload_identity()?;
        let project = unique("e2e-id");
        expect_ok(
            b.rise_cli(
                &[
                    "project",
                    "create",
                    &project,
                    "--access-class",
                    "public",
                    "--no-rise-toml",
                ],
                None,
            )?,
            "project create",
        )?;
        // Build & deploy the identity fixture from source (needs the docker socket).
        expect_ok(
            b.rise_cli_build(
                &[
                    "deploy",
                    "--project",
                    &project,
                    "--backend",
                    "docker:build",
                    "--container-cli",
                    "docker",
                    "--http-port",
                    "8080",
                    "--replicas",
                    "1",
                    "tests/e2e-identity-fixture",
                ],
                None,
            )?,
            "deploy identity fixture",
        )?;
        b.wait_healthy(&project)?;

        // The fixture verifies the exchange + JWKS *inside* the container and reports
        // JSON. file=e2e, file audience=rise-e2e-audience, exchange audience=rise-e2e-exchange.
        let resp = b
            .reach_app(&project, "/identity?file=e2e&audience=rise-e2e-exchange")?
            .context("identity fixture not reachable")?;
        anyhow::ensure!(resp.status == 200, "/identity returned {}", resp.status);
        let id: serde_json::Value =
            serde_json::from_str(&resp.body).context("parse /identity response")?;

        let sub_prefix = format!("rise:proj:{project}:env:");
        let starts_with = |v: &serde_json::Value, p: &str| -> bool {
            v.as_str().map(|s| s.starts_with(p)).unwrap_or(false)
        };
        let strip_slash = |v: &serde_json::Value| -> String {
            v.as_str().unwrap_or("").trim_end_matches('/').to_string()
        };
        anyhow::ensure!(
            id["credential_present"] == serde_json::json!(true),
            "credential not present:\n{}",
            resp.body
        );
        anyhow::ensure!(
            id["file_token"]["present"] == serde_json::json!(true),
            "file token absent:\n{}",
            resp.body
        );
        anyhow::ensure!(
            id["file_token"]["signature_valid"] == serde_json::json!(true),
            "file token sig invalid:\n{}",
            resp.body
        );
        anyhow::ensure!(
            id["file_token"]["claims"]["aud"] == serde_json::json!("rise-e2e-audience"),
            "file token aud mismatch:\n{}",
            resp.body
        );
        anyhow::ensure!(
            id["exchanged_token"]["signature_valid"] == serde_json::json!(true),
            "exchanged token sig invalid:\n{}",
            resp.body
        );
        anyhow::ensure!(
            id["exchanged_token"]["claims"]["aud"] == serde_json::json!("rise-e2e-exchange"),
            "exchanged token aud mismatch:\n{}",
            resp.body
        );
        anyhow::ensure!(
            starts_with(&id["exchanged_token"]["claims"]["sub"], &sub_prefix),
            "exchanged sub not project-bound:\n{}",
            resp.body
        );
        anyhow::ensure!(
            starts_with(&id["file_token"]["claims"]["sub"], &sub_prefix),
            "file sub not project-bound:\n{}",
            resp.body
        );
        anyhow::ensure!(
            !strip_slash(&id["exchanged_token"]["claims"]["iss"]).is_empty()
                && strip_slash(&id["exchanged_token"]["claims"]["iss"])
                    == strip_slash(&id["issuer"]),
            "exchanged iss != fixture issuer:\n{}",
            resp.body
        );

        // Prove the controller re-mints the file token in place: poll until a new,
        // still-valid jti appears (short identity_token_ttl_seconds in values-ci).
        let first_jti = id["file_token"]["claims"]["jti"]
            .as_str()
            .context("file token has no jti")?
            .to_string();
        let mut refreshed = false;
        for _ in 0..36 {
            std::thread::sleep(std::time::Duration::from_secs(5));
            let Some(r) = b.reach_app(&project, "/identity?file=e2e")? else {
                continue;
            };
            let Ok(j) = serde_json::from_str::<serde_json::Value>(&r.body) else {
                continue;
            };
            let cur = j["file_token"]["claims"]["jti"].as_str().unwrap_or("");
            if !cur.is_empty() && cur != first_jti {
                anyhow::ensure!(
                    j["file_token"]["signature_valid"] == serde_json::json!(true),
                    "token rotated (jti={cur}) but signature invalid"
                );
                refreshed = true;
                break;
            }
        }
        anyhow::ensure!(
            refreshed,
            "file token did not refresh (jti stayed {first_jti})"
        );
        Ok(())
    }
}

// ---- (f) private project / Traefik forwardAuth (docker only) ----------------

struct PrivateForwardAuth;

impl Scenario for PrivateForwardAuth {
    fn id(&self) -> &'static str {
        "private-forward-auth"
    }

    fn applies_to(&self, b: &dyn Backend) -> Applicability {
        match b.kind() {
            // Traefik forwardAuth is the docker ingress-auth mechanism; K8s uses
            // nginx auth annotations (a separate path, not covered here).
            BackendKind::Docker => Applicability::Run,
            BackendKind::Minikube => {
                Applicability::Skip("forwardAuth is a Traefik/docker-specific mechanism")
            }
        }
    }

    fn run(&self, b: &dyn Backend) -> Result<()> {
        let traefik = b.traefik_base().context("backend has no Traefik ingress")?;
        let project = unique("e2e-priv");
        let app = b.sample_app();
        let host = format!("{project}.rise.localhost");

        expect_ok(
            b.rise_cli(
                &[
                    "project",
                    "create",
                    &project,
                    "--access-class",
                    "private",
                    "--no-rise-toml",
                ],
                None,
            )?,
            "project create",
        )?;
        expect_ok(
            b.rise_cli(
                &[
                    "deploy",
                    "--project",
                    &project,
                    "--image",
                    app.image,
                    "--http-port",
                    app.http_port,
                    "--replicas",
                    "1",
                ],
                None,
            )?,
            "deploy",
        )?;
        b.wait_healthy(&project)?;

        // The routable container can lag the API's Healthy mark; poll until the
        // forwardAuth + router-middlewares labels are stamped.
        let mut labels = String::new();
        for _ in 0..30 {
            labels = b.app_container_labels(&project)?;
            if labels.contains("forwardauth.address")
                && labels.contains(".routers.")
                && labels.contains(".middlewares")
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
        anyhow::ensure!(
            labels.contains("forwardauth.address"),
            "private app container missing forwardAuth labels:\n{labels}"
        );
        anyhow::ensure!(
            labels.contains(".routers.") && labels.contains(".middlewares"),
            "private app router missing the middlewares label:\n{labels}"
        );

        // Unauthenticated must be a same-host 302 to the project's /.rise signin page.
        let mut code = 0;
        let mut location = None;
        for _ in 0..30 {
            let (c, loc) = http::get_no_redirect(&format!("{traefik}/"), Some(&host))?;
            if c != 404 {
                code = c;
                location = loc;
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
        anyhow::ensure!(
            code == 302,
            "expected 302 for unauthenticated private request, got {code}"
        );
        let location = location.context("302 had no Location header")?;
        anyhow::ensure!(
            location.contains(&format!("//{host}/.rise/auth/signin")),
            "redirect not to same-host signin page: {location}"
        );
        anyhow::ensure!(
            location.contains(&format!("project={project}")),
            "redirect Location missing project={project}: {location}"
        );

        // /.rise/* on the app host is served by the BACKEND (200, not the app).
        let mut signin =
            b.ingress_get_once(&project, &format!("/.rise/auth/signin?project={project}"))?;
        for _ in 0..30 {
            if signin.status != 404 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
            signin =
                b.ingress_get_once(&project, &format!("/.rise/auth/signin?project={project}"))?;
        }
        anyhow::ensure!(
            signin.status == 200,
            "expected 200 for backend-served /.rise/auth/signin, got {}",
            signin.status
        );
        if let Some(marker) = app.body_marker {
            anyhow::ensure!(
                !signin.body.contains(marker),
                "/.rise path was served by the app, not the backend:\n{}",
                signin.body
            );
        }

        // Authenticated (rise_jwt cookie) is allowed through to the app.
        let authed = http::get_with_cookie(
            &format!("{traefik}/"),
            &host,
            &format!("rise_jwt={}", b.ci_bearer()),
        )?;
        anyhow::ensure!(
            authed.status == 200,
            "expected 200 for authed private request, got {}",
            authed.status
        );
        if let Some(marker) = app.body_marker {
            anyhow::ensure!(
                authed.body.contains(marker),
                "authenticated private response not from the app:\n{}",
                authed.body
            );
        }
        Ok(())
    }
}
