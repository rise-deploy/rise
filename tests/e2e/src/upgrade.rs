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

use crate::backend::Backend;
use crate::report;
use crate::scenario::{
    assert_app_reachable, create_public_project, deploy_image, deployment_count, expect_ok, unique,
};

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

    create_public_project(b, &project).context("project create (old version)")?;
    deploy_image(b, &project, &app).context("deploy (old version)")?;
    b.wait_healthy(&project)?;
    assert_app_reachable(b, &app, &project).context("pre-upgrade app reach")?;

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
    assert_app_reachable(b, &app, &seeded.project).context("post-upgrade app reach")?;

    // A fresh deploy on the upgraded control plane works end to end.
    report::note("deploying a fresh project on the upgraded version");
    let fresh = unique("e2e-upg-new");
    create_public_project(b, &fresh).context("project create (post-upgrade)")?;
    deploy_image(b, &fresh, &app).context("deploy (post-upgrade)")?;
    b.wait_healthy(&fresh)?;
    assert_app_reachable(b, &app, &fresh).context("fresh post-upgrade app reach")?;

    Ok(())
}
