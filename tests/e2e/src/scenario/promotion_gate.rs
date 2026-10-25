//! Release promotion (ADR-0007): an environment with a promotion source only
//! accepts deployments promoted from that environment, and `rise promote`
//! carries a deployment across.

use anyhow::{Context, Result};

use super::{create_public_project, expect_ok, unique, Applicability, Scenario};
use crate::backend::{Backend, CliAuth};
use crate::dex;

/// Deploy to `staging`, make `production` accept only promotions from
/// `staging`, show a fresh deploy to `production` by a non-admin is refused,
/// then `rise promote` the staging deployment and check its provenance.
pub(super) struct PromotionGate;

impl PromotionGate {
    /// `deployment_id` of the project's deployment in `environment`.
    fn deployment_in(b: &dyn Backend, project: &str, environment: &str) -> Result<String> {
        let resp = b.api_get(&format!("/api/v1/projects/{project}/deployments"))?;
        anyhow::ensure!(
            resp.status == 200,
            "listing deployments returned {}:\n{}",
            resp.status,
            resp.body
        );
        let list: Vec<serde_json::Value> =
            serde_json::from_str(&resp.body).context("parse deployments")?;
        list.iter()
            .find(|d| d["environment"].as_str() == Some(environment))
            .and_then(|d| d["deployment_id"].as_str())
            .map(str::to_string)
            .with_context(|| format!("no {environment} deployment in {}", resp.body))
    }

    /// A non-admin principal for the project: a service account assumed with
    /// a Dex token for `user@example.com`. Returns `(oidc token, SA email)`.
    fn service_account(b: &dyn Backend, project: &str) -> Result<(String, String)> {
        let dexep = b.dex().context("backend exposes no reachable Dex")?;
        expect_ok(
            b.rise_cli(
                &[
                    "service-account",
                    "create",
                    "--project",
                    project,
                    "--issuer",
                    &dexep.issuer,
                    "--claim",
                    "aud=rise-backend",
                    "--claim",
                    "email=user@example.com",
                ],
                None,
            )?,
            "service-account create",
        )?;
        let sas = b.api_get(&format!("/api/v1/projects/{project}/service-accounts"))?;
        anyhow::ensure!(
            sas.status == 200,
            "list service-accounts returned {}:\n{}",
            sas.status,
            sas.body
        );
        let email = serde_json::from_str::<serde_json::Value>(&sas.body)
            .context("parse service-accounts response")?["service_accounts"][0]["email"]
            .as_str()
            .context("no service account email in API response")?
            .to_string();
        let oidc = dex::mint_password_token(dexep, "user@example.com", "password")
            .context("mint Dex OIDC token (password grant)")?;
        Ok((oidc, email))
    }
}

impl Scenario for PromotionGate {
    fn id(&self) -> &'static str {
        "promotion-gate"
    }

    fn applies_to(&self, _b: &dyn Backend) -> Applicability {
        // Control-plane only: the gate and the promotion are decided before
        // any backend sees the deployment.
        Applicability::Run
    }

    fn run(&self, b: &dyn Backend) -> Result<()> {
        let project = unique("e2e-promo");
        let app = b.sample_app();
        create_public_project(b, &project)?;
        expect_ok(
            b.rise_cli(
                &[
                    "environment",
                    "create",
                    "staging",
                    "-p",
                    &project,
                    "--group",
                    "staging",
                ],
                None,
            )?,
            "environment create staging",
        )?;

        // 1. A staging deployment to promote.
        expect_ok(
            b.rise_cli(
                &[
                    "deploy",
                    "--project",
                    &project,
                    "-E",
                    "staging",
                    "--image",
                    app.image,
                    "--http-port",
                    app.http_port,
                    "--replicas",
                    "1",
                ],
                None,
            )?,
            "staging deploy",
        )?;
        b.wait_healthy(&project)?;
        let staging_id = Self::deployment_in(b, &project, "staging")?;

        // 2. As the admin CI bearer: production only accepts promotions
        //    from staging.
        expect_ok(
            b.rise_cli(
                &[
                    "environment",
                    "update",
                    "production",
                    "-p",
                    &project,
                    "--promote-from",
                    "staging",
                ],
                None,
            )?,
            "environment update --promote-from",
        )?;

        // 3. A fresh deploy to production by a non-admin is refused. A
        //    service account never gets the administrator bypass.
        let (oidc, sa_email) = Self::service_account(b, &project)?;
        let refused = b.rise_cli(
            &[
                "deploy",
                "--project",
                &project,
                "-E",
                "production",
                "--image",
                app.image,
                "--http-port",
                app.http_port,
                "--replicas",
                "1",
            ],
            Some(CliAuth {
                token: &oidc,
                identity: Some(&sa_email),
            }),
        )?;
        anyhow::ensure!(
            !refused.success() && refused.combined().contains("promotion_source_required"),
            "a fresh production deploy was not refused by the promotion gate:\n{}",
            refused.combined()
        );

        // 4. Promoting the staging deployment is accepted and becomes healthy.
        let promoted = expect_ok(
            b.rise_cli(
                &[
                    "promote",
                    &staging_id,
                    "-p",
                    &project,
                    "-E",
                    "production",
                    "--yes",
                    "--no-config",
                    "--json",
                ],
                None,
            )?,
            "rise promote",
        )?;
        let output: serde_json::Value = serde_json::from_str(promoted.stdout.trim())
            .with_context(|| format!("parse `rise promote --json` output:\n{}", promoted.stdout))?;
        anyhow::ensure!(
            output["status"] == "Healthy",
            "the promoted deployment did not become Healthy:\n{output}"
        );
        let promoted_id = output["deployment_id"]
            .as_str()
            .context("`rise promote --json` printed no deployment_id")?
            .to_string();

        // 5. Its provenance names the promotion.
        let shown = expect_ok(
            b.rise_cli(
                &["deployment", "show", &promoted_id, "-p", &project, "--json"],
                None,
            )?,
            "deployment show --json",
        )?;
        let shown: serde_json::Value = serde_json::from_str(shown.stdout.trim())
            .with_context(|| format!("parse `deployment show --json`:\n{}", shown.stdout))?;
        anyhow::ensure!(
            shown["created_via"] == "promote"
                && shown["promoted_from_environment"] == "staging"
                && shown["promoted_from_deployment_id"].as_str() == Some(staging_id.as_str())
                && shown["environment"] == "production",
            "the promoted deployment's provenance is wrong:\n{shown}"
        );
        Ok(())
    }
}
