use std::collections::HashSet;

use kube::api::{Api, ListParams, Patch, PatchParams};
use kube::Client;
use kube::CustomResource;
use kube::ResourceExt;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
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

/// Create or update a `RiseProject` CRD for the given project.
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
    if let Some(class) = controller_class {
        rise_project
            .metadata
            .labels
            .get_or_insert_with(std::collections::BTreeMap::new)
            .insert(CONTROLLER_CLASS_LABEL.to_string(), class.to_string());
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

/// Update the trigger annotation on a `RiseProject` CRD to force an immediate resync.
/// Called when deployment state changes (e.g., image pushed, status updated, stopped).
pub async fn trigger_resync(client: &Client, project_name: &str) -> anyhow::Result<()> {
    let api: Api<RiseProject> = Api::all(client.clone());
    let timestamp = chrono::Utc::now().to_rfc3339();

    let patch = serde_json::json!({
        "metadata": {
            "annotations": {
                TRIGGER_ANNOTATION: timestamp,
            },
        },
    });

    match api
        .patch(project_name, &PatchParams::default(), &Patch::Merge(patch))
        .await
    {
        Ok(_) => {
            info!(
                "Triggered resync for RiseProject '{}' (trigger={})",
                project_name, timestamp
            );
        }
        Err(kube::Error::Api(err)) if err.code == 404 => {
            warn!(
                "Cannot trigger resync: RiseProject '{}' not found (project may have been deleted)",
                project_name
            );
        }
        Err(e) => {
            return Err(e.into());
        }
    }

    Ok(())
}

/// Reconcile `RiseProject` CRDs for every active project in the database.
///
/// Handles three scenarios in one pass:
/// 1. **Upgrade**: When migrating to Metacontroller, no RiseProject CRDs exist yet — they're created.
/// 2. **Recovery**: An accidentally deleted CRD is recreated.
/// 3. **Relabel**: An existing CRD missing the `rise.dev/controller-class`
///    label (created before that label existed) is patched with it.
///
/// Calls `ensure_rise_project` unconditionally for every active project;
/// server-side apply is a no-op when the resulting object would not change,
/// so re-applying on every startup is safe.
///
/// Runs once at server startup. Per-project failures are logged as warnings
/// and do not block startup or other projects.
pub async fn backfill_rise_projects(
    client: &Client,
    db_pool: &PgPool,
    controller_class: Option<&str>,
) -> anyhow::Result<()> {
    let api: Api<RiseProject> = Api::all(client.clone());
    let existing_crds = api.list(&ListParams::default()).await?;
    let existing_names: HashSet<String> =
        existing_crds.items.iter().map(|r| r.name_any()).collect();

    let active_projects = crate::db::projects::list_active(db_pool).await?;

    let mut created = 0u32;
    let mut reconciled = 0u32;
    let mut failed = 0u32;
    for project in &active_projects {
        match ensure_rise_project(client, &project.name, controller_class).await {
            Ok(()) => {
                if existing_names.contains(&project.name) {
                    reconciled += 1;
                } else {
                    created += 1;
                }
            }
            Err(e) => {
                warn!(
                    project = %project.name,
                    "Failed to reconcile RiseProject CRD: {:?}", e
                );
                failed += 1;
            }
        }
    }

    if created > 0 || failed > 0 {
        info!(
            "RiseProject backfill: {} created, {} reconciled, {} failed \
             ({} active projects, {} pre-existing CRDs)",
            created,
            reconciled,
            failed,
            active_projects.len(),
            existing_names.len()
        );
    } else {
        debug!(
            "RiseProject backfill: {} reconciled, no creates/failures",
            reconciled
        );
    }

    Ok(())
}
