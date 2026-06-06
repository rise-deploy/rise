//! Metacontroller sync and finalize webhook handlers.
//!
//! Metacontroller calls the sync webhook periodically (every N seconds) and whenever
//! the `RiseProject` CRD changes. The webhook inspects the database for the project's
//! current state, performs health checks using observed K8s resources, updates DB
//! statuses, and returns the desired set of child K8s resources.
//!
//! Metacontroller then reconciles: it creates/updates resources that are returned
//! and deletes resources that are NOT returned (garbage collection).

use std::collections::{BTreeMap, HashMap};

use anyhow::Context;
use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::Utc;
use k8s_openapi::api::apps::v1::Deployment as K8sDeployment;
use k8s_openapi::api::core::v1::{EnvVar, Secret};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::ByteString;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{debug, error, info, warn};

use crate::db::models::{
    Deployment, DeploymentEnvVar, DeploymentStatus, Project, TerminationReason,
};
use crate::db::{
    deployments as db_deployments, env_vars as db_env_vars, environments as db_environments,
    projects as db_projects,
};
use crate::server::deployment::crd;
use crate::server::deployment::resource_builder::{
    IdentityMount, ResourceBuilder, ANNOTATION_ENV_SECRET_HASH, ANNOTATION_LAST_REFRESH,
    IDENTITY_CREDENTIAL_KEY, IDENTITY_TOKEN_KEY_PREFIX, IMAGE_PULL_SECRET_NAME,
    IRRECOVERABLE_CONTAINER_REASONS, LABEL_DEPLOYMENT_ID,
};
use crate::server::deployment::state_machine;
use crate::server::state::AppState;
use crate::server::workload_tokens::{sha256_hex, workload_subject, NO_ENVIRONMENT};
use rise_backend_auth::WorkloadSubjectInfo;

// ── Metacontroller webhook protocol types ──────────────────────────────

/// Metacontroller sync request
#[derive(Debug, Deserialize)]
pub struct SyncRequest {
    pub parent: serde_json::Value,
    #[serde(default)]
    pub children: ObservedChildren,
}

/// Observed children keyed by "Kind.apiVersion" → name → resource JSON
#[derive(Debug, Default, Deserialize)]
#[allow(dead_code)]
pub struct ObservedChildren {
    #[serde(rename = "Namespace.v1", default)]
    pub namespaces: HashMap<String, serde_json::Value>,
    #[serde(rename = "Secret.v1", default)]
    pub secrets: HashMap<String, serde_json::Value>,
    #[serde(rename = "ServiceAccount.v1", default)]
    pub service_accounts: HashMap<String, serde_json::Value>,
    #[serde(rename = "Deployment.apps/v1", default)]
    pub deployments: HashMap<String, serde_json::Value>,
    #[serde(rename = "Service.v1", default)]
    pub services: HashMap<String, serde_json::Value>,
    #[serde(rename = "Ingress.networking.k8s.io/v1", default)]
    pub ingresses: HashMap<String, serde_json::Value>,
    #[serde(rename = "NetworkPolicy.networking.k8s.io/v1", default)]
    pub network_policies: HashMap<String, serde_json::Value>,
}

/// Metacontroller sync response
#[derive(Debug, Serialize)]
pub struct SyncResponse {
    pub status: serde_json::Value,
    pub children: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "resyncAfterSeconds")]
    pub resync_after_seconds: Option<f64>,
}

/// Metacontroller finalize request (same shape as sync)
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct FinalizeRequest {
    pub parent: serde_json::Value,
    #[serde(default)]
    pub children: ObservedChildren,
}

/// Metacontroller finalize response
#[derive(Debug, Serialize)]
pub struct FinalizeResponse {
    pub status: serde_json::Value,
    pub children: Vec<serde_json::Value>,
    pub finalized: bool,
}

// ── Deployment timeout ─────────────────────────────────────────────────

/// Duration a deployment can be in Deploying state before timing out
pub(crate) const DEPLOYING_TIMEOUT_MINUTES: i64 = 5;
/// Duration a deployment can be in pre-Pushed states before timing out
pub(crate) const PRE_PUSHED_TIMEOUT_MINUTES: i64 = 10;
/// Duration after which image pull secret is refreshed (6 hours)
const SECRET_REFRESH_HOURS: i64 = 6;
/// Maximum number of terminating/terminated pods to carry forward in controller_metadata
const MAX_INACTIVE_PODS: usize = 5;

#[derive(Debug, Default)]
pub(crate) struct ResolvedDeploymentEnvVars {
    pub(crate) plain_env_vars: Vec<EnvVar>,
    pub(crate) secret_env_vars: BTreeMap<String, ByteString>,
}

#[derive(Debug)]
struct PreparedDeploymentEnvSecret {
    secret_name: String,
    secret_hash: String,
    is_ready: bool,
    secret: Secret,
}

// ── Webhook authentication ─────────────────────────────────────────────

async fn validate_source_ip(
    state: &AppState,
    addr: std::net::SocketAddr,
) -> Result<(), (StatusCode, &'static str)> {
    match &state.metacontroller_ip_validator {
        Some(validator) => validator.validate(addr).await,
        None => Ok(()), // dev mode — no validation
    }
}

// ── Sync webhook handler ───────────────────────────────────────────────

pub async fn handle_sync(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    Json(request): Json<SyncRequest>,
) -> Response {
    if let Err((status, msg)) = validate_source_ip(&state, addr).await {
        return (status, msg).into_response();
    }
    let project_name = match request
        .parent
        .get("metadata")
        .and_then(|m| m.get("name"))
        .and_then(|n| n.as_str())
    {
        Some(name) => name.to_string(),
        None => {
            error!("Sync request missing parent.metadata.name");
            return (
                StatusCode::BAD_REQUEST,
                Json(SyncResponse {
                    status: serde_json::json!({"error": "missing parent name"}),
                    children: vec![],
                    resync_after_seconds: Some(300.0),
                }),
            )
                .into_response();
        }
    };

    match process_sync(&state, &project_name, &request.children).await {
        Ok(response) => (StatusCode::OK, Json(response)).into_response(),
        Err(SyncError::WrongController {
            project,
            configured,
            org_class,
        }) => {
            // Expected steady state: the project belongs to a different
            // deployment controller. The CompositeController `labelSelector`
            // should already filter most of these out — log at warn (not
            // error) so the operator sees one if the label and the
            // Organization linkage have drifted. 4xx prevents Metacontroller
            // from applying the empty children list (no GC).
            warn!(
                project = %project,
                configured = %configured,
                org_class = ?org_class,
                "Refusing to reconcile — project Organization's deploymentControllerClass does not match"
            );
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "controller-class mismatch",
                    "project": project,
                    "configured": configured,
                    "org_class": org_class,
                    "lastSyncTime": Utc::now().to_rfc3339(),
                })),
            )
                .into_response()
        }
        Err(SyncError::Internal(e)) => {
            error!(project = %project_name, "Sync webhook error: {:?}", e);
            // Return 500 so Metacontroller treats this as a failed sync and does NOT
            // apply the (empty) children list, which would garbage-collect all resources.
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": format!("{:#}", e),
                    "lastSyncTime": Utc::now().to_rfc3339(),
                })),
            )
                .into_response()
        }
    }
}

async fn process_sync(
    state: &AppState,
    project_name: &str,
    observed: &ObservedChildren,
) -> Result<SyncResponse, SyncError> {
    // 1. Load project from DB
    let project = match db_projects::find_by_name(&state.db_pool, project_name).await? {
        Some(p) => p,
        None => {
            warn!(project = %project_name, "Project not found in DB, deleting orphaned RiseProject CRD");
            // Auto-delete the orphaned CRD so Metacontroller stops syncing it
            if let Some(ref kube_client) = state.kube_client {
                if let Err(e) = crd::delete_rise_project(kube_client, project_name).await {
                    error!(project = %project_name, "Failed to delete orphaned RiseProject CRD: {:?}", e);
                }
            }
            return Ok(SyncResponse {
                status: serde_json::json!({
                    "error": "project not found",
                    "lastSyncTime": Utc::now().to_rfc3339(),
                }),
                children: vec![],
                resync_after_seconds: Some(300.0),
            });
        }
    };

    // 1b. Resolve the project's Organization UID once and thread it
    // through the controller-class check and the namespace-prefix
    // lookup. Both of those used to re-read `project_organizations`
    // independently — one DB hit per sync now serves both.
    let org_uid = resolve_project_organization_uid(state, &project).await?;

    // 1c. Verify the project's Organization is assigned to this controller.
    //
    // The Kubernetes controller filters strictly by the configured
    // `controller_class_name`. Projects whose Organization carries a
    // different (or unset) `spec.deploymentControllerClass` belong to a
    // different controller — return `SyncError::WrongController` so the
    // handler answers 400 (not 200 with empty children, which would let
    // Metacontroller GC resources owned by another controller).
    enforce_controller_class(state, &project, org_uid).await?;

    // 1d. Resolve the project's Org-scoped namespace prefix once. Every
    // helper below that names a namespace borrows this string, so the
    // miss-loader runs at most once per sync (the cache hits the second
    // and subsequent times within the 30s TTL).
    let namespace_prefix = resolve_project_namespace_prefix(state, &project, org_uid).await?;

    // 2. Load non-terminal deployments for this project (avoids loading full history)
    let non_terminal_deployments =
        db_deployments::list_non_terminal_for_project(&state.db_pool, project.id).await?;

    let non_terminal: Vec<&Deployment> = non_terminal_deployments.iter().collect();

    // 3. Perform status transitions based on observed K8s state
    perform_status_transitions(state, &project, &non_terminal, observed, &namespace_prefix).await?;

    // 4. Re-load non-terminal deployments since statuses may have changed
    let all_deployments =
        db_deployments::list_non_terminal_for_project(&state.db_pool, project.id).await?;
    let project = match db_projects::find_by_id(&state.db_pool, project.id).await? {
        Some(p) => p,
        None => {
            // Project was deleted between initial lookup and now — return empty children
            return Ok(SyncResponse {
                status: serde_json::json!({
                    "lastSyncTime": Utc::now().to_rfc3339(),
                }),
                children: vec![],
                resync_after_seconds: Some(300.0),
            });
        }
    };

    // 5. Get ResourceBuilder — returning an error (HTTP 500) if not configured,
    // so Metacontroller does NOT apply an empty children list which would
    // garbage-collect all resources.
    let resource_builder = match &state.resource_builder {
        Some(rb) => rb,
        None => {
            return Err(SyncError::Internal(anyhow::anyhow!(
                "No resource builder configured — cannot compute desired children"
            )));
        }
    };

    // 6. Compute desired children
    let children = compute_desired_children(
        state,
        resource_builder,
        &project,
        &all_deployments,
        observed,
        &namespace_prefix,
    )
    .await?;

    Ok(SyncResponse {
        status: serde_json::json!({
            "lastSyncTime": Utc::now().to_rfc3339(),
        }),
        children,
        resync_after_seconds: None,
    })
}

/// Errors that bubble out of [`process_sync`]. `WrongController` is a
/// steady-state expected outcome (this controller doesn't own the project)
/// and is mapped to HTTP 400 + `warn!` by the handler; `Internal` covers
/// genuine failures and maps to 500 + `error!`. Both prevent Metacontroller
/// from applying the (empty) children list, so neither GCs resources.
#[derive(Debug)]
enum SyncError {
    /// The project's Organization's `spec.deploymentControllerClass` does not
    /// match this controller's configured `controller_class_name`. Logged
    /// once at warn level.
    WrongController {
        project: String,
        configured: String,
        org_class: Option<String>,
    },
    /// Anything else — DB error, store error, malformed Org, missing linkage.
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for SyncError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e)
    }
}

/// Pure comparison of the configured controller class against the
/// Organization's `spec.deploymentControllerClass`. Returns `Ok(())` when
/// reconciliation should proceed (legacy `configured = None`, or both match)
/// and `Err(())` when the controller should refuse this project.
fn check_controller_class(configured: Option<&str>, org_class: Option<&str>) -> Result<(), ()> {
    match configured {
        None => Ok(()),
        Some(c) => match org_class {
            Some(o) if o == c => Ok(()),
            _ => Err(()),
        },
    }
}

/// Snapshot of the per-Organization fields the Metacontroller webhook
/// reads on every sync. Cached behind `AppState::org_view_cache` so the
/// controller hits the resource store at most once per Org per 30s
/// window — the previous design had two independent caches (one per
/// field) and re-read the same row twice.
#[derive(Debug, Clone)]
pub struct CachedOrganizationView {
    /// Value of `spec.deploymentControllerClass`. `None` when the
    /// Organization exists but does not set the field.
    pub controller_class: Option<String>,
    /// Resolved Kubernetes namespace prefix
    /// (`kubernetes.rise.dev/namespace-prefix` annotation, or
    /// `org-{discriminator}-` fallback).
    pub namespace_prefix: String,
}

/// Cache miss-loader for `AppState::org_view_cache`. Reads the
/// Organization row once and projects both fields the webhook needs.
/// Missing Organizations surface as `Err`, which prevents the cache from
/// memoising a transient "not found" (`try_get_with` only retains
/// successful loads).
async fn load_org_view(
    store: std::sync::Arc<dyn rise_resource_store::ResourceStore>,
    uid: uuid::Uuid,
) -> anyhow::Result<CachedOrganizationView> {
    let row = store
        .get(uid)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to load Organization {uid}: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("Organization {uid} is missing"))?;
    let spec: rise_resource_api::OrganizationSpec = serde_json::from_value(row.spec.clone())
        .map_err(|e| anyhow::anyhow!("Organization {uid} has malformed spec: {e}"))?;
    let annotations: BTreeMap<String, String> =
        serde_json::from_value(row.metadata.clone()).unwrap_or_default();
    let namespace_prefix =
        crate::server::bootstrap::resolve_namespace_prefix(&annotations, &row.discriminator);
    Ok(CachedOrganizationView {
        controller_class: spec.deployment_controller_class,
        namespace_prefix,
    })
}

/// Resolve the project's Organization UID, surfacing the "missing
/// linkage" invariant as a structured error. Bootstrap validation
/// refuses to start the server with unlinked projects, so a `None` here
/// is a programmer error — logged at `error!` rather than letting the
/// `None` propagate silently.
pub async fn resolve_project_organization_uid(
    state: &AppState,
    project: &Project,
) -> anyhow::Result<uuid::Uuid> {
    crate::db::organization_links::organization_uid_for_project(&state.db_pool, project.id)
        .await?
        .ok_or_else(|| {
            error!(
                project = %project.name,
                "Project has no organization linkage — bootstrap validation should have caught this; refusing to reconcile"
            );
            anyhow::anyhow!(
                "project '{}' is missing organization_resource_uid; bootstrap must backfill before reconciliation",
                project.name,
            )
        })
}

/// Look up the project's Org-scoped namespace prefix through
/// `AppState::org_view_cache`. The caller resolves the Organization UID
/// once per request (via `resolve_project_organization_uid`) and threads
/// it through so the controller-class check and this lookup share a
/// single DB and cache read.
pub async fn resolve_project_namespace_prefix(
    state: &AppState,
    project: &Project,
    org_uid: uuid::Uuid,
) -> anyhow::Result<String> {
    let view = state
        .org_view_cache
        .try_get_with(
            org_uid,
            load_org_view(state.resource_store.clone(), org_uid),
        )
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "Failed to load Organization {org_uid} for project '{}': {e}",
                project.name,
            )
        })?;
    Ok(view.namespace_prefix)
}

/// Reject reconciliation when the project's Organization's
/// `spec.deploymentControllerClass` does not match the controller's
/// configured `controller_class_name`. Reads the Org through a short-TTL
/// cache so a steady stream of resyncs does not hit the store on every
/// tick. When the controller has no configured class (legacy installs),
/// this is a no-op.
async fn enforce_controller_class(
    state: &AppState,
    project: &Project,
    org_uid: uuid::Uuid,
) -> Result<(), SyncError> {
    let Some(configured) = state.deployment_controller_class_name.as_deref() else {
        return Ok(());
    };

    let view = state
        .org_view_cache
        .try_get_with(
            org_uid,
            load_org_view(state.resource_store.clone(), org_uid),
        )
        .await
        .map_err(|e| {
            SyncError::Internal(anyhow::anyhow!(
                "Failed to load Organization {org_uid} for project '{}': {e}",
                project.name,
            ))
        })?;

    match check_controller_class(Some(configured), view.controller_class.as_deref()) {
        Ok(()) => Ok(()),
        Err(()) => Err(SyncError::WrongController {
            project: project.name.clone(),
            configured: configured.to_string(),
            org_class: view.controller_class,
        }),
    }
}

/// Inspect the observed Kubernetes state for each non-terminal deployment and
/// advance its status: Pushed → Deploying, Deploying → Healthy/Failed, timeouts,
/// expiration, and cancellation.
async fn perform_status_transitions(
    state: &AppState,
    project: &Project,
    non_terminal: &[&Deployment],
    observed: &ObservedChildren,
    namespace_prefix: &str,
) -> anyhow::Result<()> {
    for deployment in non_terminal {
        // Skip pre-infrastructure deployments — the CLI drives those transitions
        if matches!(
            deployment.status,
            DeploymentStatus::Pending | DeploymentStatus::Building | DeploymentStatus::Pushing
        ) {
            // Check for pre-pushed timeout
            check_pre_pushed_timeout(state, deployment).await?;
            continue;
        }

        // Handle Cancelling — mark as Cancelled immediately.
        // Any K8s resources created during Deploying will be garbage-collected by
        // Metacontroller since `should_have_infrastructure` returns false for Cancelled.
        if deployment.status == DeploymentStatus::Cancelling {
            info!(
                deployment_id = %deployment.deployment_id,
                "Cancelling deployment — marking as Cancelled"
            );
            db_deployments::mark_cancelled(&state.db_pool, deployment.id).await?;
            db_projects::update_calculated_status(&state.db_pool, project.id).await?;
            continue;
        }

        // Handle Terminating — mark as terminal based on reason
        // (Metacontroller will delete the K8s Deployment since we won't return it)
        if deployment.status == DeploymentStatus::Terminating {
            complete_termination(state, deployment, project).await?;
            continue;
        }

        // For Pushed/Deploying/Healthy/Unhealthy — check observed K8s Deployment
        match deployment.status {
            DeploymentStatus::Pushed => {
                // Transition Pushed → Deploying
                info!(
                    deployment_id = %deployment.deployment_id,
                    "Deployment image pushed, transitioning to Deploying"
                );
                db_deployments::update_status(
                    &state.db_pool,
                    deployment.id,
                    DeploymentStatus::Deploying,
                )
                .await?;
                db_projects::update_calculated_status(&state.db_pool, project.id).await?;
            }

            DeploymentStatus::Deploying => {
                check_deploying_timeout(state, deployment, project).await?;
                check_deployment_health_from_observed(
                    state,
                    deployment,
                    project,
                    observed,
                    namespace_prefix,
                )
                .await?;
            }

            DeploymentStatus::Healthy | DeploymentStatus::Unhealthy => {
                check_deployment_health_from_observed(
                    state,
                    deployment,
                    project,
                    observed,
                    namespace_prefix,
                )
                .await?;
            }

            _ => {}
        }
    }

    // Check for expired deployments
    check_expirations(state, non_terminal, project).await?;

    // Failed deployments don't need explicit cleanup: `should_have_infrastructure` returns
    // false for Failed status, so Metacontroller garbage-collects their K8s resources.

    Ok(())
}

/// Check if a pre-pushed deployment has timed out
async fn check_pre_pushed_timeout(state: &AppState, deployment: &Deployment) -> anyhow::Result<()> {
    let elapsed = Utc::now().signed_duration_since(deployment.created_at);
    if elapsed > chrono::Duration::minutes(PRE_PUSHED_TIMEOUT_MINUTES) {
        warn!(
            deployment_id = %deployment.deployment_id,
            "Deployment stuck in {} state for >{} minutes, marking as Failed",
            deployment.status,
            PRE_PUSHED_TIMEOUT_MINUTES
        );
        let error_msg = format!(
            "Deployment timed out after {} minutes in {} state. \
             This usually indicates the CLI was interrupted during build/push.",
            PRE_PUSHED_TIMEOUT_MINUTES, deployment.status
        );
        db_deployments::mark_failed(&state.db_pool, deployment.id, &error_msg).await?;
        db_projects::update_calculated_status(&state.db_pool, deployment.project_id).await?;
    }
    Ok(())
}

/// Check if a deploying deployment has timed out
async fn check_deploying_timeout(
    state: &AppState,
    deployment: &Deployment,
    project: &Project,
) -> anyhow::Result<()> {
    if let Some(deploying_started_at) = deployment.deploying_started_at {
        let elapsed = Utc::now().signed_duration_since(deploying_started_at);
        if elapsed > chrono::Duration::minutes(DEPLOYING_TIMEOUT_MINUTES) {
            let error_msg = format!(
                "Deployment timed out after {} seconds in Deploying state",
                elapsed.num_seconds()
            );
            warn!(
                deployment_id = %deployment.deployment_id,
                "{}", error_msg
            );
            db_deployments::mark_failed(&state.db_pool, deployment.id, &error_msg).await?;
            db_projects::update_calculated_status(&state.db_pool, project.id).await?;
        }
    }
    Ok(())
}

/// Complete termination: move from Terminating to the appropriate terminal state.
async fn complete_termination(
    state: &AppState,
    deployment: &Deployment,
    project: &Project,
) -> anyhow::Result<()> {
    match deployment.termination_reason {
        Some(TerminationReason::Superseded) => {
            db_deployments::mark_superseded(&state.db_pool, deployment.id).await?;
        }
        Some(TerminationReason::UserStopped) => {
            db_deployments::mark_stopped(&state.db_pool, deployment.id).await?;
        }
        Some(TerminationReason::Expired) => {
            db_deployments::mark_expired(&state.db_pool, deployment.id).await?;
        }
        Some(TerminationReason::Failed) => {
            db_deployments::mark_failed(
                &state.db_pool,
                deployment.id,
                deployment
                    .error_message
                    .as_deref()
                    .unwrap_or("Deployment failed"),
            )
            .await?;
        }
        Some(TerminationReason::Cancelled) => {
            db_deployments::mark_cancelled(&state.db_pool, deployment.id).await?;
        }
        None => {
            // Missing termination reason resolves to Stopped
            db_deployments::mark_stopped(&state.db_pool, deployment.id).await?;
        }
    }
    db_projects::update_calculated_status(&state.db_pool, project.id).await?;
    Ok(())
}

/// Check deployment health from observed K8s Deployment status.
/// Handles transitions: Deploying → Healthy/Failed, Healthy → Unhealthy, Unhealthy → Healthy.
///
/// Multi-container deployments emit one K8s Deployment per container
/// (`{project}-{deployment_id}-{container}`). This function sums desired/ready
/// replicas across every container and only marks the Rise deployment Healthy
/// when every K8s Deployment is fully ready.
async fn check_deployment_health_from_observed(
    state: &AppState,
    deployment: &Deployment,
    project: &Project,
    observed: &ObservedChildren,
    namespace_prefix: &str,
) -> anyhow::Result<()> {
    // Metacontroller keys namespaced children of cluster-scoped parents as "namespace/name"
    let namespace = ResourceBuilder::namespace_name(project, namespace_prefix);
    let base_name = ResourceBuilder::deployment_name(project, deployment);

    // Resolve the list of K8s Deployment names to aggregate over — one per
    // container. A single-container deployment has its implicit `app` container.
    // `containers`/`routes` come folded onto the row. An unparseable (non-NULL
    // but corrupt) side-data column fails ONLY this deployment — mark it Failed
    // and stop aggregating health for it rather than flipping it Healthy/Failed
    // against the wrong names or failing the whole project sync.
    let (container_specs, _routes) = match resolve_runtime_containers(deployment) {
        Ok(resolved) => resolved,
        Err(e) => {
            error!(
                deployment_id = %deployment.deployment_id,
                "container side-data could not be deserialized; marking deployment Failed: {:?}", e
            );
            db_deployments::mark_failed(
                &state.db_pool,
                deployment.id,
                "container side-data could not be deserialized",
            )
            .await?;
            db_projects::update_calculated_status(&state.db_pool, project.id).await?;
            return Ok(());
        }
    };
    let k8s_deploy_names: Vec<String> = container_specs
        .iter()
        .map(|spec| format!("{}/{}-{}", namespace, base_name, spec.name))
        .collect();

    let mut desired_replicas: i32 = 0;
    let mut ready_replicas: i32 = 0;
    for name in &k8s_deploy_names {
        let observed_deploy = match observed.deployments.get(name) {
            Some(d) => d,
            None => {
                // At least one K8s Deployment hasn't been observed yet — wait
                // for the next reconcile before computing health.
                debug!(
                    deployment_id = %deployment.deployment_id,
                    "No observed K8s Deployment yet for {}", name
                );
                return Ok(());
            }
        };
        let observed_k8s: K8sDeployment = serde_json::from_value(observed_deploy.clone())?;
        desired_replicas += observed_k8s
            .spec
            .as_ref()
            .and_then(|s| s.replicas)
            .unwrap_or(1);
        ready_replicas += observed_k8s
            .status
            .as_ref()
            .and_then(|s| s.ready_replicas)
            .unwrap_or(0);
    }

    // Check for pod-level errors and collect full pod status via kube-rs
    // (Metacontroller only gives us the Deployment, not individual pods)
    let pod_check = check_pod_errors_via_kube(
        state,
        project,
        deployment,
        desired_replicas,
        ready_replicas,
        &deployment.controller_metadata,
        namespace_prefix,
    )
    .await;

    // Update controller_metadata with pod status
    if let Some(ref pod_status) = pod_check.pod_status {
        let is_healthy = deployment.status == DeploymentStatus::Healthy
            || (deployment.status == DeploymentStatus::Deploying
                && !pod_check.has_error
                && ready_replicas >= desired_replicas
                && desired_replicas > 0);

        let metadata = serde_json::json!({
            "pod_status": pod_status,
            "health": {
                "last_check": Utc::now().to_rfc3339(),
                "healthy": is_healthy,
            },
        });
        if let Err(e) =
            db_deployments::update_controller_metadata(&state.db_pool, deployment.id, &metadata)
                .await
        {
            warn!(
                deployment_id = %deployment.deployment_id,
                "Failed to update controller metadata: {:?}", e
            );
        }
    }

    let is_ready = ready_replicas >= desired_replicas && desired_replicas > 0;

    match deployment.status {
        DeploymentStatus::Deploying => {
            if pod_check.has_error {
                let error_msg = pod_check
                    .error_message
                    .unwrap_or_else(|| "Pod error".to_string());
                warn!(
                    deployment_id = %deployment.deployment_id,
                    "Deployment has irrecoverable pod error: {}", error_msg
                );
                db_deployments::mark_failed(&state.db_pool, deployment.id, &error_msg).await?;
                db_projects::update_calculated_status(&state.db_pool, project.id).await?;
            } else if is_ready {
                info!(
                    deployment_id = %deployment.deployment_id,
                    "Deployment is ready ({}/{} replicas), marking as Healthy",
                    ready_replicas,
                    desired_replicas
                );
                handle_deployment_became_healthy(state, deployment, project).await?;
            }
        }

        DeploymentStatus::Healthy if pod_check.has_error || !is_ready => {
            let msg = pod_check.error_message.unwrap_or_else(|| {
                format!(
                    "Deployment unhealthy: {}/{} replicas ready",
                    ready_replicas, desired_replicas
                )
            });
            warn!(
                deployment_id = %deployment.deployment_id,
                "Healthy deployment is now unhealthy: {}", msg
            );
            db_deployments::mark_unhealthy(&state.db_pool, deployment.id, msg).await?;
            db_projects::update_calculated_status(&state.db_pool, project.id).await?;
        }

        DeploymentStatus::Unhealthy if !pod_check.has_error && is_ready => {
            info!(
                deployment_id = %deployment.deployment_id,
                "Unhealthy deployment has recovered, marking as Healthy"
            );
            db_deployments::mark_healthy(&state.db_pool, deployment.id).await?;
            db_projects::update_calculated_status(&state.db_pool, project.id).await?;
        }

        _ => {}
    }

    Ok(())
}

/// Result of checking pod status via kube-rs API.
struct PodCheckResult {
    /// Whether any pod has an irrecoverable error
    has_error: bool,
    /// Error message if has_error is true
    error_message: Option<String>,
    /// Full pod status for storing in controller_metadata
    pod_status: Option<serde_json::Value>,
}

/// Check pods for errors via direct kube-rs API call and collect full pod status.
///
/// Compares the live pod list against the previous `controller_metadata` snapshot:
/// pods that were `terminating` or `terminated` in the previous snapshot but no longer
/// exist in K8s are carried forward as `terminated: true` so the frontend can show
/// their last-known state instead of silently dropping them.
async fn check_pod_errors_via_kube(
    state: &AppState,
    project: &Project,
    deployment: &Deployment,
    desired_replicas: i32,
    ready_replicas: i32,
    prev_controller_metadata: &serde_json::Value,
    namespace_prefix: &str,
) -> PodCheckResult {
    let kube_client = match &state.kube_client {
        Some(client) => client,
        None => {
            return PodCheckResult {
                has_error: false,
                error_message: None,
                pod_status: None,
            }
        }
    };

    let namespace = ResourceBuilder::namespace_name(project, namespace_prefix);
    let pod_api: kube::Api<k8s_openapi::api::core::v1::Pod> =
        kube::Api::namespaced(kube_client.clone(), &namespace);

    let pods = match pod_api
        .list(&kube::api::ListParams::default().labels(&format!(
            "{}={}",
            LABEL_DEPLOYMENT_ID, deployment.deployment_id
        )))
        .await
    {
        Ok(pods) => pods,
        Err(e) => {
            debug!(
                deployment_id = %deployment.deployment_id,
                "Failed to list pods for health check: {:?}", e
            );
            return PodCheckResult {
                has_error: false,
                error_message: None,
                pod_status: None,
            };
        }
    };

    let mut has_error = false;
    let mut error_message: Option<String> = None;
    let mut pod_infos: Vec<serde_json::Value> = Vec::new();
    let mut current_replicas: i32 = 0;

    for pod in &pods.items {
        let is_terminating = pod.metadata.deletion_timestamp.is_some();
        if !is_terminating {
            current_replicas += 1;
        }
        let pod_name = pod
            .metadata
            .name
            .as_deref()
            .unwrap_or("unknown")
            .to_string();
        let pod_phase = pod
            .status
            .as_ref()
            .and_then(|s| s.phase.as_deref())
            .unwrap_or("Unknown")
            .to_string();

        // Collect pod conditions
        let conditions: Vec<serde_json::Value> = pod
            .status
            .as_ref()
            .and_then(|s| s.conditions.as_ref())
            .map(|conds| {
                conds
                    .iter()
                    .map(|c| {
                        serde_json::json!({
                            "type": c.type_,
                            "status": c.status,
                            "reason": c.reason,
                            "message": c.message,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Collect container statuses
        let mut container_infos: Vec<serde_json::Value> = Vec::new();
        if let Some(container_statuses) = pod
            .status
            .as_ref()
            .and_then(|s| s.container_statuses.as_ref())
        {
            for cs in container_statuses {
                let state_info = if let Some(state) = &cs.state {
                    if let Some(waiting) = &state.waiting {
                        let reason = waiting.reason.as_deref().unwrap_or("");
                        // Check for irrecoverable errors (skip for terminating pods)
                        if !is_terminating
                            && !has_error
                            && IRRECOVERABLE_CONTAINER_REASONS.contains(&reason)
                        {
                            has_error = true;
                            let message = waiting.message.as_deref().unwrap_or(reason);
                            error_message = Some(format!("{}: {}", reason, message));
                        }
                        Some(serde_json::json!({
                            "state_type": "waiting",
                            "reason": waiting.reason,
                            "message": waiting.message,
                        }))
                    } else if let Some(running) = &state.running {
                        Some(serde_json::json!({
                            "state_type": "running",
                            "started_at": running.started_at.as_ref().map(|t| t.0.to_string()),
                        }))
                    } else if let Some(terminated) = &state.terminated {
                        // Check terminated with too many restarts (skip for terminating pods)
                        if !is_terminating
                            && !has_error
                            && terminated.exit_code != 0
                            && cs.restart_count >= 3
                        {
                            has_error = true;
                            let reason = terminated.reason.as_deref().unwrap_or("ContainerFailed");
                            let default_msg = format!("Exit code: {}", terminated.exit_code);
                            let message = terminated.message.as_deref().unwrap_or(&default_msg);
                            error_message = Some(format!(
                                "{}: {} (restarts: {})",
                                reason, message, cs.restart_count
                            ));
                        }
                        Some(serde_json::json!({
                            "state_type": "terminated",
                            "reason": terminated.reason,
                            "message": terminated.message,
                            "exit_code": terminated.exit_code,
                            "started_at": terminated.started_at.as_ref().map(|t| t.0.to_string()),
                            "finished_at": terminated.finished_at.as_ref().map(|t| t.0.to_string()),
                        }))
                    } else {
                        None
                    }
                } else {
                    None
                };

                // Collect last_state info (the previous terminated state, e.g. OOMKilled)
                let last_state_info = if let Some(last_state) = &cs.last_state {
                    if let Some(terminated) = &last_state.terminated {
                        // Surface OOMKilled or other bad exit in last_state as an error
                        // when restarts are high and the current state didn't already trigger one.
                        if !is_terminating
                            && !has_error
                            && (terminated.reason.as_deref() == Some("OOMKilled")
                                || (terminated.exit_code != 0 && cs.restart_count >= 3))
                        {
                            has_error = true;
                            let reason = terminated.reason.as_deref().unwrap_or("ContainerFailed");
                            let default_msg = format!("Exit code: {}", terminated.exit_code);
                            let message = terminated.message.as_deref().unwrap_or(&default_msg);
                            error_message = Some(format!(
                                "{}: {} (restarts: {})",
                                reason, message, cs.restart_count
                            ));
                        }
                        Some(serde_json::json!({
                            "state_type": "terminated",
                            "reason": terminated.reason,
                            "message": terminated.message,
                            "exit_code": terminated.exit_code,
                            "started_at": terminated.started_at.as_ref().map(|t| t.0.to_string()),
                            "finished_at": terminated.finished_at.as_ref().map(|t| t.0.to_string()),
                        }))
                    } else if let Some(waiting) = &last_state.waiting {
                        Some(serde_json::json!({
                            "state_type": "waiting",
                            "reason": waiting.reason,
                            "message": waiting.message,
                        }))
                    } else {
                        last_state.running.as_ref().map(|running| {
                            serde_json::json!({
                                "state_type": "running",
                                "started_at": running.started_at.as_ref().map(|t| t.0.to_string()),
                            })
                        })
                    }
                } else {
                    None
                };

                container_infos.push(serde_json::json!({
                    "name": cs.name,
                    "ready": cs.ready,
                    "restart_count": cs.restart_count,
                    "state": state_info,
                    "last_state": last_state_info,
                }));
            }
        }

        pod_infos.push(serde_json::json!({
            "name": pod_name,
            "phase": pod_phase,
            "terminating": is_terminating,
            "conditions": conditions,
            "containers": container_infos,
        }));
    }

    // Carry forward pods that were terminating/terminated in the previous snapshot
    // but are no longer in the K8s pod list — mark them as fully terminated.
    // Keep at most MAX_INACTIVE_PODS inactive (terminating + terminated) pods total.
    if let Some(prev_pods) = prev_controller_metadata
        .get("pod_status")
        .and_then(|ps| ps.get("pods"))
        .and_then(|p| p.as_array())
    {
        let live_pod_names: std::collections::HashSet<String> = pod_infos
            .iter()
            .filter_map(|p| p.get("name").and_then(|n| n.as_str()).map(String::from))
            .collect();

        // Count live terminating pods already in the list
        let mut inactive_count = pod_infos
            .iter()
            .filter(|p| {
                p.get("terminating")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
            })
            .count();

        for prev_pod in prev_pods {
            if inactive_count >= MAX_INACTIVE_PODS {
                break;
            }
            let was_terminating = prev_pod
                .get("terminating")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let was_terminated = prev_pod
                .get("terminated")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let name = prev_pod.get("name").and_then(|n| n.as_str()).unwrap_or("");

            if (was_terminating || was_terminated)
                && !name.is_empty()
                && !live_pod_names.contains(name)
            {
                // Pod is gone from K8s — carry forward with terminated: true
                let mut carried = prev_pod.clone();
                if let Some(obj) = carried.as_object_mut() {
                    obj.insert("terminating".to_string(), serde_json::Value::Bool(false));
                    obj.insert("terminated".to_string(), serde_json::Value::Bool(true));
                }
                pod_infos.push(carried);
                inactive_count += 1;
            }
        }
    }

    let pod_status = serde_json::json!({
        "desired_replicas": desired_replicas,
        "ready_replicas": ready_replicas,
        "current_replicas": current_replicas,
        "pods": pod_infos,
        "last_checked": Utc::now().to_rfc3339(),
    });

    PodCheckResult {
        has_error,
        error_message,
        pod_status: Some(pod_status),
    }
}

/// Handle a deployment becoming Healthy: mark active, supersede old deployments.
async fn handle_deployment_became_healthy(
    state: &AppState,
    deployment: &Deployment,
    project: &Project,
) -> anyhow::Result<()> {
    // Find currently active deployment in this group BEFORE marking new as Healthy
    let active_in_group = db_deployments::find_active_for_project_and_group(
        &state.db_pool,
        project.id,
        &deployment.deployment_group,
    )
    .await?;

    // Mark the new deployment as healthy
    db_deployments::mark_healthy(&state.db_pool, deployment.id).await?;

    // Supersede the old active deployment
    if let Some(old_active) = active_in_group {
        if old_active.id != deployment.id && !state_machine::is_terminal(&old_active.status) {
            info!(
                "Deployment {} replacing {} in group '{}', marking old as Terminating",
                deployment.deployment_id, old_active.deployment_id, deployment.deployment_group
            );
            db_deployments::mark_terminating(
                &state.db_pool,
                old_active.id,
                TerminationReason::Superseded,
            )
            .await?;
        }
    }

    // Clean up other active (Healthy/Unhealthy) deployments in the group
    let others = db_deployments::find_non_terminal_for_project_and_group(
        &state.db_pool,
        project.id,
        &deployment.deployment_group,
    )
    .await?;

    for other in others {
        if other.id != deployment.id
            && state_machine::is_active(&other.status)
            && !state_machine::is_terminal(&other.status)
        {
            info!(
                "Cleaning up non-active deployment {} in group '{}', marking as Terminating",
                other.deployment_id, deployment.deployment_group
            );
            db_deployments::mark_terminating(
                &state.db_pool,
                other.id,
                TerminationReason::Superseded,
            )
            .await?;
        }
    }

    // Mark deployment as active
    db_deployments::mark_as_active(
        &state.db_pool,
        deployment.id,
        project.id,
        &deployment.deployment_group,
    )
    .await?;

    db_projects::update_calculated_status(&state.db_pool, project.id).await?;

    Ok(())
}

/// Check for expired deployments
async fn check_expirations(
    state: &AppState,
    non_terminal: &[&Deployment],
    project: &Project,
) -> anyhow::Result<()> {
    let now = Utc::now();
    for deployment in non_terminal {
        if let Some(expires_at) = deployment.expires_at {
            if now > expires_at
                && !matches!(
                    deployment.status,
                    DeploymentStatus::Terminating | DeploymentStatus::Cancelling
                )
            {
                info!(
                    deployment_id = %deployment.deployment_id,
                    "Deployment has expired, marking as Terminating"
                );
                db_deployments::mark_terminating(
                    &state.db_pool,
                    deployment.id,
                    TerminationReason::Expired,
                )
                .await?;
                db_projects::update_calculated_status(&state.db_pool, project.id).await?;
            }
        }
    }
    Ok(())
}

// ── Compute desired children ───────────────────────────────────────────

/// Compute the desired set of Kubernetes child resources for a project.
///
/// Resources NOT returned will be deleted by Metacontroller (garbage collection).
async fn compute_desired_children(
    state: &AppState,
    resource_builder: &ResourceBuilder,
    project: &Project,
    all_deployments: &[Deployment],
    observed: &ObservedChildren,
    namespace_prefix: &str,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let mut children: Vec<serde_json::Value> = Vec::new();
    let namespace = ResourceBuilder::namespace_name(project, namespace_prefix);

    // Preload all environments for this project to avoid per-deployment DB lookups
    let environments: HashMap<uuid::Uuid, crate::db::models::Environment> =
        db_environments::list_for_project(&state.db_pool, project.id)
            .await?
            .into_iter()
            .map(|env| (env.id, env))
            .collect();

    // 1. Namespace (always)
    let ns = resource_builder.create_namespace(project, namespace_prefix);
    children.push(serde_json::to_value(&ns)?);

    // 2. Image pull secret (if needed)
    if resource_builder.image_pull_secret_name.is_none()
        && resource_builder.registry_provider.requires_pull_secret()
    {
        if let Some(secret) =
            build_image_pull_secret(resource_builder, project, &namespace, observed).await?
        {
            children.push(serde_json::to_value(&secret)?);
        }
    }

    // 3. Backend service + endpoints (if configured)
    if let Some(ref backend_address) = resource_builder.backend_address {
        add_backend_resources(
            state,
            &mut children,
            resource_builder,
            project,
            &namespace,
            backend_address,
        )
        .await?;
    }

    // Collect deployments that should have K8s infrastructure
    let mut infra_deployments: Vec<&Deployment> = all_deployments
        .iter()
        .filter(|d| should_have_infrastructure(d))
        .collect();

    // 4. Per-environment ServiceAccounts
    let mut seen_environments: std::collections::HashSet<uuid::Uuid> =
        std::collections::HashSet::new();
    for deployment in &infra_deployments {
        if let Some(env_id) = deployment.environment_id {
            if seen_environments.insert(env_id) {
                if let Some(environment) = environments.get(&env_id) {
                    // Check if we should skip SA creation for production
                    if resource_builder.use_default_service_account_for_production
                        && environment.is_production
                    {
                        continue;
                    }
                    let sa = resource_builder.create_service_account(
                        project,
                        &environment.name,
                        &namespace,
                    );
                    children.push(serde_json::to_value(&sa)?);
                }
            }
        }
    }

    // 5. K8s Deployments, Services, Ingresses, NetworkPolicies per deployment/group
    // Track which deployment groups have active deployments that are ready for phase 2.
    let mut active_by_group: HashMap<String, &Deployment> = HashMap::new();

    // Preload the normalized container + route list per deployment. A
    // single-container deployment (`containers IS NULL`) synthesises an implicit
    // `app` container from the row's columns; multi-container deployments parse
    // their persisted side-data. Loading once here drives Service ownership and
    // avoids a duplicate DB roundtrip in phase 2.
    let mut container_specs_by_deployment: HashMap<
        uuid::Uuid,
        Vec<crate::server::deployment::models::ContainerSpec>,
    > = HashMap::new();
    let mut routes_by_deployment: HashMap<
        uuid::Uuid,
        Vec<crate::server::deployment::models::RouteSpec>,
    > = HashMap::new();
    // `containers`/`routes` come folded onto each row. An unparseable (non-NULL
    // but corrupt) side-data column fails ONLY that deployment: mark it Failed,
    // log, and drop it from `infra_deployments` so it contributes no children
    // (Metacontroller then GCs its on-cluster resources). NEVER propagate the
    // error up — that would fail the whole project sync.
    let mut failed_to_parse: std::collections::HashSet<uuid::Uuid> =
        std::collections::HashSet::new();
    for d in &infra_deployments {
        match resolve_runtime_containers(d) {
            Ok((specs, routes)) => {
                container_specs_by_deployment.insert(d.id, specs);
                routes_by_deployment.insert(d.id, routes);
            }
            Err(e) => {
                error!(
                    deployment_id = %d.deployment_id,
                    "container side-data could not be deserialized; marking deployment Failed: {:?}", e
                );
                db_deployments::mark_failed(
                    &state.db_pool,
                    d.id,
                    "container side-data could not be deserialized",
                )
                .await?;
                failed_to_parse.insert(d.id);
            }
        }
    }
    if !failed_to_parse.is_empty() {
        db_projects::update_calculated_status(&state.db_pool, project.id).await?;
        infra_deployments.retain(|d| !failed_to_parse.contains(&d.id));
    }

    // Per-container Service ownership: for each (group, container_name), which
    // deployment's labels/port the canonical Service points at.
    //
    // Rule:
    //   - If the active deployment has this container, it owns the Service.
    //     (Preserves blue-green: shared containers keep their Service pinned
    //     to the active deployment_id until a new deployment becomes active,
    //     at which point the Service spec swaps atomically.)
    //   - Otherwise, the most-recent deployment that has the container owns
    //     it. (Lets a rolling-out deployment that ADDS a new container — e.g.
    //     a sidecar Redis — emit its Service immediately, so sibling pods can
    //     reach it during their readiness probes.)
    //
    // Workers (no `port`) are absent from this map. A single-container app
    // participates via its implicit `app` container.
    let service_owner_per_container =
        compute_service_owner_per_container(&infra_deployments, &container_specs_by_deployment);

    // Helper: look up environment name from preloaded cache
    let env_name_for = |deployment: &Deployment| -> Option<String> {
        deployment
            .environment_id
            .and_then(|id| environments.get(&id))
            .map(|env| env.name.clone())
    };

    // K8s Deployments — one per non-terminal, infrastructure-bearing deployment
    for deployment in &infra_deployments {
        let env_name = env_name_for(deployment);
        let env_vars = load_env_vars(state, project, deployment).await?;

        // Container spec list for this deployment. A single-container deployment
        // has its synthesised `app`; a multi-container one has one entry per
        // `[containers.<name>]`. Computed here (before the env-secret guard) so
        // the guard can look up the per-container K8s Deployment names.
        let container_specs: Vec<crate::server::deployment::models::ContainerSpec> =
            container_specs_by_deployment
                .get(&deployment.id)
                .cloned()
                .unwrap_or_default();

        let secret_env = if env_vars.secret_env_vars.is_empty() {
            None
        } else {
            Some(prepare_deployment_env_secret(
                resource_builder,
                project,
                deployment,
                &namespace,
                env_name.as_deref(),
                &observed.secrets,
                env_vars.secret_env_vars,
            ))
        };

        if let Some(secret_env) = secret_env.as_ref() {
            let secret_name = secret_env.secret_name.clone();
            let is_ready = secret_env.is_ready;
            children.push(serde_json::to_value(&secret_env.secret)?);

            if !is_ready {
                // Observed Deployments are keyed `<namespace>/<name>` and each
                // container emits `<project>-<deployment_id>-<container>` (see
                // the health path). The deployment counts as already-observed if
                // ANY of its container Deployments exists — using the bare
                // `<project>-<deployment_id>` key (no namespace, no container
                // suffix) never matches, so the guard would always withhold the
                // Deployment and let Metacontroller GC it on every secret/env
                // rotation.
                let base_name = ResourceBuilder::deployment_name(project, deployment);
                let deploy_already_observed = any_container_deployment_observed(
                    observed,
                    &namespace,
                    &base_name,
                    &container_specs,
                );

                // Defer Deployment creation until the Secret is observed, preventing
                // pods from starting with a missing env secret. However, if a
                // Deployment already exists (e.g. the secret was rotated or
                // transiently removed), keep returning it so Metacontroller does
                // not garbage-collect it while the new secret is being created.
                if !deploy_already_observed {
                    debug!(
                        deployment_id = %deployment.deployment_id,
                        secret_name,
                        "Waiting for deployment env secret before returning Deployment"
                    );
                    continue;
                }

                debug!(
                    deployment_id = %deployment.deployment_id,
                    secret_name,
                    "Deployment env secret not ready but Deployment already exists; returning Deployment to prevent GC"
                );
            }
        }

        let (secret_env_name, secret_env_hash) = match secret_env {
            Some(secret_env) => (Some(secret_env.secret_name), Some(secret_env.secret_hash)),
            None => (None, None),
        };

        // Resolve image
        let source_deployment_id =
            if let Some(source_id) = deployment.rolled_back_from_deployment_id {
                db_deployments::find_by_id(&state.db_pool, source_id)
                    .await?
                    .map(|d| d.deployment_id)
            } else {
                None
            };
        let image =
            resource_builder.resolve_image(project, deployment, source_deployment_id.as_deref());

        // Resolve service account name from preloaded environments
        let sa_name = {
            let env = deployment
                .environment_id
                .and_then(|id| environments.get(&id));
            match env {
                Some(environment)
                    if resource_builder.use_default_service_account_for_production
                        && environment.is_production =>
                {
                    None
                }
                Some(environment) => Some(ResourceBuilder::environment_service_account_name(
                    &environment.name,
                )),
                None => None,
            }
        };

        // Workload identity Secret: bootstrap credential + auto-minted tokens.
        let identity = prepare_identity_secret(
            state,
            resource_builder,
            project,
            deployment,
            &namespace,
            env_name.as_deref(),
            observed,
        )
        .await?;
        children.push(serde_json::to_value(&identity.secret)?);

        // Emit one K8s Deployment per container. A single-container deployment
        // has a single synthesised `app` container (see `resolve_runtime_containers`);
        // a multi-container deployment has one entry per `[containers.<name>]`.
        // - The shared workload-identity Secret is mounted once per pod spec.
        // - Global env vars apply to every container; per-container overrides
        //   come from `ContainerSpec.env_overrides`.
        // - Resource names are suffixed with `-<container>` (see
        //   `ResourceBuilder::deployment_name_for_container`).
        // `container_specs` was computed above (before the env-secret guard).

        // Cross-container service-discovery vars — only meaningful with ≥2
        // containers, so a single-container app doesn't get a pointless
        // self-entry. User globals (in `plain_env_vars`) and per-container
        // `env_overrides` take precedence if they set the same name.
        let injected_host_env = if container_specs.len() >= 2 {
            ResourceBuilder::auto_container_host_env_vars(deployment, &container_specs)
        } else {
            Vec::new()
        };

        for spec in &container_specs {
            let mut per_container_env = env_vars.plain_env_vars.clone();
            for var in &injected_host_env {
                if per_container_env.iter().any(|v| v.name == var.name) {
                    continue;
                }
                per_container_env.push(var.clone());
            }
            for over in &spec.env_overrides {
                // Per-container secret overrides are rejected at request time
                // (see validate_containers_and_routes); this should never fire.
                // If it does, the right answer is to fix the request handler,
                // not silently route around it.
                debug_assert!(
                    !over.is_secret,
                    "per-container secret env override leaked past validation: {}",
                    over.key
                );
                if over.is_secret {
                    continue;
                }
                // Skip overrides whose `for_environment` doesn't match the
                // deployment's resolved environment. Mirrors `apply_env_overrides`.
                if let Some(ref target_env) = over.for_environment {
                    if env_name.as_deref() != Some(target_env.as_str()) {
                        continue;
                    }
                }
                // Replace if present, otherwise append.
                if let Some(existing) = per_container_env.iter_mut().find(|v| v.name == over.key) {
                    existing.value = Some(over.value.clone());
                } else {
                    per_container_env.push(k8s_openapi::api::core::v1::EnvVar {
                        name: over.key.clone(),
                        value: Some(over.value.clone()),
                        ..Default::default()
                    });
                }
            }

            // Pin `PORT` to this container's declared port (see
            // `apply_container_port_env`).
            apply_container_port_env(&mut per_container_env, spec.port);

            let health_check = spec.health_check.clone();

            let runtime = crate::server::deployment::resource_builder::ContainerRuntime {
                name: &spec.name,
                // Synthesised `app` carries no image; fall back to the
                // deployment's resolved image. Explicit containers always have one.
                image: spec.image.as_deref().unwrap_or(&image),
                port: spec.port,
                replicas: spec
                    .replicas
                    .map(|r| r as i32)
                    .unwrap_or(deployment.replicas),
                cpu: spec.cpu.as_deref().unwrap_or(&deployment.cpu),
                memory: spec.memory.as_deref().unwrap_or(&deployment.memory),
                env_vars: per_container_env,
                secret_env_name: secret_env_name.clone(),
                secret_env_hash: secret_env_hash.clone(),
                health_check,
            };

            let k8s_deploy = resource_builder.create_k8s_deployment_for_container(
                project,
                deployment,
                &namespace,
                &runtime,
                sa_name.clone(),
                env_name.as_deref(),
                Some(identity.mount.clone()),
            );
            children.push(serde_json::to_value(&k8s_deploy)?);

            // Emit the per-container Service iff THIS deployment owns it for
            // (group, container) — see `service_owner_per_container`. Exactly one
            // Service per (group, container) is emitted, preserving blue-green.
            let owns_service = service_owner_per_container
                .get(&(deployment.deployment_group.clone(), spec.name.clone()))
                == Some(&deployment.id);
            if owns_service {
                if let Some(service) = resource_builder.create_service_for_container(
                    project,
                    deployment,
                    &namespace,
                    &runtime,
                    env_name.as_deref(),
                ) {
                    children.push(serde_json::to_value(&service)?);
                }
            }
        }

        if deployment.is_active {
            active_by_group.insert(deployment.deployment_group.clone(), deployment);
        }
    }

    // Services, Ingresses, NetworkPolicies — one per group with an active deployment
    let custom_domains =
        crate::db::custom_domains::list_project_custom_domains(&state.db_pool, project.id).await?;
    let valid_custom_domains = resource_builder.filter_valid_custom_domains(&custom_domains);

    // Index custom domains by environment_id.
    let mut domains_by_env: HashMap<uuid::Uuid, Vec<crate::db::models::CustomDomain>> =
        HashMap::new();
    for cd in &valid_custom_domains {
        domains_by_env
            .entry(cd.environment_id)
            .or_default()
            .push(cd.clone());
    }

    // Index environments by the deployment group they're primary for. The schema
    // enforces (project_id, primary_deployment_group) uniqueness, so at most one
    // env per group.
    let mut env_by_primary_group: HashMap<String, &crate::db::models::Environment> = HashMap::new();
    for env in environments.values() {
        if let Some(group) = env.primary_deployment_group.as_deref() {
            env_by_primary_group.insert(group.to_string(), env);
        }
    }

    // If the operator configured custom_domain_ingress_annotations, custom domains
    // continue to ride on a sibling ingress so those annotations only apply there.
    // Otherwise we fold them into the primary ingress.
    let split_custom_domains = !resource_builder
        .custom_domain_ingress_annotations
        .is_empty();

    // Full env list for `create_primary_ingress` to do cross-ingress host
    // collision checks (a DG URL that matches another env's URL is suppressed).
    let all_environments: Vec<crate::db::models::Environment> =
        environments.values().cloned().collect();

    for (group, active_deployment) in &active_by_group {
        let env_name = env_name_for(active_deployment);
        let env_for_group = env_by_primary_group.get(group.as_str()).copied();
        let domains_for_group: Vec<crate::db::models::CustomDomain> = env_for_group
            .and_then(|env| domains_by_env.get(&env.id).cloned())
            .unwrap_or_default();

        // All Services (including the single-container `app`) are emitted in
        // phase 1 alongside each container's K8s Deployment via the ownership
        // map — so a new container declared by a rolling-out, not-yet-active
        // deployment gets its Service immediately (sibling pods can reach it
        // during probes). Here we only build the ingress route table.
        let active_container_specs: Vec<crate::server::deployment::models::ContainerSpec> =
            container_specs_by_deployment
                .get(&active_deployment.id)
                .cloned()
                .unwrap_or_default();

        // Primary Ingress
        let inline_domains: &[crate::db::models::CustomDomain] = if split_custom_domains {
            &[]
        } else {
            &domains_for_group
        };
        // Build the `(path, service_name)` ingress route table from the
        // preloaded route side-data (no extra DB roundtrip). Routes targeting a
        // non-routable container are filtered out. A single-container app yields
        // `[("/", "<group>-app")]`; a workers-only deployment yields an empty
        // table → no ingress.
        let routable_names: std::collections::HashSet<&str> = active_container_specs
            .iter()
            .filter(|s| s.port.is_some())
            .map(|s| s.name.as_str())
            .collect();
        let service_name_base = ResourceBuilder::service_name(project, active_deployment);
        let routes: Vec<(String, String)> = routes_by_deployment
            .get(&active_deployment.id)
            .map(|route_specs| {
                route_specs
                    .iter()
                    .filter(|r| routable_names.contains(r.container.as_str()))
                    .map(|r| {
                        (
                            r.path.clone(),
                            format!("{}-{}", service_name_base, r.container),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();

        // A workers-only deployment (no routable container) emits no ingress.
        if !routes.is_empty() {
            if let Some(ingress) = resource_builder.create_primary_ingress(
                project,
                active_deployment,
                &namespace,
                env_for_group,
                inline_domains,
                env_name.as_deref(),
                &all_environments,
                &routes,
            )? {
                children.push(serde_json::to_value(&ingress)?);
            }
            // When `create_primary_ingress` returns `None`, every candidate host for
            // this group collided with another env's URL (e.g. a deployment group
            // named the same as an environment whose primary group is different).
            // Skip the ingress so nginx admission doesn't reject it; the deployment
            // still runs and a warning was logged in `create_primary_ingress`.

            // Sibling custom-domain ingress, only when carve-out annotations are set.
            if split_custom_domains && !domains_for_group.is_empty() {
                let custom_ingress = resource_builder.create_custom_domain_ingress(
                    project,
                    active_deployment,
                    &namespace,
                    &domains_for_group,
                    env_name.as_deref(),
                    &routes,
                )?;
                children.push(serde_json::to_value(&custom_ingress)?);
            }
        }

        // NetworkPolicy
        let np = resource_builder.create_network_policy(
            project,
            active_deployment,
            &namespace,
            env_name.as_deref(),
        );
        children.push(serde_json::to_value(&np)?);
    }

    Ok(children)
}

/// Returns true if ANY of a deployment's per-container K8s Deployments has been
/// observed. Observed Deployments are keyed `<namespace>/<name>`, and each
/// container emits `<project>-<deployment_id>-<container>` (see the health
/// path). Using the bare `<project>-<deployment_id>` key — no namespace, no
/// container suffix — never matches, so callers that relied on it would always
/// treat the deployment as un-observed.
fn any_container_deployment_observed(
    observed: &ObservedChildren,
    namespace: &str,
    base_name: &str,
    container_specs: &[crate::server::deployment::models::ContainerSpec],
) -> bool {
    container_specs.iter().any(|spec| {
        let key = format!("{}/{}-{}", namespace, base_name, spec.name);
        observed.deployments.contains_key(&key)
    })
}

/// Force the `PORT` env var to match a container's declared port.
///
/// Each container listens on its own declared port — the same port used for the
/// K8s `containerPort`, the Service, the health probes, and the
/// `RISE_CONTAINER_HOST__*` discovery vars. The `PORT` env var must therefore
/// reflect *this* container's port, not the deployment-wide `http_port` carried
/// by the global env vars. Without this, a multi-container app (e.g. a frontend
/// on 3000 next to a service on 8000) would hand every process the same global
/// `PORT` (8080 by default), so each app binds the wrong port.
///
/// `PORT` can't be set via per-container `env_overrides` (rejected at request
/// time), so the declared port is the single source of truth. Containers with no
/// declared port (workers) keep whatever global `PORT` was set, if any.
fn apply_container_port_env(env: &mut Vec<EnvVar>, container_port: Option<u16>) {
    let Some(container_port) = container_port else {
        return;
    };
    let port_value = container_port.to_string();
    if let Some(existing) = env.iter_mut().find(|v| v.name == "PORT") {
        existing.value = Some(port_value);
    } else {
        env.push(EnvVar {
            name: "PORT".to_string(),
            value: Some(port_value),
            ..Default::default()
        });
    }
}

/// Resolve the container + route list the reconciler should emit for a
/// deployment. A multi-container deployment parses its persisted side-data; a
/// single-container deployment (`containers IS NULL`) synthesises an implicit
/// `app` container from the row's columns plus a `/` route. The synthesised
/// `app` carries no image (`image: None`) — the reconciler fills it from the
/// deployment's resolved image, so the existing single-container image (tagged
/// with the deployment ID) is reused with no rebuild.
///
/// `containers`/`routes` come folded onto the [`Deployment`] row. A `None`
/// `containers` column is a legitimate single-container deployment and triggers
/// the synthesised `app`. A `Some` column that fails to deserialize is treated
/// as a hard error for *this* deployment (returned `Err`): silently collapsing
/// it to the single-container fallback would drop every per-container resource.
pub(crate) fn resolve_runtime_containers(
    deployment: &Deployment,
) -> anyhow::Result<(
    Vec<crate::server::deployment::models::ContainerSpec>,
    Vec<crate::server::deployment::models::RouteSpec>,
)> {
    use crate::server::deployment::models::{decode_side_data, ContainerSpec, RouteSpec};

    if let Some(containers_value) = deployment.containers.as_ref() {
        let specs: Vec<ContainerSpec> = decode_side_data(containers_value).with_context(|| {
            format!(
                "deployment {} ({}) has a non-NULL `containers` column that could not be \
                 decoded into Vec<ContainerSpec>",
                deployment.id, deployment.deployment_id
            )
        })?;
        if specs.is_empty() {
            anyhow::bail!(
                "deployment {} ({}) has a non-NULL `containers` column with an empty items list; \
                 this is a degenerate state that should have been rejected at write time",
                deployment.id,
                deployment.deployment_id
            );
        }
        let routes: Vec<RouteSpec> = match deployment.routes.as_ref() {
            Some(routes_value) => decode_side_data(routes_value).with_context(|| {
                format!(
                    "deployment {} ({}) has a non-NULL `routes` column that could not be \
                     decoded into Vec<RouteSpec>",
                    deployment.id, deployment.deployment_id
                )
            })?,
            None => Vec::new(),
        };
        return Ok((specs, routes));
    }

    // Single-container deployment: synthesise the implicit `app` container.
    // A single-container deployment always has an http port (NOT NULL, default
    // 8080), so it is always routable at `/`.
    let port = deployment.http_port as u16;
    let app = ContainerSpec {
        name: crate::rise_toml::DEFAULT_CONTAINER_NAME.to_string(),
        image: None,
        port: Some(port),
        replicas: Some(deployment.replicas as u32),
        cpu: Some(deployment.cpu.clone()),
        memory: Some(deployment.memory.clone()),
        env_overrides: Vec::new(),
        health_check: None,
    };
    let routes = vec![RouteSpec {
        path: "/".to_string(),
        container: crate::rise_toml::DEFAULT_CONTAINER_NAME.to_string(),
    }];
    Ok((vec![app], routes))
}

/// Returns true if this deployment should have K8s infrastructure (K8s Deployment resource).
/// Compute Service ownership per (group, container_name) across the project's
/// in-play deployments. The owner is the deployment whose labels/port the
/// canonical Service `<group>-<container>` points at.
///
/// Rule:
///   - The active deployment owns every routable container it declares.
///     Shared containers stay pinned to the active deployment_id until a new
///     deployment becomes active, preserving the atomic blue-green swap.
///   - Among non-active deployments, the most-recent one wins for any
///     routable container the active doesn't declare. This lets a rolling-out
///     deployment that adds a new sidecar container (e.g. Redis) emit its
///     Service immediately, so sibling pods can reach it during probes.
///
/// Workers (no `port`) are absent from the result. A single-container app
/// participates via its implicit `app` container.
pub(crate) fn compute_service_owner_per_container(
    infra_deployments: &[&Deployment],
    container_specs_by_deployment: &HashMap<
        uuid::Uuid,
        Vec<crate::server::deployment::models::ContainerSpec>,
    >,
) -> HashMap<(String, String), uuid::Uuid> {
    let mut owner: HashMap<(String, String), uuid::Uuid> = HashMap::new();

    let mut by_group: HashMap<&str, Vec<&Deployment>> = HashMap::new();
    for d in infra_deployments {
        by_group
            .entry(d.deployment_group.as_str())
            .or_default()
            .push(d);
    }

    for (group, ds_in_group) in &by_group {
        // Active wins for every container it declares.
        if let Some(active) = ds_in_group.iter().find(|d| d.is_active) {
            if let Some(specs) = container_specs_by_deployment.get(&active.id) {
                for spec in specs {
                    if spec.port.is_none() {
                        continue;
                    }
                    owner.insert(((*group).to_string(), spec.name.clone()), active.id);
                }
            }
        }
        // Walk non-active deployments newest-first; assign routable containers
        // not yet owned to the most-recent deployment that declares them.
        let mut others: Vec<&Deployment> = ds_in_group
            .iter()
            .filter(|d| !d.is_active)
            .copied()
            .collect();
        others.sort_by(|a, b| b.deployment_id.cmp(&a.deployment_id));
        for d in others {
            if let Some(specs) = container_specs_by_deployment.get(&d.id) {
                for spec in specs {
                    if spec.port.is_none() {
                        continue;
                    }
                    owner
                        .entry(((*group).to_string(), spec.name.clone()))
                        .or_insert(d.id);
                }
            }
        }
    }

    owner
}

pub(crate) fn should_have_infrastructure(deployment: &Deployment) -> bool {
    matches!(
        deployment.status,
        DeploymentStatus::Pushed
            | DeploymentStatus::Deploying
            | DeploymentStatus::Healthy
            | DeploymentStatus::Unhealthy
            | DeploymentStatus::Terminating
    )
}

/// Build image pull secret, refreshing if stale.
async fn build_image_pull_secret(
    resource_builder: &ResourceBuilder,
    project: &Project,
    namespace: &str,
    observed: &ObservedChildren,
) -> anyhow::Result<Option<Secret>> {
    // Metacontroller keys namespaced children of cluster-scoped parents as "namespace/name"
    let secret_key = format!("{}/{}", namespace, IMAGE_PULL_SECRET_NAME);

    // Check if existing secret is fresh enough
    let needs_refresh = match observed.secrets.get(&secret_key) {
        Some(secret_json) => {
            let last_refresh = secret_json
                .get("metadata")
                .and_then(|m| m.get("annotations"))
                .and_then(|a| a.get(ANNOTATION_LAST_REFRESH))
                .and_then(|v| v.as_str())
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());

            match last_refresh {
                Some(ts) => {
                    let age = Utc::now().signed_duration_since(ts.with_timezone(&Utc));
                    age > chrono::Duration::hours(SECRET_REFRESH_HOURS)
                }
                None => true, // No annotation → refresh
            }
        }
        None => true, // Not observed → create
    };

    if !needs_refresh {
        // Return a clean secret with only the fields we control, using the existing
        // data and last-refresh annotation from the observed secret. Echoing back
        // the full observed JSON would include server-set fields (managedFields,
        // resourceVersion, etc.) that change on every update, creating a perpetual
        // diff loop with Metacontroller's last-applied-configuration.
        if let Some(secret_json) = observed.secrets.get(&secret_key) {
            let existing: Secret = serde_json::from_value(secret_json.clone())?;
            let last_refresh = existing
                .metadata
                .annotations
                .as_ref()
                .and_then(|a| a.get(ANNOTATION_LAST_REFRESH))
                .cloned()
                .unwrap_or_default();
            let mut annotations = std::collections::BTreeMap::new();
            annotations.insert(ANNOTATION_LAST_REFRESH.to_string(), last_refresh);
            return Ok(Some(Secret {
                metadata: ObjectMeta {
                    name: Some(IMAGE_PULL_SECRET_NAME.to_string()),
                    namespace: Some(namespace.to_string()),
                    annotations: Some(annotations),
                    ..Default::default()
                },
                type_: existing.type_,
                data: existing.data,
                ..Default::default()
            }));
        }
    }

    // Fetch fresh pull credentials (scoped to this project's repository)
    let credentials = resource_builder
        .registry_provider
        .get_k8s_pull_credentials(&project.name)
        .await?;
    let registry_host = resource_builder.registry_provider.registry_host();

    let secret = resource_builder.create_dockerconfigjson_secret(
        IMAGE_PULL_SECRET_NAME,
        namespace,
        registry_host,
        &credentials,
    )?;

    Ok(Some(secret))
}

/// A prepared workload-identity Secret plus the mount description for the pod.
struct PreparedIdentitySecret {
    secret: Secret,
    mount: IdentityMount,
}

/// Build the per-deployment workload-identity Secret.
///
/// The bootstrap credential is generated once and then reused from the observed
/// Secret on subsequent syncs. Its SHA-256 hash is persisted to the deployment
/// row only once the credential has been read back from the *observed* Secret —
/// never in the generate-fresh path — so that with multiple controller replicas
/// the persisted hash always matches the credential in the Secret that was
/// actually applied. Auto-minted workload tokens (one per `[identity]` audience)
/// are re-minted only when the observed Secret is stale or its audience set
/// changed, avoiding a Secret rewrite on every resync.
async fn prepare_identity_secret(
    state: &AppState,
    resource_builder: &ResourceBuilder,
    project: &Project,
    deployment: &Deployment,
    namespace: &str,
    environment_name: Option<&str>,
    observed: &ObservedChildren,
) -> anyhow::Result<PreparedIdentitySecret> {
    use base64::Engine;
    use rand::Rng;

    let secret_name = ResourceBuilder::deployment_identity_secret_name(project, deployment);
    let secret_key = format!("{}/{}", namespace, secret_name);

    let observed_secret: Option<Secret> = observed
        .secrets
        .get(&secret_key)
        .and_then(|json| serde_json::from_value(json.clone()).ok());

    // ── Bootstrap credential: generate once, then reuse ─────────────────
    let observed_credential = observed_secret
        .as_ref()
        .and_then(|s| s.data.as_ref())
        .and_then(|d| d.get(IDENTITY_CREDENTIAL_KEY))
        .and_then(|b| String::from_utf8(b.0.clone()).ok());

    let credential = match observed_credential {
        Some(c) => {
            // HA-safe: only the credential read back from the *observed* Secret is
            // authoritative. Persist its hash so the token-exchange endpoint can
            // authenticate the app. This is idempotent — every replica observes the
            // same applied Secret and writes the same hash.
            let credential_hash = sha256_hex(c.as_bytes());
            if deployment.identity_credential_hash.as_deref() != Some(credential_hash.as_str()) {
                db_deployments::set_identity_credential_hash(
                    &state.db_pool,
                    deployment.id,
                    &credential_hash,
                )
                .await
                .context("Failed to persist identity credential hash")?;
            }
            c
        }
        None => {
            // No Secret observed yet: generate a fresh credential but do NOT persist
            // its hash. With multiple replicas behind the webhook Service, two
            // replicas could generate different credentials; persisting here risks
            // a DB hash that does not match the Secret that actually got applied.
            // The hash is written on the next sync via the reuse path above, once
            // some replica's Secret has been applied and is observed by all.
            let mut bytes = [0u8; 32];
            rand::rng().fill_bytes(&mut bytes);
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
        }
    };

    // ── Auto-minted workload tokens ─────────────────────────────────────
    let audiences: BTreeMap<String, String> =
        serde_json::from_value(deployment.identity_audiences.clone()).unwrap_or_default();

    let mut token_filenames: Vec<String> = audiences.keys().cloned().collect();
    token_filenames.sort();

    // The Secret's last-refresh timestamp from the previous reconcile, if any.
    let observed_refresh = observed_secret
        .as_ref()
        .and_then(|s| s.metadata.annotations.as_ref())
        .and_then(|a| a.get(ANNOTATION_LAST_REFRESH))
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|ts| ts.with_timezone(&Utc));

    let (tokens, refreshed_at) = if audiences.is_empty() {
        // Nothing to mint — reuse the observed last-refresh timestamp so the
        // annotation does not churn on every (~5s) reconcile, which would cause
        // constant Secret rewrites. Fall back to now only when no Secret exists.
        (BTreeMap::new(), observed_refresh.unwrap_or_else(Utc::now))
    } else {
        let observed_tokens: BTreeMap<String, String> = observed_secret
            .as_ref()
            .and_then(|s| s.data.as_ref())
            .map(|d| {
                d.iter()
                    .filter_map(|(k, v)| {
                        let name = k.strip_prefix(IDENTITY_TOKEN_KEY_PREFIX)?;
                        let jwt = String::from_utf8(v.0.clone()).ok()?;
                        Some((name.to_string(), jwt))
                    })
                    .collect()
            })
            .unwrap_or_default();

        let ttl_secs = state.identity_token_ttl_seconds;
        let refresh_secs = (ttl_secs / 2) as i64;

        let fresh = observed_refresh
            .map(|ts| {
                Utc::now().signed_duration_since(ts) < chrono::Duration::seconds(refresh_secs)
            })
            .unwrap_or(false);

        if fresh && observed_tokens.keys().eq(audiences.keys()) {
            // Reuse observed tokens unchanged, preserving the refresh timestamp.
            (observed_tokens, observed_refresh.unwrap_or_else(Utc::now))
        } else {
            let sub = workload_subject(&project.name, environment_name);
            let subject_info = WorkloadSubjectInfo {
                sub: &sub,
                project: &project.name,
                environment: environment_name.unwrap_or(NO_ENVIRONMENT),
                deployment_group: &deployment.deployment_group,
                deployment_id: &deployment.deployment_id,
            };
            let mut minted = BTreeMap::new();
            for (filename, audience) in &audiences {
                let jwt = state
                    .jwt_signer
                    .sign_workload_jwt(&subject_info, audience, ttl_secs)?;
                minted.insert(filename.clone(), jwt);
            }
            (minted, Utc::now())
        }
    };

    let secret = resource_builder.create_identity_secret(
        project,
        deployment,
        namespace,
        environment_name,
        &credential,
        &tokens,
        refreshed_at,
    );

    Ok(PreparedIdentitySecret {
        secret,
        mount: IdentityMount {
            secret_name,
            token_filenames,
        },
    })
}

fn hash_deployment_env_secret(data: &BTreeMap<String, ByteString>) -> String {
    let mut hasher = Sha256::new();

    for (key, value) in data {
        let key_len = key.len() as u64;
        let value_len = value.0.len() as u64;
        hasher.update(key_len.to_le_bytes());
        hasher.update(key.as_bytes());
        hasher.update(value_len.to_le_bytes());
        hasher.update(&value.0);
    }

    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn observed_secret_matches_hash(
    observed_secrets: &HashMap<String, serde_json::Value>,
    namespace: &str,
    secret_name: &str,
    expected_hash: &str,
) -> bool {
    let secret_key = format!("{}/{}", namespace, secret_name);
    observed_secrets
        .get(&secret_key)
        .and_then(|secret| secret.get("metadata"))
        .and_then(|metadata| metadata.get("annotations"))
        .and_then(|annotations| annotations.get(ANNOTATION_ENV_SECRET_HASH))
        .and_then(|hash| hash.as_str())
        == Some(expected_hash)
}

fn prepare_deployment_env_secret(
    resource_builder: &ResourceBuilder,
    project: &Project,
    deployment: &Deployment,
    namespace: &str,
    environment_name: Option<&str>,
    observed_secrets: &HashMap<String, serde_json::Value>,
    data: BTreeMap<String, ByteString>,
) -> PreparedDeploymentEnvSecret {
    let secret_hash = hash_deployment_env_secret(&data);
    let secret = resource_builder.create_deployment_env_secret(
        project,
        deployment,
        namespace,
        environment_name,
        &secret_hash,
        data,
    );
    let secret_name = secret
        .metadata
        .name
        .clone()
        .unwrap_or_else(|| ResourceBuilder::deployment_env_secret_name(project, deployment));
    let is_ready =
        observed_secret_matches_hash(observed_secrets, namespace, &secret_name, &secret_hash);

    PreparedDeploymentEnvSecret {
        secret_name,
        secret_hash,
        is_ready,
        secret,
    }
}

/// Add backend service + endpoints resources.
///
/// The Service is returned as a Metacontroller child. For IP-based backends,
/// the Endpoints are applied directly via kube-rs because Endpoints cannot be
/// a Metacontroller child resource type — Kubernetes auto-creates Endpoints
/// for Services with selectors (e.g. the deployment `default` Service), and
/// Metacontroller would thrash deleting/adopting those in an infinite loop.
/// Since child resource types are all-or-nothing, we manage the `rise-backend`
/// Endpoints outside of Metacontroller as well.
async fn add_backend_resources(
    state: &AppState,
    children: &mut Vec<serde_json::Value>,
    resource_builder: &ResourceBuilder,
    project: &Project,
    namespace: &str,
    backend_address: &crate::server::settings::BackendAddress,
) -> anyhow::Result<()> {
    if backend_address.is_ip_address() {
        // IP address → ClusterIP Service (as child) + Endpoints (applied directly)
        let svc = resource_builder.create_backend_service_clusterip(
            project,
            namespace,
            backend_address.port,
        );
        children.push(serde_json::to_value(&svc)?);

        let endpoints = resource_builder.create_backend_endpoints(
            project,
            namespace,
            &backend_address.host,
            backend_address.port,
        );
        apply_backend_endpoints(state, &endpoints, namespace).await;
    } else {
        // DNS name → ExternalName
        let svc = resource_builder.create_backend_service_externalname(
            project,
            namespace,
            &backend_address.host,
        );
        children.push(serde_json::to_value(&svc)?);
    }

    Ok(())
}

/// Apply backend Endpoints directly via kube-rs server-side apply.
///
/// On the first sync for a new project the namespace returned in this sync's
/// children list has not yet been applied by Metacontroller, so the apply
/// 404s. That's expected and self-heals on the next resync, so the 404 is
/// logged at debug instead of warning.
async fn apply_backend_endpoints(
    state: &AppState,
    endpoints: &k8s_openapi::api::core::v1::Endpoints,
    namespace: &str,
) {
    let Some(ref kube_client) = state.kube_client else {
        return;
    };
    let api: kube::Api<k8s_openapi::api::core::v1::Endpoints> =
        kube::Api::namespaced(kube_client.clone(), namespace);
    let name = endpoints.metadata.name.as_deref().unwrap_or("rise-backend");
    let params = kube::api::PatchParams::apply("rise-controller").force();
    match api
        .patch(name, &params, &kube::api::Patch::Apply(endpoints))
        .await
    {
        Ok(_) => {}
        Err(kube::Error::Api(err)) if err.code == 404 => {
            debug!(
                namespace = %namespace,
                "Backend Endpoints apply deferred: namespace not yet created (will retry on next resync)"
            );
        }
        Err(e) => {
            warn!(
                namespace = %namespace,
                "Failed to apply backend Endpoints: {:?}", e
            );
        }
    }
}

/// Load and decrypt environment variables for a deployment
async fn load_env_vars(
    state: &AppState,
    _project: &Project,
    deployment: &Deployment,
) -> anyhow::Result<ResolvedDeploymentEnvVars> {
    let env_vars = db_env_vars::list_deployment_env_vars(&state.db_pool, deployment.id).await?;
    resolve_deployment_env_vars(env_vars, state.encryption_provider.as_deref()).await
}

pub(crate) async fn resolve_deployment_env_vars(
    env_vars: Vec<DeploymentEnvVar>,
    encryption_provider: Option<&dyn crate::server::encryption::EncryptionProvider>,
) -> anyhow::Result<ResolvedDeploymentEnvVars> {
    let mut resolved = ResolvedDeploymentEnvVars::default();

    for var in env_vars {
        let key = var.key.trim().to_string();
        if key != var.key {
            tracing::warn!(
                "Environment variable key {:?} has surrounding whitespace; trimming to {:?}",
                var.key,
                key
            );
        }

        let value = if var.is_secret {
            match encryption_provider {
                Some(provider) => provider
                    .decrypt(&var.value)
                    .await
                    .with_context(|| format!("Failed to decrypt secret variable '{}'", key))?,
                None => {
                    tracing::error!(
                        "Encountered secret variable '{}' but no encryption provider configured",
                        key
                    );
                    return Err(anyhow::anyhow!(
                        "Cannot decrypt secret variable '{}': no encryption provider",
                        key
                    ));
                }
            }
        } else {
            var.value
        };

        if var.is_secret {
            resolved
                .secret_env_vars
                .insert(key, ByteString(value.into_bytes()));
        } else {
            resolved.plain_env_vars.push(EnvVar {
                name: key,
                value: Some(value),
                ..Default::default()
            });
        }
    }

    Ok(resolved)
}

// ── Finalize webhook handler ───────────────────────────────────────────

pub async fn handle_finalize(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    Json(request): Json<FinalizeRequest>,
) -> Response {
    if let Err((status, msg)) = validate_source_ip(&state, addr).await {
        return (status, msg).into_response();
    }

    let project_name = match request
        .parent
        .get("metadata")
        .and_then(|m| m.get("name"))
        .and_then(|n| n.as_str())
    {
        Some(name) => name.to_string(),
        None => {
            error!("Finalize request missing parent.metadata.name");
            return (
                StatusCode::BAD_REQUEST,
                Json(FinalizeResponse {
                    status: serde_json::json!({"error": "missing parent name"}),
                    children: vec![],
                    finalized: true,
                }),
            )
                .into_response();
        }
    };

    match process_finalize(&state, &project_name).await {
        Ok(response) => (StatusCode::OK, Json(response)).into_response(),
        Err(e) => {
            error!(project = %project_name, "Finalize webhook error: {:?}", e);
            // Return finalized: false so Metacontroller retries on transient errors
            // (e.g., DB connectivity). This avoids leaving deployment records in
            // non-terminal states while Metacontroller deletes all resources.
            (
                StatusCode::OK,
                Json(FinalizeResponse {
                    status: serde_json::json!({
                        "error": format!("{:#}", e),
                    }),
                    children: vec![],
                    finalized: false,
                }),
            )
                .into_response()
        }
    }
}

async fn process_finalize(
    state: &AppState,
    project_name: &str,
) -> anyhow::Result<FinalizeResponse> {
    info!(project = %project_name, "Processing finalize webhook — marking all deployments as stopped");

    if let Some(project) = db_projects::find_by_name(&state.db_pool, project_name).await? {
        // Mark all non-terminal deployments as Stopped
        let deployments =
            db_deployments::list_non_terminal_for_project(&state.db_pool, project.id).await?;
        for deployment in deployments {
            {
                info!(
                    deployment_id = %deployment.deployment_id,
                    "Finalize: marking deployment as Stopped"
                );
                // Try the most appropriate terminal transition
                if state_machine::is_valid_transition(
                    &deployment.status,
                    &DeploymentStatus::Cancelling,
                ) {
                    db_deployments::mark_cancelling(&state.db_pool, deployment.id).await?;
                    db_deployments::mark_cancelled(&state.db_pool, deployment.id).await?;
                } else {
                    db_deployments::mark_terminating(
                        &state.db_pool,
                        deployment.id,
                        TerminationReason::UserStopped,
                    )
                    .await?;
                    db_deployments::mark_stopped(&state.db_pool, deployment.id).await?;
                }
            }
        }
        db_projects::update_calculated_status(&state.db_pool, project.id).await?;
    }

    Ok(FinalizeResponse {
        status: serde_json::json!({}),
        children: vec![],
        finalized: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{DeploymentStatus, Project, ProjectStatus};
    use crate::server::deployment::resource_builder::ResourceBuilder;
    use crate::server::encryption::EncryptionProvider;
    use crate::server::registry::models::RegistryCredentials;
    use crate::server::registry::{ImageTagType, RegistryProvider};
    use anyhow::Result;
    use async_trait::async_trait;
    use std::sync::Arc;

    struct TestRegistryProvider;

    #[async_trait]
    impl RegistryProvider for TestRegistryProvider {
        async fn get_credentials(
            &self,
            _repository: &str,
            _tags: &[&str],
        ) -> Result<RegistryCredentials> {
            unreachable!("not used in these tests")
        }

        async fn get_pull_credentials(&self) -> Result<(String, String)> {
            unreachable!("not used in these tests")
        }

        fn registry_host(&self) -> &str {
            "registry.example.test"
        }

        fn registry_url(&self) -> &str {
            "registry.example.test/rise"
        }

        fn get_image_tag(&self, repository: &str, tag: &str, _tag_type: ImageTagType) -> String {
            format!("registry.example.test/rise/{repository}:{tag}")
        }
    }

    // ── should_have_infrastructure ─────────────────────────────────────

    #[test]
    fn test_should_have_infrastructure_for_active_states() {
        let statuses_with_infra = [
            DeploymentStatus::Pushed,
            DeploymentStatus::Deploying,
            DeploymentStatus::Healthy,
            DeploymentStatus::Unhealthy,
            DeploymentStatus::Terminating,
        ];
        for status in &statuses_with_infra {
            let d = test_deployment(status.clone());
            assert!(
                should_have_infrastructure(&d),
                "{:?} should have infrastructure",
                status
            );
        }
    }

    #[test]
    fn test_should_not_have_infrastructure_for_terminal_and_pre_push_states() {
        let statuses_without_infra = [
            DeploymentStatus::Pending,
            DeploymentStatus::Building,
            DeploymentStatus::Pushing,
            DeploymentStatus::Cancelling,
            DeploymentStatus::Cancelled,
            DeploymentStatus::Stopped,
            DeploymentStatus::Superseded,
            DeploymentStatus::Failed,
            DeploymentStatus::Expired,
        ];
        for status in &statuses_without_infra {
            let d = test_deployment(status.clone());
            assert!(
                !should_have_infrastructure(&d),
                "{:?} should NOT have infrastructure",
                status
            );
        }
    }

    // ── FinalizeResponse serialization ─────────────────────────────────

    #[test]
    fn test_finalize_response_serialization() {
        let response = FinalizeResponse {
            status: serde_json::json!({}),
            children: vec![],
            finalized: true,
        };
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["finalized"], true);
        assert_eq!(json["children"], serde_json::json!([]));
    }

    #[test]
    fn test_finalize_response_not_finalized() {
        let response = FinalizeResponse {
            status: serde_json::json!({"error": "db down"}),
            children: vec![],
            finalized: false,
        };
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["finalized"], false);
    }

    // ── SyncResponse serialization ─────────────────────────────────────

    #[test]
    fn test_sync_response_omits_resync_when_none() {
        let response = SyncResponse {
            status: serde_json::json!({}),
            children: vec![],
            resync_after_seconds: None,
        };
        let json = serde_json::to_value(&response).unwrap();
        assert!(!json.as_object().unwrap().contains_key("resyncAfterSeconds"));
    }

    #[test]
    fn test_sync_response_includes_resync_when_set() {
        let response = SyncResponse {
            status: serde_json::json!({}),
            children: vec![],
            resync_after_seconds: Some(30.0),
        };
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["resyncAfterSeconds"], 30.0);
    }

    #[tokio::test]
    async fn resolve_deployment_env_vars_splits_plain_and_secret_values() {
        let env_vars = vec![
            test_env_var("API_KEY", "ciphertext-a", true, true),
            test_env_var("PORT", "8080", false, false),
            test_env_var("SESSION_SECRET", "ciphertext-b", true, false),
        ];
        // 32 zero bytes encoded as standard base64
        let provider = crate::server::encryption::providers::local::LocalEncryptionProvider::new(
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
        )
        .unwrap();

        let mut encrypted_env_vars = Vec::new();
        for mut var in env_vars {
            if var.is_secret {
                var.value = provider.encrypt(&var.value).await.unwrap();
            }
            encrypted_env_vars.push(var);
        }

        let resolved = resolve_deployment_env_vars(encrypted_env_vars, Some(&provider))
            .await
            .unwrap();

        assert_eq!(resolved.plain_env_vars.len(), 1);
        assert_eq!(resolved.plain_env_vars[0].name, "PORT");
        assert_eq!(resolved.plain_env_vars[0].value.as_deref(), Some("8080"));
        assert_eq!(resolved.secret_env_vars.len(), 2);
        assert_eq!(
            resolved.secret_env_vars["API_KEY"].0,
            b"ciphertext-a".to_vec()
        );
        assert_eq!(
            resolved.secret_env_vars["SESSION_SECRET"].0,
            b"ciphertext-b".to_vec()
        );
    }

    #[test]
    fn deployment_env_secret_hash_is_stable_for_identical_data() {
        let mut data_a = BTreeMap::new();
        data_a.insert("API_KEY".to_string(), ByteString(b"secret-a".to_vec()));
        data_a.insert(
            "SESSION_SECRET".to_string(),
            ByteString(b"secret-b".to_vec()),
        );

        let mut data_b = BTreeMap::new();
        data_b.insert(
            "SESSION_SECRET".to_string(),
            ByteString(b"secret-b".to_vec()),
        );
        data_b.insert("API_KEY".to_string(), ByteString(b"secret-a".to_vec()));

        assert_eq!(
            hash_deployment_env_secret(&data_a),
            hash_deployment_env_secret(&data_b)
        );
    }

    #[test]
    fn prepare_deployment_env_secret_waits_for_matching_observed_hash() {
        let builder = test_resource_builder();
        let project = test_project();
        let deployment = test_deployment(DeploymentStatus::Deploying);
        let mut data = BTreeMap::new();
        data.insert("API_KEY".to_string(), ByteString(b"secret-a".to_vec()));

        let prepared = prepare_deployment_env_secret(
            &builder,
            &project,
            &deployment,
            "demo",
            None,
            &HashMap::new(),
            data.clone(),
        );

        assert!(!prepared.is_ready);

        let mut observed_secrets = HashMap::new();
        observed_secrets.insert(
            "demo/demo-20260429-000000-env".to_string(),
            serde_json::json!({
                "metadata": {
                    "annotations": {
                        ANNOTATION_ENV_SECRET_HASH: prepared.secret_hash
                    }
                }
            }),
        );

        let prepared_ready = prepare_deployment_env_secret(
            &builder,
            &project,
            &deployment,
            "demo",
            None,
            &observed_secrets,
            data,
        );

        assert!(prepared_ready.is_ready);
    }

    // ── Helper ─────────────────────────────────────────────────────────

    fn test_resource_builder() -> ResourceBuilder {
        ResourceBuilder {
            production_ingress_url_template: "{project_name}.example.test".to_string(),
            staging_ingress_url_template: None,
            environment_ingress_url_template: None,
            ingress_port: None,
            ingress_schema: "https".to_string(),
            registry_provider: Arc::new(TestRegistryProvider),
            auth_backend_url: "https://auth.example.test".to_string(),
            auth_signin_url: "https://signin.example.test".to_string(),
            backend_address: None,
            namespace_labels: HashMap::new(),
            namespace_annotations: HashMap::new(),
            ingress_annotations: HashMap::new(),
            ingress_tls_secret_name: None,
            custom_domain_tls_mode: crate::server::settings::CustomDomainTlsMode::PerDomain,
            custom_domain_ingress_annotations: HashMap::new(),
            node_selector: HashMap::new(),
            image_pull_secret_name: None,
            access_classes: HashMap::new(),
            host_aliases: HashMap::new(),
            extra_service_token_audiences: HashMap::new(),
            use_default_service_account_for_production: true,
            network_policy: crate::server::settings::NetworkPolicyConfig {
                ingress: vec![],
                egress: None,
            },
            pod_security_enabled: true,
            health_probes: None,
        }
    }

    fn test_project() -> Project {
        Project {
            id: uuid::Uuid::nil(),
            name: "demo".to_string(),
            status: ProjectStatus::Running,
            access_class: "default".to_string(),
            owner_user_id: None,
            owner_team_id: None,
            finalizers: vec![],
            source_url: None,
            template_id: None,
            template_image: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn test_deployment(status: DeploymentStatus) -> Deployment {
        Deployment {
            id: uuid::Uuid::nil(),
            deployment_id: "20260429-000000".to_string(),
            project_id: uuid::Uuid::nil(),
            created_by_id: uuid::Uuid::nil(),
            status,
            deployment_group: "default".to_string(),
            environment_id: None,
            expires_at: None,
            termination_reason: None,
            completed_at: None,
            error_message: None,
            build_logs: None,
            controller_metadata: serde_json::Value::Null,
            image: None,
            image_digest: None,
            rolled_back_from_deployment_id: None,
            http_port: 8080,
            needs_reconcile: false,
            is_active: false,
            deploying_started_at: None,
            first_healthy_at: None,
            job_url: None,
            pull_request_url: None,
            git_repository_url: None,
            replicas: 1,
            cpu: "500m".to_string(),
            memory: "256Mi".to_string(),
            identity_credential_hash: None,
            identity_audiences: serde_json::json!({}),
            containers: None,
            routes: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn test_env_var(
        key: &str,
        value: &str,
        is_secret: bool,
        is_protected: bool,
    ) -> DeploymentEnvVar {
        DeploymentEnvVar {
            id: uuid::Uuid::new_v4(),
            deployment_id: uuid::Uuid::nil(),
            key: key.to_string(),
            value: value.to_string(),
            is_secret,
            is_protected,
            source: "global".to_string(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn resolve_deployment_env_vars_trims_whitespace_from_keys() {
        let env_vars = vec![
            test_env_var(" LEADING_SPACE", "value1", false, false),
            test_env_var("TRAILING_SPACE ", "value2", false, false),
            test_env_var("  BOTH  ", "value3", false, false),
            test_env_var("CLEAN_KEY", "value4", false, false),
        ];

        let resolved = resolve_deployment_env_vars(env_vars, None).await.unwrap();

        let names: Vec<&str> = resolved
            .plain_env_vars
            .iter()
            .map(|v| v.name.as_str())
            .collect();
        assert!(
            names.contains(&"LEADING_SPACE"),
            "leading space should be trimmed"
        );
        assert!(
            names.contains(&"TRAILING_SPACE"),
            "trailing space should be trimmed"
        );
        assert!(
            names.contains(&"BOTH"),
            "both-side spaces should be trimmed"
        );
        assert!(names.contains(&"CLEAN_KEY"));
        assert!(
            !names.iter().any(|n| n.starts_with(' ') || n.ends_with(' ')),
            "no key should retain surrounding whitespace"
        );
    }

    #[tokio::test]
    async fn resolve_deployment_env_vars_trims_whitespace_from_secret_keys() {
        let provider = crate::server::encryption::providers::local::LocalEncryptionProvider::new(
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
        )
        .unwrap();
        let raw_value = "secret-value";
        let encrypted = provider.encrypt(raw_value).await.unwrap();

        let env_vars = vec![test_env_var(" SECRET_KEY", &encrypted, true, false)];

        let resolved = resolve_deployment_env_vars(env_vars, Some(&provider))
            .await
            .unwrap();

        assert!(
            resolved.secret_env_vars.contains_key("SECRET_KEY"),
            "trimmed key 'SECRET_KEY' should be present"
        );
        assert!(
            !resolved.secret_env_vars.contains_key(" SECRET_KEY"),
            "untrimmed key should not be present"
        );
    }

    // -----------------------------------------------------------------------------
    // check_controller_class
    // -----------------------------------------------------------------------------

    #[test]
    fn check_controller_class_unconfigured_always_passes() {
        // Legacy installs (no controller class configured) reconcile every
        // project regardless of what the Organization carries.
        assert!(check_controller_class(None, None).is_ok());
        assert!(check_controller_class(None, Some("kubernetes.rise.dev/default")).is_ok());
        assert!(check_controller_class(None, Some("something-else")).is_ok());
    }

    #[test]
    fn check_controller_class_matching_passes() {
        assert!(check_controller_class(
            Some("kubernetes.rise.dev/default"),
            Some("kubernetes.rise.dev/default"),
        )
        .is_ok());
    }

    #[test]
    fn check_controller_class_mismatched_rejects() {
        assert!(check_controller_class(
            Some("kubernetes.rise.dev/default"),
            Some("kubernetes.rise.dev/other"),
        )
        .is_err());
    }

    #[test]
    fn check_controller_class_unset_on_org_rejects() {
        // An Org that has no `spec.deploymentControllerClass` is not owned by
        // this (or any) controller, even when the controller has a class
        // configured. Refuse to reconcile.
        assert!(check_controller_class(Some("kubernetes.rise.dev/default"), None).is_err());
    }

    fn cspec(name: &str, port: Option<u16>) -> crate::server::deployment::models::ContainerSpec {
        crate::server::deployment::models::ContainerSpec {
            name: name.to_string(),
            image: Some("img".to_string()),
            port,
            replicas: Some(1),
            cpu: None,
            memory: None,
            env_overrides: vec![],
            health_check: None,
        }
    }

    fn deploy_at(deployment_id: &str, is_active: bool) -> Deployment {
        let mut d = test_deployment(DeploymentStatus::Healthy);
        d.id = uuid::Uuid::new_v4();
        d.deployment_id = deployment_id.to_string();
        d.is_active = is_active;
        d
    }

    #[test]
    fn ownership_active_only_owns_all_its_routable_containers() {
        // The simple steady-state case: one active deployment, no rolling-out
        // sibling. Active owns every routable container it has; workers (no
        // http_port) are absent from the map.
        let active = deploy_at("20260520-000000", true);
        let mut specs = HashMap::new();
        specs.insert(
            active.id,
            vec![
                cspec("api", Some(8080)),
                cspec("worker", None),
                cspec("redis", Some(6379)),
            ],
        );

        let owners = compute_service_owner_per_container(&[&active], &specs);

        assert_eq!(owners.len(), 2, "worker is absent");
        assert_eq!(
            owners.get(&("default".to_string(), "api".to_string())),
            Some(&active.id),
        );
        assert_eq!(
            owners.get(&("default".to_string(), "redis".to_string())),
            Some(&active.id),
        );
        assert!(
            !owners.contains_key(&("default".to_string(), "worker".to_string())),
            "worker has no http_port, so no Service is owned",
        );
    }

    #[test]
    fn ownership_active_wins_for_shared_containers_during_rollout() {
        // Active old [api, frontend, worker] + rolling-out new
        // [api, frontend, worker, redis]. Active still owns api/frontend
        // (blue-green preserved) while the rolling-out new deployment becomes
        // the owner of the brand-new redis Service so its sibling pods can
        // reach it during probes.
        let old = deploy_at("20260520-000000", true);
        let new = deploy_at("20260528-000000", false);

        let mut specs = HashMap::new();
        specs.insert(
            old.id,
            vec![
                cspec("api", Some(8080)),
                cspec("frontend", Some(8080)),
                cspec("worker", None),
            ],
        );
        specs.insert(
            new.id,
            vec![
                cspec("api", Some(8080)),
                cspec("frontend", Some(8080)),
                cspec("worker", None),
                cspec("redis", Some(6379)),
            ],
        );

        let owners = compute_service_owner_per_container(&[&old, &new], &specs);

        assert_eq!(
            owners.get(&("default".to_string(), "api".to_string())),
            Some(&old.id),
            "shared container stays owned by the active deployment",
        );
        assert_eq!(
            owners.get(&("default".to_string(), "frontend".to_string())),
            Some(&old.id),
        );
        assert_eq!(
            owners.get(&("default".to_string(), "redis".to_string())),
            Some(&new.id),
            "new-only container is owned by the rolling-out deployment",
        );
    }

    #[test]
    fn ownership_no_active_uses_most_recent_deployment() {
        // Fresh project (or a group with no active deployment): the
        // most-recent deployment that has each container takes ownership so
        // intra-pod communication works during the first deploy too.
        let older = deploy_at("20260101-000000", false);
        let newer = deploy_at("20260528-000000", false);
        let mut specs = HashMap::new();
        specs.insert(
            older.id,
            vec![cspec("api", Some(8080)), cspec("frontend", Some(8080))],
        );
        specs.insert(
            newer.id,
            vec![
                cspec("api", Some(8080)),
                cspec("frontend", Some(8080)),
                cspec("redis", Some(6379)),
            ],
        );

        let owners = compute_service_owner_per_container(&[&older, &newer], &specs);

        assert_eq!(
            owners.get(&("default".to_string(), "api".to_string())),
            Some(&newer.id),
            "no active → most recent wins for shared containers",
        );
        assert_eq!(
            owners.get(&("default".to_string(), "redis".to_string())),
            Some(&newer.id),
        );
    }

    #[test]
    fn ownership_isolated_per_deployment_group() {
        // Containers with the same name in different groups (e.g. "default"
        // and "staging") have independent ownership maps — they emit distinct
        // Services with distinct names.
        let mut prod = deploy_at("20260520-000000", true);
        prod.deployment_group = "default".to_string();
        let mut stg = deploy_at("20260520-000000", true);
        stg.deployment_group = "staging".to_string();

        let mut specs = HashMap::new();
        specs.insert(prod.id, vec![cspec("api", Some(8080))]);
        specs.insert(stg.id, vec![cspec("api", Some(8080))]);

        let owners = compute_service_owner_per_container(&[&prod, &stg], &specs);

        assert_eq!(
            owners.get(&("default".to_string(), "api".to_string())),
            Some(&prod.id),
        );
        assert_eq!(
            owners.get(&("staging".to_string(), "api".to_string())),
            Some(&stg.id),
        );
    }

    #[test]
    fn resolve_runtime_containers_synthesises_app_for_single_container() {
        // A row with no `containers` side-data is a single-container deployment:
        // synthesise one `app` container (no image — the reconciler fills it from
        // the resolved deployment image) plus a `/` route to it.
        let mut d = test_deployment(DeploymentStatus::Healthy);
        d.http_port = 8080;
        d.replicas = 2;
        d.cpu = "500m".to_string();
        d.memory = "256Mi".to_string();
        d.containers = None;
        d.routes = None;

        let (specs, routes) = resolve_runtime_containers(&d).unwrap();

        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "app");
        assert_eq!(specs[0].port, Some(8080));
        assert_eq!(specs[0].replicas, Some(2));
        assert_eq!(specs[0].cpu.as_deref(), Some("500m"));
        assert_eq!(specs[0].image, None, "image is filled by the reconciler");
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].path, "/");
        assert_eq!(routes[0].container, "app");
    }

    #[test]
    fn apply_container_port_env_overrides_global_port() {
        // The global env vars carry the deployment-wide PORT (here the 8080
        // default). A container declaring its own port (3000) must have PORT
        // rewritten to 3000 so the app binds the same port the Service/probes
        // target — not the inherited 8080.
        let mut env = vec![
            EnvVar {
                name: "PORT".to_string(),
                value: Some("8080".to_string()),
                ..Default::default()
            },
            EnvVar {
                name: "FOO".to_string(),
                value: Some("bar".to_string()),
                ..Default::default()
            },
        ];

        apply_container_port_env(&mut env, Some(3000));

        let port = env.iter().find(|v| v.name == "PORT").unwrap();
        assert_eq!(port.value.as_deref(), Some("3000"));
        // Unrelated vars are untouched and PORT isn't duplicated.
        assert_eq!(env.iter().filter(|v| v.name == "PORT").count(), 1);
        assert_eq!(
            env.iter()
                .find(|v| v.name == "FOO")
                .unwrap()
                .value
                .as_deref(),
            Some("bar")
        );
    }

    #[test]
    fn apply_container_port_env_inserts_when_absent() {
        // If no global PORT was set, a container with a declared port still gets
        // one.
        let mut env = Vec::new();
        apply_container_port_env(&mut env, Some(9000));
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].name, "PORT");
        assert_eq!(env[0].value.as_deref(), Some("9000"));
    }

    #[test]
    fn apply_container_port_env_leaves_workers_untouched() {
        // A worker (no declared port) keeps whatever global PORT was set, if any.
        let mut env = vec![EnvVar {
            name: "PORT".to_string(),
            value: Some("8080".to_string()),
            ..Default::default()
        }];
        apply_container_port_env(&mut env, None);
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].value.as_deref(), Some("8080"));

        // ...and a portless worker with no global PORT gets none injected.
        let mut empty = Vec::new();
        apply_container_port_env(&mut empty, None);
        assert!(empty.is_empty());
    }

    #[test]
    fn resolve_runtime_containers_parses_persisted_multi_container() {
        // A row WITH persisted container side-data (a versioned envelope) parses
        // it (and does not synthesise the implicit `app`).
        let mut d = test_deployment(DeploymentStatus::Healthy);
        d.containers = Some(serde_json::json!({
            "version": 1,
            "items": [
                { "name": "api", "image": "img-api", "port": 8080 },
                { "name": "worker", "image": "img-worker" },
            ]
        }));
        d.routes = Some(serde_json::json!({
            "version": 1,
            "items": [ { "path": "/", "container": "api" } ]
        }));

        let (specs, routes) = resolve_runtime_containers(&d).unwrap();

        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].name, "api");
        assert_eq!(specs[1].name, "worker");
        assert_eq!(specs[1].port, None, "worker has no port");
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].container, "api");
    }

    #[test]
    fn resolve_runtime_containers_errors_on_unparseable_containers() {
        // A non-NULL but corrupt `containers` column is a hard error for THIS
        // deployment — it must NOT silently collapse to the single-container
        // fallback (which would drop every per-container K8s resource). Here the
        // envelope is well-formed but `items` is not a list of container specs.
        let mut d = test_deployment(DeploymentStatus::Healthy);
        d.containers = Some(serde_json::json!({
            "version": 1,
            "items": { "not": "a list of container specs" }
        }));

        let err = resolve_runtime_containers(&d).unwrap_err();
        assert!(
            format!("{:?}", err).contains("could not be"),
            "error should mention the decode failure, got: {:?}",
            err
        );
    }

    #[test]
    fn resolve_runtime_containers_errors_on_unparseable_routes() {
        // Same for routes: present-but-corrupt is an error, not an empty route
        // set.
        let mut d = test_deployment(DeploymentStatus::Healthy);
        d.containers = Some(serde_json::json!({
            "version": 1,
            "items": [ { "name": "api", "image": "img-api", "port": 8080 } ]
        }));
        d.routes = Some(serde_json::json!({
            "version": 1,
            "items": { "not": "a list of routes" }
        }));

        assert!(resolve_runtime_containers(&d).is_err());
    }

    #[test]
    fn env_secret_guard_matches_existing_observed_container_deployment() {
        // Regression: the env-secret GC guard used to look up the bare
        // `<project>-<deployment_id>` key, but observed Deployments are keyed
        // `<namespace>/<project>-<deployment_id>-<container>`. The bare key
        // never matched, so a stale env-secret hash withheld (and Metacontroller
        // GC'd) every container Deployment. The guard must recognise an existing
        // observed container Deployment so it re-emits it while the new secret
        // is created.
        let project = test_project();
        let deployment = test_deployment(DeploymentStatus::Healthy);
        let namespace = ResourceBuilder::namespace_name(&project, "");
        let base_name = ResourceBuilder::deployment_name(&project, &deployment);
        let container_specs = vec![cspec("api", Some(8080)), cspec("worker", None)];

        // No observed Deployments yet → guard reports "not observed".
        let empty = ObservedChildren::default();
        assert!(
            !any_container_deployment_observed(&empty, &namespace, &base_name, &container_specs),
            "no observed Deployments should report as not-yet-observed"
        );

        // The bare key the buggy guard used must NOT be present in the observed
        // map — this is exactly why the old guard always returned false.
        let mut observed = ObservedChildren::default();
        let bare_key = base_name.clone();
        assert!(
            !observed.deployments.contains_key(&bare_key),
            "the bare <project>-<deployment_id> key is never how observed Deployments are keyed"
        );

        // Insert one container's observed Deployment under the real
        // `<namespace>/<project>-<deployment_id>-<container>` key.
        let real_key = format!("{}/{}-{}", namespace, base_name, "api");
        observed
            .deployments
            .insert(real_key, serde_json::json!({ "kind": "Deployment" }));

        assert!(
            any_container_deployment_observed(&observed, &namespace, &base_name, &container_specs),
            "an existing observed container Deployment must be recognised so it is re-emitted"
        );
    }
}
