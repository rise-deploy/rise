//! Backend-agnostic scenarios. Each is written once against `dyn Backend` and
//! declares which backends it `applies_to`; an unsupported combo is a logged
//! `Skip(reason)` — a visible parity gap, never silent drift.

use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result};

use crate::backend::{Backend, BackendKind, CliAuth};
use crate::cli::CliOutput;
use crate::dex;

pub enum Applicability {
    Run,
    Skip(&'static str),
}

pub trait Scenario {
    fn id(&self) -> &'static str;
    fn applies_to(&self, kind: BackendKind) -> Applicability;
    fn run(&self, b: &dyn Backend) -> Result<()>;
}

/// The scenarios in this increment. Workload-identity, health-rolling cutover,
/// private/forwardAuth and Loki retention are tracked as follow-ups (ROADMAP).
pub fn all() -> Vec<Box<dyn Scenario>> {
    vec![Box::new(PublicDeploy), Box::new(SaTokenExchange)]
}

/// Run every scenario applicable to `b`, printing RUN/PASS/FAIL/SKIP lines, and
/// fail if any applicable scenario failed.
pub fn run_all(b: &dyn Backend) -> Result<()> {
    let mut failed: Vec<&'static str> = Vec::new();
    for s in all() {
        match s.applies_to(b.kind()) {
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

    fn applies_to(&self, _kind: BackendKind) -> Applicability {
        Applicability::Run
    }

    fn run(&self, b: &dyn Backend) -> Result<()> {
        let project = unique("e2e-pub");
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
                    "traefik/whoami",
                    "--http-port",
                    "80",
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
                if b.kind() == BackendKind::Docker {
                    anyhow::ensure!(
                        resp.body.contains("Hostname:"),
                        "response did not look like whoami output:\n{}",
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

    fn applies_to(&self, _kind: BackendKind) -> Applicability {
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
