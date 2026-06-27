//! In-place upgrade suite. Stands the stack up at an older stable Rise version
//! (`RISE_E2E_UPGRADE_FROM`), seeds a project + deployment, upgrades the control
//! plane to the target version (`RISE_IMAGE_TAG`) — which runs its DB migrations
//! against the seeded data — then asserts the seeded project survived and the
//! upgraded control plane still deploys end to end.
//!
//! Backend fidelity differs by what each backend can faithfully version:
//!   - Docker recreates the `rise` service on the new image against the existing
//!     Postgres volume (image + DB-migration upgrade), keeping the in-repo compose
//!     topology.
//!   - Kubernetes builds the OLD chart from its source at the release tag (with
//!     that version's own values-ci.yaml), then `helm upgrade`s to the in-repo
//!     chart on the new image (a full chart + image + DB-migration upgrade).

use anyhow::{Context, Result};

use crate::backend::{Backend, SampleApp};
use crate::report;
use crate::scenario::{expect_ok, unique};

/// What `seed` created on the old version, for `verify` to check after upgrade.
struct Seeded {
    project: String,
}

/// Run the full upgrade suite against an already brought-up (old-version) backend.
/// `from` is the old version tag, for logging only.
pub fn run(b: &mut dyn Backend, from: &str) -> Result<()> {
    report::section(&format!(
        "Upgrade suite (backend = {}, from {from})",
        b.name()
    ));

    let seeded = seed(&*b)?;

    report::section("Upgrading the control plane in place");
    let up = std::time::Instant::now();
    b.upgrade().context("in-place upgrade")?;
    report::note(&format!(
        "upgrade complete ({})",
        report::human(up.elapsed())
    ));

    verify(&*b, &seeded)?;

    report::section("Upgrade suite passed");
    Ok(())
}

/// On the OLD version: create a project, deploy the sample app, confirm it's
/// healthy and reachable, and confirm the deployment is persisted.
fn seed(b: &dyn Backend) -> Result<Seeded> {
    let project = unique("e2e-upg");
    let app = b.sample_app();
    report::note(&format!("seeding project '{project}' on the old version"));

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
        "project create (old version)",
    )?;
    deploy_sample(b, &app, &project, "old version")?;
    b.wait_healthy(&project)?;
    assert_reachable(b, &app, &project, "pre-upgrade")?;

    // The deployment record must exist so we can prove it survives the migration.
    anyhow::ensure!(
        deployment_count(b, &project)? >= 1,
        "expected >=1 deployment for the seeded project before upgrade"
    );

    Ok(Seeded { project })
}

/// After the upgrade: the seeded project + its deployment history survived the
/// migration, its workload is still healthy and reachable, and a fresh deploy on
/// the upgraded control plane works end to end.
fn verify(b: &dyn Backend, seeded: &Seeded) -> Result<()> {
    report::note("verifying the seeded project survived the upgrade");

    // The project row migrated cleanly and is still listed.
    let listed = expect_ok(
        b.rise_cli(&["project", "list"], None)?,
        "project list (new version)",
    )?;
    anyhow::ensure!(
        listed.combined().contains(&seeded.project),
        "seeded project '{}' missing from `project list` after upgrade:\n{}",
        seeded.project,
        listed.combined()
    );

    // Its pre-upgrade deployment history survived, and the workload reconverges.
    anyhow::ensure!(
        deployment_count(b, &seeded.project)? >= 1,
        "seeded project '{}' lost its deployment history across the upgrade",
        seeded.project
    );
    b.wait_healthy(&seeded.project)?;
    let app = b.sample_app();
    assert_reachable(b, &app, &seeded.project, "post-upgrade")?;

    // A fresh deploy on the upgraded control plane works end to end.
    report::note("deploying a fresh project on the upgraded version");
    let fresh = unique("e2e-upg-new");
    expect_ok(
        b.rise_cli(
            &[
                "project",
                "create",
                &fresh,
                "--access-class",
                "public",
                "--no-rise-toml",
            ],
            None,
        )?,
        "project create (post-upgrade)",
    )?;
    deploy_sample(b, &app, &fresh, "post-upgrade")?;
    b.wait_healthy(&fresh)?;
    assert_reachable(b, &app, &fresh, "fresh post-upgrade")?;

    Ok(())
}

/// `rise deploy --image <app> --http-port <port> --replicas 1` for `project`.
fn deploy_sample(b: &dyn Backend, app: &SampleApp, project: &str, phase: &str) -> Result<()> {
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
        &format!("deploy ({phase})"),
    )?;
    Ok(())
}

/// Number of deployments the API reports for `project`.
fn deployment_count(b: &dyn Backend, project: &str) -> Result<usize> {
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

/// Assert the app answers 200 (and carries its body marker) through the ingress,
/// or log a declared gap when app-HTTP reach isn't wired for the backend.
fn assert_reachable(b: &dyn Backend, app: &SampleApp, project: &str, phase: &str) -> Result<()> {
    match b.reach_app(project, "/")? {
        Some(resp) => {
            anyhow::ensure!(
                resp.status == 200,
                "{phase}: expected 200 from app, got {}",
                resp.status
            );
            if let Some(marker) = app.body_marker {
                anyhow::ensure!(
                    resp.body.contains(marker),
                    "{phase}: response body missing expected marker {marker:?}:\n{}",
                    resp.body
                );
            }
        }
        None => eprintln!(
            "[e2e] upgrade {phase}: app-HTTP reach not wired for {} — asserted via Healthy only",
            b.name()
        ),
    }
    Ok(())
}
