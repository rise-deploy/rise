//! Release promotion (ADR-0007): an environment with a promotion source only
//! accepts deployments promoted from that environment, and `rise promote`
//! carries a deployment across.

use anyhow::{Context, Result};

use super::{create_public_project, expect_ok, unique, Applicability, Scenario};
use crate::backend::{Backend, CliAuth};
use crate::cli::CliOutput;
use crate::dex;

/// Deploy to `staging`, make `production` accept only promotions from
/// `staging`, show a fresh deploy to `production` by a non-admin is refused,
/// then `rise promote` the staging deployment and check its provenance.
pub(super) struct PromotionGate;

/// Run `rise` with a whitespace-separated command line (no argument here
/// contains spaces).
fn rise(b: &dyn Backend, line: &str, auth: Option<CliAuth<'_>>) -> Result<CliOutput> {
    b.rise_cli(&line.split_whitespace().collect::<Vec<_>>(), auth)
}

/// Run `rise` as the admin CI bearer, require success and parse stdout as JSON.
fn rise_json(b: &dyn Backend, line: &str) -> Result<serde_json::Value> {
    let out = expect_ok(rise(b, line, None)?, line)?;
    serde_json::from_str(out.stdout.trim())
        .with_context(|| format!("parse `rise {line}` output:\n{}", out.stdout))
}

/// GET an API path as the admin CI bearer and parse the JSON body.
fn api_json(b: &dyn Backend, path: &str) -> Result<serde_json::Value> {
    let resp = b.api_get(path)?;
    anyhow::ensure!(
        resp.status == 200,
        "GET {path} returned {}:\n{}",
        resp.status,
        resp.body
    );
    serde_json::from_str(&resp.body).with_context(|| format!("parse GET {path}"))
}

impl PromotionGate {
    /// A non-admin principal for the project: a service account assumed with
    /// a Dex token for `user@example.com`. Returns `(oidc token, SA email)`.
    fn service_account(b: &dyn Backend, project: &str) -> Result<(String, String)> {
        let dexep = b.dex().context("backend exposes no reachable Dex")?;
        let create = format!(
            "service-account create --project {project} --issuer {} \
             --claim aud=rise-backend --claim email=user@example.com",
            dexep.issuer
        );
        expect_ok(rise(b, &create, None)?, "service-account create")?;
        let sas = api_json(b, &format!("/api/v1/projects/{project}/service-accounts"))?;
        let email = sas["service_accounts"][0]["email"]
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
        let create_staging = format!("environment create staging -p {project} --group staging");
        expect_ok(rise(b, &create_staging, None)?, &create_staging)?;
        let deploy_to = |env: &str| {
            format!(
                "deploy --project {project} -E {env} --image {} --http-port {} --replicas 1",
                app.image, app.http_port
            )
        };

        // 1. A staging deployment to promote.
        expect_ok(rise(b, &deploy_to("staging"), None)?, "staging deploy")?;
        b.wait_healthy(&project)?;
        let deployments = api_json(b, &format!("/api/v1/projects/{project}/deployments"))?;
        let staging_id = deployments
            .as_array()
            .into_iter()
            .flatten()
            .find(|d| d["environment"] == "staging")
            .and_then(|d| d["deployment_id"].as_str())
            .with_context(|| format!("no staging deployment in {deployments}"))?
            .to_string();

        // 2. As the admin CI bearer: production only accepts promotions
        //    from staging.
        let gate = format!("environment update production -p {project} --promote-from staging");
        expect_ok(rise(b, &gate, None)?, &gate)?;

        // 3. A fresh deploy to production by a non-admin is refused. A
        //    service account never gets the administrator bypass.
        let (oidc, sa_email) = Self::service_account(b, &project)?;
        let auth = CliAuth {
            token: &oidc,
            identity: Some(&sa_email),
        };
        let refused = rise(b, &deploy_to("production"), Some(auth))?;
        anyhow::ensure!(
            !refused.success() && refused.combined().contains("promotion_source_required"),
            "a fresh production deploy was not refused by the promotion gate:\n{}",
            refused.combined()
        );

        // 4. Promoting the staging deployment is accepted and becomes healthy.
        let output = rise_json(
            b,
            &format!("promote {staging_id} -p {project} -E production --yes --no-config --json"),
        )?;
        anyhow::ensure!(
            output["status"] == "Healthy",
            "the promoted deployment did not become Healthy:\n{output}"
        );
        let promoted_id = output["deployment_id"]
            .as_str()
            .context("`rise promote --json` printed no deployment_id")?;

        // 5. Its provenance names the promotion.
        let shown = rise_json(
            b,
            &format!("deployment show {promoted_id} -p {project} --json"),
        )?;
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
