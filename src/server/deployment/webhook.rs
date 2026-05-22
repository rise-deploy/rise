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
use crate::server::workload_tokens::{sha256_hex, workload_subject};

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
const DEPLOYING_TIMEOUT_MINUTES: i64 = 5;
/// Duration a deployment can be in pre-Pushed states before timing out
const PRE_PUSHED_TIMEOUT_MINUTES: i64 = 10;
/// Duration after which image pull secret is refreshed (6 hours)
const SECRET_REFRESH_HOURS: i64 = 6;
/// Maximum number of terminating/terminated pods to carry forward in controller_metadata
const MAX_INACTIVE_PODS: usize = 5;
/// Lifetime of auto-minted workload identity tokens.
const IDENTITY_TOKEN_TTL_SECS: u64 = 3600;
/// Age past which auto-minted workload tokens are re-minted (half of the TTL).
const IDENTITY_TOKEN_REFRESH_SECS: i64 = 1800;

#[derive(Debug, Default)]
struct ResolvedDeploymentEnvVars {
    plain_env_vars: Vec<EnvVar>,
    secret_env_vars: BTreeMap<String, ByteString>,
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
        Err(e) => {
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
) -> anyhow::Result<SyncResponse> {
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

    // 2. Load non-terminal deployments for this project (avoids loading full history)
    let non_terminal_deployments =
        db_deployments::list_non_terminal_for_project(&state.db_pool, project.id).await?;

    let non_terminal: Vec<&Deployment> = non_terminal_deployments.iter().collect();

    // 3. Perform status transitions based on observed K8s state
    perform_status_transitions(state, &project, &non_terminal, observed).await?;

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
            anyhow::bail!("No resource builder configured — cannot compute desired children");
        }
    };

    // 6. Compute desired children
    let children = compute_desired_children(
        state,
        resource_builder,
        &project,
        &all_deployments,
        observed,
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

/// Inspect the observed Kubernetes state for each non-terminal deployment and
/// advance its status: Pushed → Deploying, Deploying → Healthy/Failed, timeouts,
/// expiration, and cancellation.
async fn perform_status_transitions(
    state: &AppState,
    project: &Project,
    non_terminal: &[&Deployment],
    observed: &ObservedChildren,
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
                check_deployment_health_from_observed(state, deployment, project, observed).await?;
            }

            DeploymentStatus::Healthy | DeploymentStatus::Unhealthy => {
                check_deployment_health_from_observed(state, deployment, project, observed).await?;
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
async fn check_deployment_health_from_observed(
    state: &AppState,
    deployment: &Deployment,
    project: &Project,
    observed: &ObservedChildren,
) -> anyhow::Result<()> {
    // Metacontroller keys namespaced children of cluster-scoped parents as "namespace/name"
    let resource_builder = match &state.resource_builder {
        Some(rb) => rb,
        None => return Ok(()),
    };
    let namespace = resource_builder.namespace_name(project);
    let k8s_deploy_name = format!(
        "{}/{}-{}",
        namespace, project.name, deployment.deployment_id
    );

    let observed_deploy = match observed.deployments.get(&k8s_deploy_name) {
        Some(d) => d,
        None => {
            // K8s Deployment doesn't exist yet — nothing to check
            debug!(
                deployment_id = %deployment.deployment_id,
                "No observed K8s Deployment yet for {}", k8s_deploy_name
            );
            return Ok(());
        }
    };

    // Parse the observed deployment to check readiness
    let observed_k8s: K8sDeployment = serde_json::from_value(observed_deploy.clone())?;

    let desired_replicas = observed_k8s
        .spec
        .as_ref()
        .and_then(|s| s.replicas)
        .unwrap_or(1);
    let ready_replicas = observed_k8s
        .status
        .as_ref()
        .and_then(|s| s.ready_replicas)
        .unwrap_or(0);

    // Check for pod-level errors and collect full pod status via kube-rs
    // (Metacontroller only gives us the Deployment, not individual pods)
    let pod_check = check_pod_errors_via_kube(
        state,
        project,
        deployment,
        desired_replicas,
        ready_replicas,
        &deployment.controller_metadata,
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

    let namespace = match &state.resource_builder {
        Some(rb) => rb.namespace_name(project),
        None => {
            return PodCheckResult {
                has_error: false,
                error_message: None,
                pod_status: None,
            }
        }
    };
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
                            "reason": running.started_at.as_ref().map(|t| t.0.to_string()),
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
                        }))
                    } else {
                        None
                    }
                } else {
                    None
                };

                container_infos.push(serde_json::json!({
                    "name": cs.name,
                    "ready": cs.ready,
                    "restart_count": cs.restart_count,
                    "state": state_info,
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

    // Clear needs_reconcile if set
    if deployment.needs_reconcile {
        db_deployments::clear_needs_reconcile(&state.db_pool, deployment.id).await?;
    }

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
) -> anyhow::Result<Vec<serde_json::Value>> {
    let mut children: Vec<serde_json::Value> = Vec::new();
    let namespace = resource_builder.namespace_name(project);

    // Preload all environments for this project to avoid per-deployment DB lookups
    let environments: HashMap<uuid::Uuid, crate::db::models::Environment> =
        db_environments::list_for_project(&state.db_pool, project.id)
            .await?
            .into_iter()
            .map(|env| (env.id, env))
            .collect();

    // 1. Namespace (always)
    let ns = resource_builder.create_namespace(project);
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
    let infra_deployments: Vec<&Deployment> = all_deployments
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
                let k8s_deploy_name = ResourceBuilder::deployment_name(project, deployment);
                let deploy_already_observed = observed.deployments.contains_key(&k8s_deploy_name);

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

        let k8s_deploy = resource_builder.create_k8s_deployment(
            project,
            deployment,
            &namespace,
            &image,
            deployment.http_port as u16,
            env_vars.plain_env_vars,
            secret_env_name,
            secret_env_hash,
            sa_name,
            env_name.as_deref(),
            Some(identity.mount),
        );
        children.push(serde_json::to_value(&k8s_deploy)?);

        if deployment.is_active {
            active_by_group.insert(deployment.deployment_group.clone(), deployment);
        }
    }

    // Services, Ingresses, NetworkPolicies — one per group with an active deployment
    let custom_domains =
        crate::db::custom_domains::list_project_custom_domains(&state.db_pool, project.id).await?;
    let valid_custom_domains = resource_builder.filter_valid_custom_domains(&custom_domains);

    for (group, active_deployment) in &active_by_group {
        let env_name = env_name_for(active_deployment);

        // Service (selector points to the active deployment)
        let service = resource_builder.create_service(
            project,
            active_deployment,
            &namespace,
            active_deployment.http_port as u16,
            env_name.as_deref(),
        );
        children.push(serde_json::to_value(&service)?);

        // Primary Ingress
        let ingress = resource_builder.create_primary_ingress(
            project,
            active_deployment,
            &namespace,
            env_name.as_deref(),
        )?;
        children.push(serde_json::to_value(&ingress)?);

        // Custom domain Ingress (only for production primary group)
        let environment = active_deployment
            .environment_id
            .and_then(|id| environments.get(&id));

        let is_production_primary = environment
            .map(|env| {
                env.is_production && env.primary_deployment_group.as_deref() == Some(group.as_str())
            })
            .unwrap_or(*group == crate::server::deployment::models::DEFAULT_DEPLOYMENT_GROUP);

        if is_production_primary && !valid_custom_domains.is_empty() {
            let custom_ingress = resource_builder.create_custom_domain_ingress(
                project,
                active_deployment,
                &namespace,
                &valid_custom_domains,
                env_name.as_deref(),
            )?;
            children.push(serde_json::to_value(&custom_ingress)?);
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

/// Returns true if this deployment should have K8s infrastructure (K8s Deployment resource).
pub(crate) fn should_have_infrastructure(deployment: &Deployment) -> bool {
    matches!(
        deployment.status,
        DeploymentStatus::Pushed
            | DeploymentStatus::Deploying
            | DeploymentStatus::Healthy
            | DeploymentStatus::Unhealthy
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

        let fresh = observed_refresh
            .map(|ts| {
                Utc::now().signed_duration_since(ts)
                    < chrono::Duration::seconds(IDENTITY_TOKEN_REFRESH_SECS)
            })
            .unwrap_or(false);

        if fresh && observed_tokens.keys().eq(audiences.keys()) {
            // Reuse observed tokens unchanged, preserving the refresh timestamp.
            (observed_tokens, observed_refresh.unwrap_or_else(Utc::now))
        } else {
            let sub = workload_subject(&project.name, environment_name);
            let mut minted = BTreeMap::new();
            for (filename, audience) in &audiences {
                let jwt = state.jwt_signer.sign_workload_jwt(
                    &sub,
                    &project.name,
                    environment_name.unwrap_or("_none"),
                    &deployment.deployment_group,
                    &deployment.deployment_id,
                    audience,
                    IDENTITY_TOKEN_TTL_SECS,
                )?;
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
    if let Err(e) = api
        .patch(name, &params, &kube::api::Patch::Apply(endpoints))
        .await
    {
        warn!(
            namespace = %namespace,
            "Failed to apply backend Endpoints: {:?}", e
        );
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

async fn resolve_deployment_env_vars(
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
            _tag: &str,
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
            DeploymentStatus::Terminating,
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
            namespace_format: "{project_name}".to_string(),
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
            replicas: 1,
            cpu: "500m".to_string(),
            memory: "256Mi".to_string(),
            identity_credential_hash: None,
            identity_audiences: serde_json::json!({}),
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
}
