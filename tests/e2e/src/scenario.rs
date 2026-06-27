//! Backend-agnostic scenarios. Each is written once against `dyn Backend` and
//! declares which backends it `applies_to`; an unsupported combo is a logged
//! `Skip(reason)` — a visible parity gap, never silent drift.

use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::backend::{Backend, BackendKind, CliAuth, SampleApp};
use crate::cli::CliOutput;
use crate::dex;
use crate::http;
use crate::report;

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
        Box::new(PrivateIngressAuth),
        Box::new(HealthRollingCutover),
        Box::new(LokiLogRetention),
        Box::new(HelmIdempotency),
        // Last: on Docker it recreates the backend with the identity overlay, so
        // it must run after the scenarios that expect the default config.
        Box::new(WorkloadIdentity),
    ]
}

/// Run every scenario applicable to `b` (all share the one backend `bring_up`, so
/// they run as one suite, in order), reporting each with its duration plus a
/// final summary; fail if any applicable scenario failed.
pub fn run_all(b: &dyn Backend) -> Result<()> {
    report::section(&format!("Scenarios (backend = {})", b.name()));
    let suite_start = std::time::Instant::now();
    let mut passed = 0u32;
    let mut skipped = 0u32;
    let mut failed: Vec<&'static str> = Vec::new();

    for s in all() {
        match s.applies_to(b) {
            Applicability::Skip(reason) => {
                skipped += 1;
                report::note(&format!("SKIP {:.<34} {reason}", s.id()));
            }
            Applicability::Run => {
                report::note(&format!("RUN  {}", s.id()));
                let start = std::time::Instant::now();
                let result = s.run(b);
                let took = report::human(start.elapsed());
                match result {
                    Ok(()) => {
                        passed += 1;
                        report::note(&format!("PASS {:.<34} ({took})", s.id()));
                    }
                    Err(e) => {
                        failed.push(s.id());
                        report::note(&format!("FAIL {:.<34} ({took})", s.id()));
                        eprintln!("         {e:#}");
                    }
                }
            }
        }
    }

    report::section(&format!(
        "Summary: {passed} passed, {} failed, {skipped} skipped in {}",
        failed.len(),
        report::human(suite_start.elapsed())
    ));
    anyhow::ensure!(failed.is_empty(), "scenarios failed: {failed:?}");
    Ok(())
}

/// Unique-enough project name per run (DNS-safe), so a stale stack can't collide.
pub(crate) fn unique(prefix: &str) -> String {
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

pub(crate) fn expect_ok(out: CliOutput, what: &str) -> Result<CliOutput> {
    anyhow::ensure!(
        out.success(),
        "{what} failed (exit {:?}):\n{}",
        out.status,
        out.combined()
    );
    Ok(out)
}

/// Create a public-access project with no rise.toml side effects. Shared by the
/// scenarios and the upgrade suite.
pub(crate) fn create_public_project(b: &dyn Backend, project: &str) -> Result<()> {
    expect_ok(
        b.rise_cli(
            &[
                "project",
                "create",
                project,
                "--access-class",
                "public",
                "--no-rise-toml",
            ],
            None,
        )?,
        "project create",
    )?;
    Ok(())
}

/// Deploy a prebuilt image at one replica for `project`.
pub(crate) fn deploy_image(b: &dyn Backend, project: &str, app: &SampleApp) -> Result<()> {
    expect_ok(
        b.rise_cli(
            &[
                "deploy",
                "--project",
                project,
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
    Ok(())
}

/// Assert the app answers 200 (and carries its body marker, if any) through the
/// backend's reach path, or log a declared gap when app-HTTP reach isn't wired.
pub(crate) fn assert_app_reachable(b: &dyn Backend, app: &SampleApp, project: &str) -> Result<()> {
    match b.reach_app(project, "/")? {
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
            "[e2e] app-HTTP reach not wired for {} — asserted via Healthy only",
            b.name()
        ),
    }
    Ok(())
}

/// Number of deployments the API reports for `project`.
pub(crate) fn deployment_count(b: &dyn Backend, project: &str) -> Result<usize> {
    let resp = b.api_get(&format!("/api/v1/projects/{project}/deployments"))?;
    anyhow::ensure!(
        resp.status == 200,
        "deployments API returned {} :\n{}",
        resp.status,
        resp.body
    );
    let parsed: serde_json::Value =
        serde_json::from_str(&resp.body).context("parse deployments response")?;
    Ok(parsed.as_array().map(|a| a.len()).unwrap_or(0))
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
        create_public_project(b, &project)?;
        deploy_image(b, &project, &app)?;
        b.wait_healthy(&project)?;
        assert_app_reachable(b, &app, &project)?;
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

        create_public_project(b, &project)?;

        // Register an SA on this project trusting Dex, keyed on the email claim.
        expect_ok(
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
        // Read the SA's synthetic email from the API, not the human CLI output.
        let sas = b.api_get(&format!("/api/v1/projects/{project}/service-accounts"))?;
        anyhow::ensure!(
            sas.status == 200,
            "list service-accounts API returned {} :\n{}",
            sas.status,
            sas.body
        );
        let sa_email = serde_json::from_str::<serde_json::Value>(&sas.body)
            .context("parse service-accounts response")?["service_accounts"][0]["email"]
            .as_str()
            .context("no service account email in API response")?
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
        create_public_project(b, &project)?;
        deploy_image(b, &project, &app)?;
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
        create_public_project(b, &project)?;
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

        // The controller re-mints the file token in place at half its (short) TTL
        // (identity_token_ttl_seconds in values-ci). Assert this in two stages so a
        // failure pinpoints *where* it broke:
        //   1. the controller re-mints at the source it writes (the K8s Secret) —
        //      deterministic, independent of in-pod mount propagation;
        //   2. the pod's mounted file then reflects the new token — exercising the
        //      Kubernetes secret-volume propagation deployed apps depend on.
        // On a backend with no source distinct from the mounted file (Docker
        // bind-mounts the controller's file directly), stage 1 is a no-op
        // (`minted_token_jti` → None) and stage 2 alone proves re-minting.
        let first_jti = id["file_token"]["claims"]["jti"]
            .as_str()
            .context("file token has no jti")?
            .to_string();
        let first_exp = id["file_token"]["claims"]["exp"].as_i64();

        // Stage 1: controller re-mints the Secret (the source).
        if let Some(secret_first) = b.minted_token_jti(&project, "e2e")? {
            http::poll(
                std::time::Duration::from_secs(180),
                std::time::Duration::from_secs(5),
                "controller to re-mint the identity Secret",
                || {
                    Ok(b.minted_token_jti(&project, "e2e")?
                        .is_some_and(|j| j != secret_first))
                },
            )
            .with_context(|| {
                format!(
                    "controller did not re-mint the identity Secret within 180s \
                     (jti stuck at {secret_first}) — the controller/reconcile re-mint, \
                     not in-pod propagation, is the problem"
                )
            })?;
        }

        // Stage 2: the pod's mounted file reflects a re-minted, still-valid token.
        let refreshed = b.poll_app(
            &project,
            "/identity?file=e2e",
            std::time::Duration::from_secs(360),
            std::time::Duration::from_secs(5),
            &mut |body| {
                serde_json::from_str::<serde_json::Value>(body).is_ok_and(|j| {
                    let cur = j["file_token"]["claims"]["jti"].as_str().unwrap_or("");
                    let cur_exp = j["file_token"]["claims"]["exp"].as_i64();
                    // A genuine re-mint changes the jti AND advances the expiry. Gating
                    // on jti alone would accept a token that reused the old `exp`
                    // (an already-near-expired re-mint); require exp to move forward
                    // too (when both are present).
                    let exp_advanced = match (cur_exp, first_exp) {
                        (Some(c), Some(f)) => c > f,
                        _ => true,
                    };
                    !cur.is_empty()
                        && cur != first_jti
                        && exp_advanced
                        && j["file_token"]["signature_valid"] == serde_json::json!(true)
                })
            },
        )?;
        if !refreshed {
            // Decisive diagnostic: did the controller re-mint at all? If `exp`
            // advanced, the token IS rotating (slow projection); if it's static, the
            // controller never re-minted (a server/config issue, not propagation lag).
            // Best-effort: a failing diagnostic reach must not mask the jti/exp
            // bail! below, which is the actual signal we want on failure.
            let last = b
                .reach_app(&project, "/identity?file=e2e")
                .ok()
                .flatten()
                .map(|r| r.body)
                .unwrap_or_default();
            let lv: serde_json::Value =
                serde_json::from_str(&last).unwrap_or(serde_json::Value::Null);
            let last_jti = lv["file_token"]["claims"]["jti"].as_str().unwrap_or("?");
            let last_exp = lv["file_token"]["claims"]["exp"].as_i64();
            anyhow::bail!(
                "file token did not refresh in 360s — first(jti={first_jti}, exp={first_exp:?}) \
                 last(jti={last_jti}, exp={last_exp:?})"
            );
        }
        Ok(())
    }
}

// ---- (f) private project / ingress auth (both backends) ---------------------

struct PrivateIngressAuth;

impl Scenario for PrivateIngressAuth {
    fn id(&self) -> &'static str {
        "private-ingress-auth"
    }

    fn applies_to(&self, _b: &dyn Backend) -> Applicability {
        // The ingress-auth contract — block unauthenticated, allow authenticated —
        // holds on both backends; only the wiring + reach differ, behind the trait
        // (Traefik forwardAuth labels on docker; nginx auth annotations on K8s).
        Applicability::Run
    }

    fn run(&self, b: &dyn Backend) -> Result<()> {
        let project = unique("e2e-priv");
        let app = b.sample_app();
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
        deploy_image(b, &project, &app)?;
        b.wait_healthy(&project)?;

        // Per-backend wiring check (Traefik forwardAuth labels / nginx annotations).
        b.assert_ingress_auth_configured(&project)?;

        // Unauthenticated → 302 to the project's signin page. Poll the ingress until
        // the route is wired (tolerate warmup 404/5xx/connection errors).
        let mut status = 0;
        let mut location = None;
        for _ in 0..45 {
            match b.ingress_get(&project, "/", false, None) {
                Ok(r) if r.status == 302 => {
                    status = r.status;
                    location = r.location;
                    break;
                }
                Ok(r) => status = r.status,
                Err(_) => {}
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
        anyhow::ensure!(
            status == 302,
            "expected 302 for unauthenticated private request, got {status}"
        );
        let location = location.context("302 had no Location header")?;
        anyhow::ensure!(
            location.contains("/.rise/auth/signin")
                && location.contains(&format!("project={project}")),
            "redirect not to the project's signin page: {location}"
        );

        // Authenticated (rise_jwt cookie) is allowed straight through to the app.
        // Do NOT follow redirects: a rejected cookie answers 302 -> signin (which
        // itself serves 200), so following would let an auth regression pass the
        // status check. A 200 here therefore means the ingress let us through.
        let cookie = format!("rise_jwt={}", b.ci_bearer());
        let authed = b.ingress_get(&project, "/", false, Some(&cookie))?;
        anyhow::ensure!(
            authed.status == 200,
            "expected 200 for authed private request, got {} (a redirect means the \
             cookie was rejected at the ingress)",
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

// ---- (g) health-rolling cutover (docker only) ------------------------------

/// Sanitize to a Traefik router/service base name (lowercase; non-alnum runs → `-`).
fn sanitize_router_name(s: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in s.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

struct HealthRollingCutover;

impl Scenario for HealthRollingCutover {
    fn id(&self) -> &'static str {
        "health-rolling-cutover"
    }

    fn applies_to(&self, b: &dyn Backend) -> Applicability {
        match b.kind() {
            // Reads Traefik serverStatus + per-container env; Traefik/docker-specific.
            BackendKind::Docker => Applicability::Run,
            BackendKind::Minikube => {
                Applicability::Skip("cutover probe reads the Traefik API (docker-specific)")
            }
        }
    }

    fn run(&self, b: &dyn Backend) -> Result<()> {
        let project = unique("e2e-cut");
        let app = b.sample_app();

        // A health_check needs a [containers.<name>] block (the --image path has
        // none), so deploy from a generated project dir.
        let dir = std::env::temp_dir().join(unique("rise-cut"));
        std::fs::create_dir_all(&dir).context("create cutover project dir")?;
        std::fs::write(
            dir.join("rise.toml"),
            format!(
                "[project]\nname = \"{project}\"\n\n[containers.app]\nimage = \"{}\"\nport = {}\nreplicas = 2\n\n[containers.app.deploy.health_check]\npath = \"/\"\n",
                app.image, app.http_port
            ),
        )
        .context("write cutover rise.toml")?;
        let dir = dir.to_string_lossy().to_string();

        create_public_project(b, &project)?;
        expect_ok(
            b.rise_cli(
                &["deploy", &dir, "--project", &project, "--env", "REV=1"],
                None,
            )?,
            "deploy REV=1",
        )?;
        b.wait_healthy(&project)?;

        // Read the authoritative group-scoped Traefik service name off the app
        // container's `traefik.http.services.<svc>.loadbalancer.server.port` label.
        let labels_json = b.app_container_labels(&project)?;
        let labels: serde_json::Value =
            serde_json::from_str(&labels_json).context("parse container labels")?;
        let svc = labels
            .as_object()
            .context("labels not an object")?
            .keys()
            .find_map(|k| {
                k.strip_prefix("traefik.http.services.")
                    .and_then(|r| r.strip_suffix(".loadbalancer.server.port"))
            })
            .context("no traefik service label on the app container")?
            .to_string();
        // Drift guard: readable base + an injective 16-hex suffix.
        let base = sanitize_router_name(&format!("{project}-default-app"));
        let suffix = svc
            .strip_prefix(&format!("{base}-"))
            .with_context(|| format!("service '{svc}' does not start with '{base}-'"))?;
        anyhow::ensure!(
            suffix.len() == 16
                && suffix
                    .chars()
                    .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "service '{svc}' suffix is not 16 lowercase hex chars"
        );

        // The serverStatus gate signal must exist and be correctly shaped (Traefik
        // v3 puts it at the top level, keyed by server URL → UP/DOWN).
        let mut status_ok = false;
        let mut last = String::new();
        for _ in 0..30 {
            let resp = b.traefik_api(&format!("/api/http/services/{svc}@docker"))?;
            last = resp.body.clone();
            if resp.status == 200 {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&resp.body) {
                    if let Some(ss) = v.get("serverStatus").and_then(|s| s.as_object()) {
                        let well_formed = !ss.is_empty()
                            && ss.iter().all(|(k, val)| {
                                k.starts_with("http://")
                                    && k.rsplit(':').next().is_some_and(|p| {
                                        !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())
                                    })
                                    && val.as_str().is_some_and(|s| {
                                        s.eq_ignore_ascii_case("UP")
                                            || s.eq_ignore_ascii_case("DOWN")
                                    })
                            });
                        let any_up = ss
                            .values()
                            .any(|v| v.as_str().is_some_and(|s| s.eq_ignore_ascii_case("UP")));
                        if well_formed && any_up {
                            status_ok = true;
                            break;
                        }
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
        anyhow::ensure!(
            status_ok,
            "Traefik API never exposed a valid non-empty serverStatus for {svc}@docker; last:\n{last}"
        );

        // Force a cutover (changed env → new revision) and assert NO 5xx gap across
        // the rollout. `rise deploy` FOLLOWS the new revision to a terminal state,
        // so the probe must run CONCURRENTLY with the deploy to span the cutover
        // window — a probe that started only after the deploy returned would sample
        // an already-converged stack and never witness a gap. A background thread
        // hammers the ingress (Host-routed GETs) and records the worst status seen.
        let base = b
            .traefik_base()
            .context("backend has no Traefik ingress")?
            .to_string();
        let url = format!("{base}/");
        let host = b.app_host(&project);
        let stop = Arc::new(AtomicBool::new(false));
        let worst = Arc::new(AtomicU16::new(0));
        let probe = {
            let (stop, worst) = (stop.clone(), worst.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if let Ok(r) = http::get(&url, Some(&host)) {
                        if r.status > worst.load(Ordering::Relaxed) {
                            worst.store(r.status, Ordering::Relaxed);
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            })
        };
        let deployed = b.rise_cli(
            &["deploy", &dir, "--project", &project, "--env", "REV=2"],
            None,
        );
        stop.store(true, Ordering::Relaxed);
        let _ = probe.join();
        expect_ok(deployed?, "deploy REV=2")?;
        let worst = worst.load(Ordering::Relaxed);
        anyhow::ensure!(
            worst < 500,
            "5xx gap during cutover — worst status observed through Traefik was {worst}"
        );
        b.wait_healthy(&project)?;

        // Assert REV=2 actually took over: every running app container carries REV=2
        // and none still carries REV=1 (the old replicas were retired).
        let mut took_over = false;
        for _ in 0..30 {
            let envs = b.app_container_envs(&project)?;
            if !envs.is_empty() {
                let saw_rev2 = envs.iter().any(|e| e.contains("\"REV=2\""));
                let saw_rev1 = envs.iter().any(|e| e.contains("\"REV=1\""));
                if saw_rev2 && !saw_rev1 {
                    took_over = true;
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
        anyhow::ensure!(
            took_over,
            "REV=2 did not become the sole active revision after cutover convergence"
        );
        Ok(())
    }
}
