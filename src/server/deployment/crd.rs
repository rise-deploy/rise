use std::collections::HashMap;

use kube::api::{Api, ListParams, Patch, PatchParams};
use kube::Client;
use kube::CustomResource;
use kube::ResourceExt;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// CRD spec for RiseProject — intentionally empty.
/// The database remains the source of truth; the CRD is a marker that tells
/// Metacontroller "this project exists, manage its resources."
#[derive(CustomResource, Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "rise.dev",
    version = "v1alpha1",
    kind = "RiseProject",
    plural = "riseprojects",
    shortname = "rp",
    status = "RiseProjectStatus",
    derive = "Default"
)]
pub struct RiseProjectSpec {}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RiseProjectStatus {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_sync_time: Option<String>,
    /// Set by Metacontroller to track which generation of the spec has been observed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
}

/// Annotation key used to trigger an immediate Metacontroller resync.
/// Updating this annotation causes the CRD's `metadata.resourceVersion` to change,
/// which Metacontroller detects and triggers a sync webhook call.
const TRIGGER_ANNOTATION: &str = "rise.dev/trigger";

/// Label key recording which deployment controller's class owns a
/// `RiseProject` CR. The value is the project's Organization's
/// `spec.deploymentControllerClass` (in PR5 this is always the configured
/// controller's own class, because every project links to the default
/// Organization). The label exists so that a future multi-controller install
/// can switch its Metacontroller `CompositeController.parentResource` to a
/// `labelSelector` filtering on this key — at that point each controller
/// only sees its own CRs and they stop fighting over each other's children.
/// Today the webhook still enforces the same filter via
/// `enforce_controller_class`; the label is the forward-compatible carrier.
pub const CONTROLLER_CLASS_LABEL: &str = "rise.dev/controller-class";

/// Label key recording the Rise server version that last upserted this CRD.
/// When the label value changes after an upgrade, the SSA patch bumps
/// `resourceVersion`, which Metacontroller detects as a parent-resource update
/// and immediately triggers a sync webhook call — no explicit annotation nudge
/// required.
pub const RISE_VERSION_LABEL: &str = "rise.dev/version";

/// Create or update a `RiseProject` CRD for the given project.
///
/// Always stamps the current `CARGO_PKG_VERSION` as the `rise.dev/version`
/// label. When the label value changes (i.e. after an upgrade), the SSA patch
/// updates `resourceVersion`, which Metacontroller detects as a parent-resource
/// change and triggers an immediate sync webhook call — no separate annotation
/// nudge required.
///
/// `controller_class` is stamped as the `rise.dev/controller-class` label
/// when present. `None` means "no Kubernetes deployment controller is
/// configured for this install", which is unusual but supported.
pub async fn ensure_rise_project(
    client: &Client,
    project_name: &str,
    controller_class: Option<&str>,
) -> anyhow::Result<()> {
    let api: Api<RiseProject> = Api::all(client.clone());

    let mut rise_project = RiseProject::new(project_name, RiseProjectSpec {});
    let labels = rise_project
        .metadata
        .labels
        .get_or_insert_with(std::collections::BTreeMap::new);
    labels.insert(
        RISE_VERSION_LABEL.to_string(),
        env!("CARGO_PKG_VERSION").to_string(),
    );
    if let Some(class) = controller_class {
        labels.insert(CONTROLLER_CLASS_LABEL.to_string(), class.to_string());
    }

    api.patch(
        project_name,
        &PatchParams::apply("rise-controller").force(),
        &Patch::Apply(&rise_project),
    )
    .await?;

    info!("Ensured RiseProject CRD for project '{}'", project_name);
    Ok(())
}

/// Delete the `RiseProject` CRD for the given project.
/// Metacontroller will call the finalize webhook, which cleans up all children.
pub async fn delete_rise_project(client: &Client, project_name: &str) -> anyhow::Result<()> {
    let api: Api<RiseProject> = Api::all(client.clone());

    match api
        .delete(project_name, &kube::api::DeleteParams::default())
        .await
    {
        Ok(_) => {
            info!("Deleted RiseProject CRD for project '{}'", project_name);
        }
        Err(kube::Error::Api(err)) if err.code == 404 => {
            debug!(
                "RiseProject CRD for project '{}' did not exist (already deleted)",
                project_name
            );
        }
        Err(e) => {
            return Err(e.into());
        }
    }

    Ok(())
}

/// Outcome of a [`trigger_resync_status`] call.
pub enum ResyncOutcome {
    /// The annotation bump succeeded; Metacontroller will sync the project.
    Triggered,
    /// The `RiseProject` CR does not exist (deleted, or never created).
    ProjectNotFound,
}

/// Bump the trigger annotation on a `RiseProject` CRD to force a resync, reporting
/// whether the CR existed — so a caller can distinguish a deleted project from a
/// transient error (e.g. to stop re-picking an orphaned deployment).
pub async fn trigger_resync_status(
    client: &Client,
    project_name: &str,
) -> anyhow::Result<ResyncOutcome> {
    let api: Api<RiseProject> = Api::all(client.clone());
    let timestamp = chrono::Utc::now().to_rfc3339();
    let patch = serde_json::json!({
        "metadata": { "annotations": { TRIGGER_ANNOTATION: timestamp } },
    });
    match api
        .patch(project_name, &PatchParams::default(), &Patch::Merge(patch))
        .await
    {
        Ok(_) => Ok(ResyncOutcome::Triggered),
        Err(kube::Error::Api(err)) if err.code == 404 => Ok(ResyncOutcome::ProjectNotFound),
        Err(e) => Err(e.into()),
    }
}

/// Update the trigger annotation on a `RiseProject` CRD to force an immediate resync.
/// Called when deployment state changes (e.g., image pushed, status updated, stopped).
pub async fn trigger_resync(client: &Client, project_name: &str) -> anyhow::Result<()> {
    match trigger_resync_status(client, project_name).await? {
        ResyncOutcome::Triggered => {
            info!("Triggered resync for RiseProject '{}'", project_name);
        }
        ResyncOutcome::ProjectNotFound => {
            warn!(
                "Cannot trigger resync: RiseProject '{}' not found (project may have been deleted)",
                project_name
            );
        }
    }
    Ok(())
}

/// Reconcile `RiseProject` CRDs for every active project in the database.
///
/// Handles three scenarios in one pass:
/// 1. **Upgrade**: Existing CRDs whose `rise.dev/version` label differs
///    from the running version are patched — the label change bumps
///    `resourceVersion`, which Metacontroller sees as a parent-resource update
///    and immediately calls the sync webhook. No explicit `trigger_resync` call
///    is needed.
/// 2. **Recovery**: An accidentally deleted CRD is recreated.
/// 3. **New project**: A CRD that was never created (e.g. missed during a
///    failed earlier startup) is created now.
///
/// Projects are processed sequentially with `interval` between each upsert to
/// spread the load across the Kubernetes API server and Metacontroller rather
/// than flooding them all at once. Runs as a background task so startup is not
/// blocked. Per-project failures are logged as warnings and do not affect other
/// projects.
pub async fn backfill_rise_projects(
    client: &Client,
    store: &dyn rise_backend_core::DeploymentStore,
    controller_class: Option<&str>,
    interval: std::time::Duration,
) -> anyhow::Result<()> {
    let current_version = env!("CARGO_PKG_VERSION");
    let api: Api<RiseProject> = Api::all(client.clone());
    let existing_crds = api.list(&ListParams::default()).await?;

    // Build a name→version-label map so we can log how many projects are
    // getting an upgrade-triggered resync vs. a plain no-op reconcile.
    let existing_version_by_name: HashMap<String, Option<String>> = existing_crds
        .items
        .iter()
        .map(|r| {
            let version = r
                .metadata
                .labels
                .as_ref()
                .and_then(|l| l.get(RISE_VERSION_LABEL))
                .cloned();
            (r.name_any(), version)
        })
        .collect();

    let active_projects = store.list_active_projects().await?;

    let total = active_projects.len();
    info!(
        "RiseProject backfill: {} active projects, {} pre-existing CRDs, interval={}ms",
        total,
        existing_version_by_name.len(),
        interval.as_millis(),
    );

    let mut created = 0u32;
    let mut upgraded = 0u32;
    let mut reconciled = 0u32;
    let mut failed = 0u32;
    for (i, project) in active_projects.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(interval).await;
        }
        match ensure_rise_project(client, &project.name, controller_class).await {
            Ok(()) => match existing_version_by_name.get(&project.name) {
                None => created += 1,
                Some(v) if v.as_deref() != Some(current_version) => upgraded += 1,
                _ => reconciled += 1,
            },
            Err(e) => {
                warn!(
                    project = %project.name,
                    "Failed to reconcile RiseProject CRD: {:?}", e
                );
                failed += 1;
            }
        }
    }

    info!(
        "RiseProject backfill complete: {} created, {} upgraded, {} reconciled, {} failed",
        created, upgraded, reconciled, failed,
    );

    Ok(())
}
