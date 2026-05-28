use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::db::models::{Deployment, DeploymentContainers, DeploymentStatus, TerminationReason};
use crate::server::deployment::state_machine;

/// Parameters for creating a new deployment
pub struct CreateDeploymentParams<'a> {
    pub deployment_id: &'a str,
    pub project_id: Uuid,
    pub created_by_id: Uuid,
    pub status: DeploymentStatus,
    pub image: Option<&'a str>,
    pub image_digest: Option<&'a str>,
    pub rolled_back_from_deployment_id: Option<Uuid>,
    pub deployment_group: &'a str,
    pub environment_id: Option<Uuid>,
    pub expires_at: Option<DateTime<Utc>>,
    pub http_port: i32,
    pub is_active: bool,
    /// URL to the CI pipeline/job that created this deployment
    pub job_url: Option<&'a str>,
    /// URL to the pull request/merge request associated with this deployment
    pub pull_request_url: Option<&'a str>,
    /// HTTPS URL of the Git repository this deployment was created from
    pub git_repository_url: Option<&'a str>,
    /// Number of replicas
    pub replicas: i32,
    /// CPU allocation (e.g., "500m", "1")
    pub cpu: &'a str,
    /// Memory allocation (e.g., "256Mi", "1Gi")
    pub memory: &'a str,
    /// Map of { in-pod filename -> token audience } for auto-minted workload JWTs.
    pub identity_audiences: serde_json::Value,
}

/// List deployments for a project
pub async fn list_for_project(pool: &PgPool, project_id: Uuid) -> Result<Vec<Deployment>> {
    let deployments = sqlx::query_as!(
        Deployment,
        r#"
        SELECT
            id, deployment_id, project_id, created_by_id,
            status as "status: DeploymentStatus",
            deployment_group, environment_id, expires_at,
            completed_at, error_message, build_logs,
            controller_metadata as "controller_metadata: serde_json::Value",
            image, image_digest, rolled_back_from_deployment_id,
            http_port, needs_reconcile, is_active,
            deploying_started_at,
            first_healthy_at, job_url, pull_request_url, git_repository_url,
            replicas, cpu, memory,
            termination_reason as "termination_reason: _",
            created_at, updated_at, identity_credential_hash,
            identity_audiences as "identity_audiences: serde_json::Value"
        FROM deployments
        WHERE project_id = $1
        ORDER BY created_at DESC
        "#,
        project_id
    )
    .fetch_all(pool)
    .await
    .context("Failed to list deployments for project")?;

    Ok(deployments)
}

/// Resolve a project's Git repository URL from its deployment history.
///
/// Prefers the URL recorded on the active deployment in the default group
/// (the "primary" deployment); otherwise falls back to the most recent
/// deployment that has a repository URL recorded. Returns `None` when no
/// deployment carries Git metadata.
#[cfg(feature = "backend")]
pub async fn resolve_git_repository_url(pool: &PgPool, project_id: Uuid) -> Result<Option<String>> {
    let url = sqlx::query_scalar!(
        r#"
        SELECT git_repository_url
        FROM deployments
        WHERE project_id = $1 AND git_repository_url IS NOT NULL
        ORDER BY (is_active AND deployment_group = 'default') DESC, created_at DESC
        LIMIT 1
        "#,
        project_id
    )
    .fetch_optional(pool)
    .await
    .context("Failed to resolve git repository URL for project")?;

    Ok(url.flatten())
}

/// List non-terminal deployments for a project.
///
/// Returns only deployments that are not in a terminal state (Cancelled, Stopped,
/// Superseded, Failed, Expired). Used by the sync webhook to avoid loading the
/// full deployment history on every sync cycle.
#[cfg(feature = "backend")]
pub async fn list_non_terminal_for_project(
    pool: &PgPool,
    project_id: Uuid,
) -> Result<Vec<Deployment>> {
    let deployments = sqlx::query_as!(
        Deployment,
        r#"
        SELECT
            id, deployment_id, project_id, created_by_id,
            status as "status: DeploymentStatus",
            deployment_group, environment_id, expires_at,
            completed_at, error_message, build_logs,
            controller_metadata as "controller_metadata: serde_json::Value",
            image, image_digest, rolled_back_from_deployment_id,
            http_port, needs_reconcile, is_active,
            deploying_started_at,
            first_healthy_at, job_url, pull_request_url, git_repository_url,
            replicas, cpu, memory,
            termination_reason as "termination_reason: _",
            created_at, updated_at, identity_credential_hash,
            identity_audiences as "identity_audiences: serde_json::Value"
        FROM deployments
        WHERE project_id = $1
          AND NOT is_terminal(status)
        ORDER BY created_at DESC
        "#,
        project_id
    )
    .fetch_all(pool)
    .await
    .context("Failed to list non-terminal deployments for project")?;

    Ok(deployments)
}

/// Batch fetch deployments by their UUIDs
/// Returns a HashMap mapping deployment ID to Deployment
pub async fn get_deployments_batch(
    pool: &PgPool,
    deployment_ids: &[Uuid],
) -> Result<std::collections::HashMap<Uuid, Deployment>> {
    let deployments = sqlx::query_as!(
        Deployment,
        r#"
        SELECT
            id, deployment_id, project_id, created_by_id,
            status as "status: DeploymentStatus",
            deployment_group, environment_id, expires_at,
            completed_at, error_message, build_logs,
            controller_metadata as "controller_metadata: serde_json::Value",
            image, image_digest, rolled_back_from_deployment_id,
            http_port, needs_reconcile, is_active,
            deploying_started_at,
            first_healthy_at, job_url, pull_request_url, git_repository_url,
            replicas, cpu, memory,
            termination_reason as "termination_reason: _",
            created_at, updated_at, identity_credential_hash,
            identity_audiences as "identity_audiences: serde_json::Value"
        FROM deployments
        WHERE id = ANY($1)
        "#,
        deployment_ids
    )
    .fetch_all(pool)
    .await
    .context("Failed to fetch deployments batch")?;

    let mut map = std::collections::HashMap::new();
    for deployment in deployments {
        map.insert(deployment.id, deployment);
    }

    Ok(map)
}

/// Find deployment by ID (UUID)
pub async fn find_by_id(pool: &PgPool, id: Uuid) -> Result<Option<Deployment>> {
    let deployment = sqlx::query_as!(
        Deployment,
        r#"
        SELECT
            id, deployment_id, project_id, created_by_id,
            status as "status: DeploymentStatus",
            deployment_group, environment_id, expires_at,
            completed_at, error_message, build_logs,
            controller_metadata as "controller_metadata: serde_json::Value",
            image, image_digest, rolled_back_from_deployment_id,
            http_port, needs_reconcile, is_active,
            deploying_started_at,
            first_healthy_at, job_url, pull_request_url, git_repository_url,
            replicas, cpu, memory,
            termination_reason as "termination_reason: _",
            created_at, updated_at, identity_credential_hash,
            identity_audiences as "identity_audiences: serde_json::Value"
        FROM deployments
        WHERE id = $1
        "#,
        id
    )
    .fetch_optional(pool)
    .await?;

    Ok(deployment)
}

/// Find deployment by deployment_id and project_id
pub async fn find_by_deployment_id(
    pool: &PgPool,
    deployment_id: &str,
    project_id: Uuid,
) -> Result<Option<Deployment>> {
    let deployment = sqlx::query_as!(
        Deployment,
        r#"
        SELECT
            id, deployment_id, project_id, created_by_id,
            status as "status: DeploymentStatus",
            deployment_group, environment_id, expires_at,
            completed_at, error_message, build_logs,
            controller_metadata as "controller_metadata: serde_json::Value",
            image, image_digest, rolled_back_from_deployment_id,
            http_port, needs_reconcile, is_active,
            deploying_started_at,
            first_healthy_at, job_url, pull_request_url, git_repository_url,
            replicas, cpu, memory,
            termination_reason as "termination_reason: _",
            created_at, updated_at, identity_credential_hash,
            identity_audiences as "identity_audiences: serde_json::Value"
        FROM deployments
        WHERE deployment_id = $1 AND project_id = $2
        "#,
        deployment_id,
        project_id
    )
    .fetch_optional(pool)
    .await
    .context("Failed to find deployment by deployment_id")?;

    Ok(deployment)
}

/// Find deployments by deployment_id across all projects (unscoped).
///
/// Returns up to `limit` matching deployments. Used by the deprecated unscoped
/// status update endpoint to detect collisions.
pub async fn find_by_deployment_id_unscoped(
    pool: &PgPool,
    deployment_id: &str,
    limit: i64,
) -> Result<Vec<Deployment>> {
    let deployments = sqlx::query_as!(
        Deployment,
        r#"
        SELECT
            id, deployment_id, project_id, created_by_id,
            status as "status: DeploymentStatus",
            deployment_group, environment_id, expires_at,
            completed_at, error_message, build_logs,
            controller_metadata as "controller_metadata: serde_json::Value",
            image, image_digest, rolled_back_from_deployment_id,
            http_port, needs_reconcile, is_active,
            deploying_started_at,
            first_healthy_at, job_url, pull_request_url, git_repository_url,
            replicas, cpu, memory,
            termination_reason as "termination_reason: _",
            created_at, updated_at, identity_credential_hash,
            identity_audiences as "identity_audiences: serde_json::Value"
        FROM deployments
        WHERE deployment_id = $1
        ORDER BY created_at ASC
        LIMIT $2
        "#,
        deployment_id,
        limit
    )
    .fetch_all(pool)
    .await
    .context("Failed to find deployments by deployment_id (unscoped)")?;

    Ok(deployments)
}

/// Create a new deployment
pub async fn create(pool: &PgPool, params: CreateDeploymentParams<'_>) -> Result<Deployment> {
    let status_str = params.status.to_string();

    let deployment = sqlx::query_as!(
        Deployment,
        r#"
        INSERT INTO deployments (deployment_id, project_id, created_by_id, status, image, image_digest, rolled_back_from_deployment_id, deployment_group, environment_id, expires_at, http_port, is_active, job_url, pull_request_url, git_repository_url, replicas, cpu, memory, identity_audiences)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19)
        RETURNING
            id, deployment_id, project_id, created_by_id,
            status as "status: DeploymentStatus",
            deployment_group, environment_id, expires_at,
            termination_reason as "termination_reason: _",
            completed_at, error_message, build_logs,
            controller_metadata as "controller_metadata: serde_json::Value",
            image, image_digest, rolled_back_from_deployment_id,
            http_port, needs_reconcile, is_active,
            deploying_started_at,
            first_healthy_at, job_url, pull_request_url, git_repository_url,
            replicas, cpu, memory,
            created_at, updated_at, identity_credential_hash,
            identity_audiences as "identity_audiences: serde_json::Value"
        "#,
        params.deployment_id,
        params.project_id,
        params.created_by_id,
        status_str,
        params.image,
        params.image_digest,
        params.rolled_back_from_deployment_id,
        params.deployment_group,
        params.environment_id,
        params.expires_at,
        params.http_port,
        params.is_active,
        params.job_url,
        params.pull_request_url,
        params.git_repository_url,
        params.replicas,
        params.cpu,
        params.memory,
        params.identity_audiences
    )
    .fetch_one(pool)
    .await
    .context("Failed to create deployment")?;

    Ok(deployment)
}

/// Update deployment status
///
/// Validates state transition using the state machine before updating.
/// Returns error if the transition is invalid or if the deployment doesn't exist.
pub async fn update_status(
    pool: &PgPool,
    id: Uuid,
    status: DeploymentStatus,
) -> Result<Deployment> {
    // Fetch current deployment to validate state transition
    let current = sqlx::query_as!(
        Deployment,
        r#"
        SELECT
            id, deployment_id, project_id, created_by_id,
            status as "status: DeploymentStatus",
            deployment_group, environment_id, expires_at,
            completed_at, error_message, build_logs,
            controller_metadata as "controller_metadata: serde_json::Value",
            image, image_digest, rolled_back_from_deployment_id,
            http_port, needs_reconcile, is_active,
            deploying_started_at,
            first_healthy_at, job_url, pull_request_url, git_repository_url,
            replicas, cpu, memory,
            termination_reason as "termination_reason: _",
            created_at, updated_at, identity_credential_hash,
            identity_audiences as "identity_audiences: serde_json::Value"
        FROM deployments
        WHERE id = $1
        "#,
        id
    )
    .fetch_optional(pool)
    .await
    .context("Failed to fetch deployment for status update")?;

    let current = current.ok_or_else(|| anyhow::anyhow!("Deployment not found"))?;

    // Validate state transition
    state_machine::validate_transition(&current.status, &status)?;

    // Perform the update
    // Set deploying_started_at when transitioning to Deploying status (if not already set)
    let status_str = status.to_string();
    let deployment = sqlx::query_as!(
        Deployment,
        r#"
        UPDATE deployments
        SET status = $2,
            deploying_started_at = CASE
                WHEN $2 = 'Deploying' AND deploying_started_at IS NULL THEN NOW()
                ELSE deploying_started_at
            END,
            first_healthy_at = CASE
                WHEN $2 = 'Healthy' AND first_healthy_at IS NULL THEN NOW()
                ELSE first_healthy_at
            END,
            completed_at = CASE
                WHEN $2 IN ('Cancelled', 'Stopped', 'Superseded', 'Expired', 'Failed')
                    AND completed_at IS NULL THEN NOW()
                ELSE completed_at
            END
        WHERE id = $1
        RETURNING
            id, deployment_id, project_id, created_by_id,
            status as "status: DeploymentStatus",
            deployment_group, environment_id, expires_at,
            completed_at, error_message, build_logs,
            controller_metadata as "controller_metadata: serde_json::Value",
            image, image_digest, rolled_back_from_deployment_id,
            http_port, needs_reconcile, is_active,
            deploying_started_at,
            first_healthy_at, job_url, pull_request_url, git_repository_url,
            replicas, cpu, memory,
            termination_reason as "termination_reason: _",
            created_at, updated_at, identity_credential_hash,
            identity_audiences as "identity_audiences: serde_json::Value"
        "#,
        id,
        status_str
    )
    .fetch_optional(pool)
    .await
    .context("Failed to execute deployment status update")?;

    match deployment {
        Some(d) => Ok(d),
        None => {
            tracing::warn!(
                "UPDATE returned 0 rows for deployment {} (transition {} -> {}), but validation passed",
                current.deployment_id,
                current.status,
                status
            );
            bail!("Failed to update deployment status: deployment may have been modified concurrently");
        }
    }
}

/// Mark deployment as failed
pub async fn mark_failed(pool: &PgPool, id: Uuid, error_message: &str) -> Result<Deployment> {
    let deployment = sqlx::query_as!(
        Deployment,
        r#"
        UPDATE deployments
        SET status = 'Failed', completed_at = COALESCE(completed_at, NOW()), error_message = $2
        WHERE id = $1
        RETURNING
            id, deployment_id, project_id, created_by_id,
            status as "status: DeploymentStatus",
            deployment_group, environment_id, expires_at,
            completed_at, error_message, build_logs,
            controller_metadata as "controller_metadata: serde_json::Value",
            image, image_digest, rolled_back_from_deployment_id,
            http_port, needs_reconcile, is_active,
            deploying_started_at,
            first_healthy_at, job_url, pull_request_url, git_repository_url,
            replicas, cpu, memory,
            termination_reason as "termination_reason: _",
            created_at, updated_at, identity_credential_hash,
            identity_audiences as "identity_audiences: serde_json::Value"
        "#,
        id,
        error_message
    )
    .fetch_one(pool)
    .await
    .context("Failed to mark deployment as failed")?;

    Ok(deployment)
}

/// Find deployments in non-terminal states for reconciliation
/// Update controller metadata
#[cfg(feature = "backend")]
pub async fn update_controller_metadata(
    pool: &PgPool,
    id: Uuid,
    metadata: &serde_json::Value,
) -> Result<Deployment> {
    let deployment = sqlx::query_as!(
        Deployment,
        r#"
        UPDATE deployments
        SET controller_metadata = $2
        WHERE id = $1
        RETURNING
            id, deployment_id, project_id, created_by_id,
            status as "status: DeploymentStatus",
            deployment_group, environment_id, expires_at,
            completed_at, error_message, build_logs,
            controller_metadata as "controller_metadata: serde_json::Value",
            image, image_digest, rolled_back_from_deployment_id,
            http_port, needs_reconcile, is_active,
            deploying_started_at,
            first_healthy_at, job_url, pull_request_url, git_repository_url,
            replicas, cpu, memory,
            termination_reason as "termination_reason: _",
            created_at, updated_at, identity_credential_hash,
            identity_audiences as "identity_audiences: serde_json::Value"
        "#,
        id,
        metadata
    )
    .fetch_one(pool)
    .await
    .context("Failed to update controller metadata")?;

    Ok(deployment)
}

/// Find deployment by project_id and deployment_id (for CLI commands)
pub async fn find_by_project_and_deployment_id(
    pool: &PgPool,
    project_id: Uuid,
    deployment_id: &str,
) -> Result<Option<Deployment>> {
    // This is the same as find_by_deployment_id, but with explicit naming for CLI use
    find_by_deployment_id(pool, deployment_id, project_id).await
}

/// Mark deployment as cancelled
#[cfg(feature = "backend")]
pub async fn mark_cancelled(pool: &PgPool, id: Uuid) -> Result<Deployment> {
    let deployment = sqlx::query_as!(
        Deployment,
        r#"
        UPDATE deployments
        SET
            status = 'Cancelled',
            termination_reason = 'Cancelled',
            controller_metadata = '{}',
            completed_at = COALESCE(completed_at, NOW()),
            updated_at = NOW()
        WHERE id = $1
        RETURNING
            id, deployment_id, project_id, created_by_id,
            status as "status: DeploymentStatus",
            deployment_group, environment_id, expires_at,
            termination_reason as "termination_reason: _",
            completed_at, error_message, build_logs,
            controller_metadata as "controller_metadata: serde_json::Value",
            image, image_digest, rolled_back_from_deployment_id,
            http_port, needs_reconcile, is_active,
            deploying_started_at,
            first_healthy_at, job_url, pull_request_url, git_repository_url,
            replicas, cpu, memory,
            created_at, updated_at, identity_credential_hash,
            identity_audiences as "identity_audiences: serde_json::Value"
        "#,
        id
    )
    .fetch_one(pool)
    .await
    .context("Failed to mark deployment as cancelled")?;

    Ok(deployment)
}

/// Mark deployment as stopped (user-initiated termination)
#[cfg(feature = "backend")]
pub async fn mark_stopped(pool: &PgPool, id: Uuid) -> Result<Deployment> {
    let deployment = sqlx::query_as!(
        Deployment,
        r#"
        UPDATE deployments
        SET
            status = 'Stopped',
            termination_reason = 'UserStopped',
            controller_metadata = '{}',
            completed_at = COALESCE(completed_at, NOW()),
            updated_at = NOW()
        WHERE id = $1
        RETURNING
            id, deployment_id, project_id, created_by_id,
            status as "status: DeploymentStatus",
            deployment_group, environment_id, expires_at,
            termination_reason as "termination_reason: _",
            completed_at, error_message, build_logs,
            controller_metadata as "controller_metadata: serde_json::Value",
            image, image_digest, rolled_back_from_deployment_id,
            http_port, needs_reconcile, is_active,
            deploying_started_at,
            first_healthy_at, job_url, pull_request_url, git_repository_url,
            replicas, cpu, memory,
            created_at, updated_at, identity_credential_hash,
            identity_audiences as "identity_audiences: serde_json::Value"
        "#,
        id
    )
    .fetch_one(pool)
    .await
    .context("Failed to mark deployment as stopped")?;

    Ok(deployment)
}

/// Mark deployment as superseded (replaced by newer deployment)
#[cfg(feature = "backend")]
pub async fn mark_superseded(pool: &PgPool, id: Uuid) -> Result<Deployment> {
    let deployment = sqlx::query_as!(
        Deployment,
        r#"
        UPDATE deployments
        SET
            status = 'Superseded',
            termination_reason = 'Superseded',
            controller_metadata = '{}',
            completed_at = COALESCE(completed_at, NOW()),
            updated_at = NOW()
        WHERE id = $1
        RETURNING
            id, deployment_id, project_id, created_by_id,
            status as "status: DeploymentStatus",
            deployment_group, environment_id, expires_at,
            termination_reason as "termination_reason: _",
            completed_at, error_message, build_logs,
            controller_metadata as "controller_metadata: serde_json::Value",
            image, image_digest, rolled_back_from_deployment_id,
            http_port, needs_reconcile, is_active,
            deploying_started_at,
            first_healthy_at, job_url, pull_request_url, git_repository_url,
            replicas, cpu, memory,
            created_at, updated_at, identity_credential_hash,
            identity_audiences as "identity_audiences: serde_json::Value"
        "#,
        id
    )
    .fetch_one(pool)
    .await
    .context("Failed to mark deployment as superseded")?;

    Ok(deployment)
}

/// Mark a deployment as expired (terminal state for deployments that timed out)
#[cfg(feature = "backend")]
pub async fn mark_expired(pool: &PgPool, id: Uuid) -> Result<Deployment> {
    let deployment = sqlx::query_as!(
        Deployment,
        r#"
        UPDATE deployments
        SET
            status = 'Expired',
            termination_reason = 'Expired',
            controller_metadata = '{}',
            completed_at = COALESCE(completed_at, NOW()),
            updated_at = NOW()
        WHERE id = $1
        RETURNING
            id, deployment_id, project_id, created_by_id,
            status as "status: DeploymentStatus",
            deployment_group, environment_id, expires_at,
            termination_reason as "termination_reason: _",
            completed_at, error_message, build_logs,
            controller_metadata as "controller_metadata: serde_json::Value",
            image, image_digest, rolled_back_from_deployment_id,
            http_port, needs_reconcile, is_active,
            deploying_started_at,
            first_healthy_at, job_url, pull_request_url, git_repository_url,
            replicas, cpu, memory,
            created_at, updated_at, identity_credential_hash,
            identity_audiences as "identity_audiences: serde_json::Value"
        "#,
        id
    )
    .fetch_one(pool)
    .await
    .context("Failed to mark deployment as expired")?;

    Ok(deployment)
}

/// Mark deployment as healthy
#[cfg(feature = "backend")]
pub async fn mark_healthy(pool: &PgPool, id: Uuid) -> Result<Deployment> {
    let deployment = sqlx::query_as!(
        Deployment,
        r#"
        UPDATE deployments
        SET
            status = 'Healthy',
            error_message = NULL,
            first_healthy_at = COALESCE(first_healthy_at, NOW()),
            updated_at = NOW()
        WHERE id = $1
        RETURNING
            id, deployment_id, project_id, created_by_id,
            status as "status: DeploymentStatus",
            deployment_group, environment_id, expires_at,
            termination_reason as "termination_reason: _",
            completed_at, error_message, build_logs,
            controller_metadata as "controller_metadata: serde_json::Value",
            image, image_digest, rolled_back_from_deployment_id,
            http_port, needs_reconcile, is_active,
            deploying_started_at,
            first_healthy_at, job_url, pull_request_url, git_repository_url,
            replicas, cpu, memory,
            created_at, updated_at, identity_credential_hash,
            identity_audiences as "identity_audiences: serde_json::Value"
        "#,
        id
    )
    .fetch_one(pool)
    .await
    .context("Failed to mark deployment as healthy")?;

    Ok(deployment)
}

/// Mark deployment as unhealthy with reason
#[cfg(feature = "backend")]
pub async fn mark_unhealthy(pool: &PgPool, id: Uuid, reason: String) -> Result<Deployment> {
    let deployment = sqlx::query_as!(
        Deployment,
        r#"
        UPDATE deployments
        SET
            status = 'Unhealthy',
            error_message = $2,
            updated_at = NOW()
        WHERE id = $1
        RETURNING
            id, deployment_id, project_id, created_by_id,
            status as "status: DeploymentStatus",
            deployment_group, environment_id, expires_at,
            termination_reason as "termination_reason: _",
            completed_at, error_message, build_logs,
            controller_metadata as "controller_metadata: serde_json::Value",
            image, image_digest, rolled_back_from_deployment_id,
            http_port, needs_reconcile, is_active,
            deploying_started_at,
            first_healthy_at, job_url, pull_request_url, git_repository_url,
            replicas, cpu, memory,
            created_at, updated_at, identity_credential_hash,
            identity_audiences as "identity_audiences: serde_json::Value"
        "#,
        id,
        reason
    )
    .fetch_one(pool)
    .await
    .context("Failed to mark deployment as unhealthy")?;

    Ok(deployment)
}

/// Mark deployment as terminating with reason
pub async fn mark_terminating(
    pool: &PgPool,
    id: Uuid,
    reason: TerminationReason,
) -> Result<Deployment> {
    let deployment = sqlx::query_as!(
        Deployment,
        r#"
        UPDATE deployments
        SET
            status = 'Terminating',
            termination_reason = $2,
            updated_at = NOW()
        WHERE id = $1
        RETURNING
            id, deployment_id, project_id, created_by_id,
            status as "status: DeploymentStatus",
            deployment_group, environment_id, expires_at,
            termination_reason as "termination_reason: _",
            completed_at, error_message, build_logs,
            controller_metadata as "controller_metadata: serde_json::Value",
            image, image_digest, rolled_back_from_deployment_id,
            http_port, needs_reconcile, is_active,
            deploying_started_at,
            first_healthy_at, job_url, pull_request_url, git_repository_url,
            replicas, cpu, memory,
            created_at, updated_at, identity_credential_hash,
            identity_audiences as "identity_audiences: serde_json::Value"
        "#,
        id,
        reason as TerminationReason
    )
    .fetch_one(pool)
    .await
    .context("Failed to mark deployment as terminating")?;

    Ok(deployment)
}

/// Mark deployment as cancelling
pub async fn mark_cancelling(pool: &PgPool, id: Uuid) -> Result<Deployment> {
    let deployment = sqlx::query_as!(
        Deployment,
        r#"
        UPDATE deployments
        SET
            status = 'Cancelling',
            termination_reason = 'Cancelled',
            updated_at = NOW()
        WHERE id = $1
        RETURNING
            id, deployment_id, project_id, created_by_id,
            status as "status: DeploymentStatus",
            deployment_group, environment_id, expires_at,
            termination_reason as "termination_reason: _",
            completed_at, error_message, build_logs,
            controller_metadata as "controller_metadata: serde_json::Value",
            image, image_digest, rolled_back_from_deployment_id,
            http_port, needs_reconcile, is_active,
            deploying_started_at,
            first_healthy_at, job_url, pull_request_url, git_repository_url,
            replicas, cpu, memory,
            created_at, updated_at, identity_credential_hash,
            identity_audiences as "identity_audiences: serde_json::Value"
        "#,
        id
    )
    .fetch_one(pool)
    .await
    .context("Failed to mark deployment as cancelling")?;

    Ok(deployment)
}

/// Find a deployment by its workload-identity bootstrap credential hash.
///
/// **Security note**: the caller MUST filter the result through
/// `should_have_infrastructure` before treating the deployment as a valid
/// token-exchange subject. A hash match alone is not sufficient — the
/// deployment must also have live infrastructure (i.e. be in a status where
/// tokens are meaningful). See `should_have_infrastructure` in
/// `server::deployment::webhook`.
pub async fn get_by_identity_credential_hash(
    pool: &PgPool,
    hash: &str,
) -> Result<Option<Deployment>> {
    let deployment = sqlx::query_as!(
        Deployment,
        r#"
        SELECT
            id, deployment_id, project_id, created_by_id,
            status as "status: DeploymentStatus",
            deployment_group, environment_id, expires_at,
            termination_reason as "termination_reason: _",
            completed_at, error_message, build_logs,
            controller_metadata as "controller_metadata: serde_json::Value",
            image, image_digest, rolled_back_from_deployment_id,
            http_port, needs_reconcile, is_active,
            deploying_started_at,
            first_healthy_at, job_url, pull_request_url, git_repository_url,
            replicas, cpu, memory,
            created_at, updated_at, identity_credential_hash,
            identity_audiences as "identity_audiences: serde_json::Value"
        FROM deployments
        WHERE identity_credential_hash = $1
        "#,
        hash
    )
    .fetch_optional(pool)
    .await
    .context("Failed to find deployment by identity credential hash")?;

    Ok(deployment)
}

/// Persist the SHA-256 hash of a deployment's workload-identity bootstrap credential.
///
/// Called by the controller after generating the credential during reconciliation.
pub async fn set_identity_credential_hash(pool: &PgPool, id: Uuid, hash: &str) -> Result<()> {
    sqlx::query!(
        "UPDATE deployments SET identity_credential_hash = $2, updated_at = NOW() WHERE id = $1",
        id,
        hash
    )
    .execute(pool)
    .await
    .context("Failed to set identity credential hash")?;

    Ok(())
}

/// Find active deployment for a project in a specific group
/// Active = most recent Healthy deployment in the group
#[cfg(feature = "backend")]
pub async fn find_active_for_project_and_group(
    pool: &PgPool,
    project_id: Uuid,
    group: &str,
) -> Result<Option<Deployment>> {
    let deployment = sqlx::query_as!(
        Deployment,
        r#"
        SELECT
            id, deployment_id, project_id, created_by_id,
            status as "status: DeploymentStatus",
                        deployment_group, environment_id, expires_at,
            termination_reason as "termination_reason: _",
            completed_at, error_message, build_logs,
            controller_metadata as "controller_metadata: serde_json::Value",
            image, image_digest, rolled_back_from_deployment_id,
            http_port, needs_reconcile, is_active,
            deploying_started_at,
            first_healthy_at, job_url, pull_request_url, git_repository_url,
            replicas, cpu, memory,
            created_at, updated_at, identity_credential_hash,
            identity_audiences as "identity_audiences: serde_json::Value"
        FROM deployments
        WHERE project_id = $1
          AND deployment_group = $2
          AND status = 'Healthy'
        ORDER BY created_at DESC
        LIMIT 1
        "#,
        project_id,
        group
    )
    .fetch_optional(pool)
    .await
    .context("Failed to find active deployment for project and group")?;

    Ok(deployment)
}

/// Find non-terminal deployments for a project in a specific group
pub async fn find_non_terminal_for_project_and_group(
    pool: &PgPool,
    project_id: Uuid,
    group: &str,
) -> Result<Vec<Deployment>> {
    let deployments = sqlx::query_as!(
        Deployment,
        r#"
        SELECT
            id, deployment_id, project_id, created_by_id,
            status as "status: DeploymentStatus",
                        deployment_group, environment_id, expires_at,
            termination_reason as "termination_reason: _",
            completed_at, error_message, build_logs,
            controller_metadata as "controller_metadata: serde_json::Value",
            image, image_digest, rolled_back_from_deployment_id,
            http_port, needs_reconcile, is_active,
            deploying_started_at,
            first_healthy_at, job_url, pull_request_url, git_repository_url,
            replicas, cpu, memory,
            created_at, updated_at, identity_credential_hash,
            identity_audiences as "identity_audiences: serde_json::Value"
        FROM deployments
        WHERE project_id = $1
          AND deployment_group = $2
          AND NOT is_terminal(status)
        ORDER BY created_at DESC
        "#,
        project_id,
        group
    )
    .fetch_all(pool)
    .await
    .context("Failed to find non-terminal deployments for project and group")?;

    Ok(deployments)
}

/// Find the active deployment for a project in a specific group
/// Returns the deployment marked as is_active=true in the specified group
pub async fn find_active_deployment_for_group(
    pool: &PgPool,
    project_id: Uuid,
    group: &str,
) -> Result<Option<Deployment>> {
    let deployment = sqlx::query_as!(
        Deployment,
        r#"
        SELECT
            id, deployment_id, project_id, created_by_id,
            status as "status: DeploymentStatus",
            deployment_group, environment_id, expires_at,
            termination_reason as "termination_reason: _",
            completed_at, error_message, build_logs,
            controller_metadata as "controller_metadata: serde_json::Value",
            image, image_digest, rolled_back_from_deployment_id,
            http_port, needs_reconcile, is_active,
            deploying_started_at,
            first_healthy_at, job_url, pull_request_url, git_repository_url,
            replicas, cpu, memory,
            created_at, updated_at, identity_credential_hash,
            identity_audiences as "identity_audiences: serde_json::Value"
        FROM deployments
        WHERE project_id = $1
          AND deployment_group = $2
          AND is_active = TRUE
        LIMIT 1
        "#,
        project_id,
        group
    )
    .fetch_optional(pool)
    .await
    .context("Failed to find active deployment for project and group")?;

    Ok(deployment)
}

/// Find last deployment for a project in a specific group
/// Returns the most recent deployment regardless of status
pub async fn find_last_for_project_and_group(
    pool: &PgPool,
    project_id: Uuid,
    group: &str,
) -> Result<Option<Deployment>> {
    let deployment = sqlx::query_as!(
        Deployment,
        r#"
        SELECT
            id, deployment_id, project_id, created_by_id,
            status as "status: DeploymentStatus",
            deployment_group, environment_id, expires_at,
            termination_reason as "termination_reason: _",
            completed_at, error_message, build_logs,
            controller_metadata as "controller_metadata: serde_json::Value",
            image, image_digest, rolled_back_from_deployment_id,
            http_port, needs_reconcile, is_active,
            deploying_started_at,
            first_healthy_at, job_url, pull_request_url, git_repository_url,
            replicas, cpu, memory,
            created_at, updated_at, identity_credential_hash,
            identity_audiences as "identity_audiences: serde_json::Value"
        FROM deployments
        WHERE project_id = $1
          AND deployment_group = $2
        ORDER BY created_at DESC
        LIMIT 1
        "#,
        project_id,
        group
    )
    .fetch_optional(pool)
    .await
    .context("Failed to find last deployment for project and group")?;

    Ok(deployment)
}

/// List deployments for a project with optional group filter
pub async fn list_for_project_and_group(
    pool: &PgPool,
    project_id: Uuid,
    group: Option<&str>,
    limit: Option<i64>,
    offset: Option<i64>,
) -> Result<Vec<Deployment>> {
    let limit_value = limit.unwrap_or(10);
    let offset_value = offset.unwrap_or(0);

    let deployments = if let Some(g) = group {
        sqlx::query_as!(
            Deployment,
            r#"
            SELECT
                id, deployment_id, project_id, created_by_id,
                status as "status: DeploymentStatus",
                deployment_group, environment_id, expires_at,
                termination_reason as "termination_reason: _",
                completed_at, error_message, build_logs,
                controller_metadata as "controller_metadata: serde_json::Value",
                image, image_digest, rolled_back_from_deployment_id,
                http_port, needs_reconcile, is_active,
                deploying_started_at,
                first_healthy_at, job_url, pull_request_url, git_repository_url,
                replicas, cpu, memory,
                created_at, updated_at, identity_credential_hash,
                identity_audiences as "identity_audiences: serde_json::Value"
            FROM deployments
            WHERE project_id = $1 AND deployment_group = $2
            ORDER BY created_at DESC
            LIMIT $3 OFFSET $4
            "#,
            project_id,
            g,
            limit_value,
            offset_value
        )
        .fetch_all(pool)
        .await?
    } else {
        // No group filter - return all for project with pagination
        sqlx::query_as!(
            Deployment,
            r#"
            SELECT
                id, deployment_id, project_id, created_by_id,
                status as "status: DeploymentStatus",
                deployment_group, environment_id, expires_at,
                termination_reason as "termination_reason: _",
                completed_at, error_message, build_logs,
                controller_metadata as "controller_metadata: serde_json::Value",
                image, image_digest, rolled_back_from_deployment_id,
                http_port, needs_reconcile, is_active,
                deploying_started_at,
                first_healthy_at, job_url, pull_request_url, git_repository_url,
                replicas, cpu, memory,
                created_at, updated_at, identity_credential_hash,
                identity_audiences as "identity_audiences: serde_json::Value"
            FROM deployments
            WHERE project_id = $1
            ORDER BY created_at DESC
            LIMIT $2 OFFSET $3
            "#,
            project_id,
            limit_value,
            offset_value
        )
        .fetch_all(pool)
        .await?
    };

    Ok(deployments)
}

/// Get all active deployment groups for a project
/// Returns deployment groups based on the following rules:
/// - "default" group: always included if it has any deployments (regardless of status)
/// - Other groups: only included if they have at least one non-terminal deployment
pub async fn get_active_deployment_groups(pool: &PgPool, project_id: Uuid) -> Result<Vec<String>> {
    let groups = sqlx::query_scalar!(
        r#"
        SELECT DISTINCT deployment_group
        FROM deployments
        WHERE project_id = $1
          AND (
            -- Always include default group if it has any deployments
            deployment_group = 'default'
            OR
            -- Include other groups only if they have non-terminal deployments
            (deployment_group != 'default' AND NOT is_terminal(status))
          )
        ORDER BY deployment_group
        "#,
        project_id
    )
    .fetch_all(pool)
    .await?;

    Ok(groups)
}

/// Get all deployment groups for a project, sorted by activity
/// Returns deployment groups in the following order:
/// 1. "default" (if it exists)
/// 2. Other groups with active (Healthy) deployments (alphabetically)
/// 3. Other groups without active deployments (alphabetically)
pub async fn get_all_deployment_groups(pool: &PgPool, project_id: Uuid) -> Result<Vec<String>> {
    let groups = sqlx::query_scalar!(
        r#"
        WITH group_priority AS (
            SELECT
                deployment_group,
                CASE
                    WHEN deployment_group = 'default' THEN 0
                    WHEN EXISTS (
                        SELECT 1 FROM deployments d2
                        WHERE d2.project_id = $1
                        AND d2.deployment_group = deployments.deployment_group
                        AND d2.status = 'Healthy'
                    ) THEN 1
                    ELSE 2
                END as priority
            FROM deployments
            WHERE project_id = $1
            GROUP BY deployment_group
        )
        SELECT deployment_group
        FROM group_priority
        ORDER BY priority, deployment_group
        "#,
        project_id
    )
    .fetch_all(pool)
    .await?;

    Ok(groups)
}

/// Mark a deployment as active, automatically unmarking others in same (project, group)
///
/// The database constraint ensures only one deployment can be active per (project_id, deployment_group).
/// This function uses a transaction to:
/// 1. Unmark all other deployments in the same (project, group) as inactive
/// 2. Mark the target deployment as active
pub async fn mark_as_active(
    pool: &PgPool,
    deployment_id: Uuid,
    project_id: Uuid,
    deployment_group: &str,
) -> Result<()> {
    let mut tx = pool.begin().await?;

    // First, unmark all deployments in this project/group as inactive
    sqlx::query!(
        "UPDATE deployments
         SET is_active = FALSE, updated_at = NOW()
         WHERE project_id = $1 AND deployment_group = $2 AND is_active = TRUE",
        project_id,
        deployment_group
    )
    .execute(&mut *tx)
    .await?;

    // Then mark the target deployment as active
    sqlx::query!(
        "UPDATE deployments
         SET is_active = TRUE, updated_at = NOW()
         WHERE id = $1",
        deployment_id
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(())
}

/// Get all active deployments for a project across all deployment groups
pub async fn get_active_deployments_for_project(
    pool: &PgPool,
    project_id: Uuid,
) -> Result<Vec<Deployment>> {
    let deployments = sqlx::query_as!(
        Deployment,
        r#"
        SELECT
            id, deployment_id, project_id, created_by_id,
            status as "status: DeploymentStatus",
            deployment_group, environment_id, expires_at,
            completed_at, error_message, build_logs,
            controller_metadata as "controller_metadata: serde_json::Value",
            image, image_digest, rolled_back_from_deployment_id,
            http_port, needs_reconcile, is_active,
            deploying_started_at,
            first_healthy_at, job_url, pull_request_url, git_repository_url,
            replicas, cpu, memory,
            termination_reason as "termination_reason: _",
            created_at, updated_at, identity_credential_hash,
            identity_audiences as "identity_audiences: serde_json::Value"
        FROM deployments
        WHERE project_id = $1 AND is_active = TRUE
        ORDER BY deployment_group, created_at DESC
        "#,
        project_id
    )
    .fetch_all(pool)
    .await?;

    Ok(deployments)
}

/// Persist multi-container side-data on a deployment row. Pass `None` for
/// `containers`/`routes` to clear them (single-container path).
pub async fn set_containers(
    pool: &PgPool,
    deployment_id: Uuid,
    containers: Option<&serde_json::Value>,
    routes: Option<&serde_json::Value>,
) -> Result<()> {
    sqlx::query!(
        "UPDATE deployments SET containers = $1, routes = $2 WHERE id = $3",
        containers,
        routes,
        deployment_id
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Load multi-container side-data for a deployment. Both fields are `None`
/// for legacy single-container deployments.
pub async fn get_containers(pool: &PgPool, deployment_id: Uuid) -> Result<DeploymentContainers> {
    let row = sqlx::query!(
        r#"SELECT containers as "containers: serde_json::Value",
                  routes as "routes: serde_json::Value"
           FROM deployments WHERE id = $1"#,
        deployment_id
    )
    .fetch_one(pool)
    .await?;
    Ok(DeploymentContainers {
        containers: row.containers,
        routes: row.routes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::deployment::state_machine;

    fn str_to_status(s: &str) -> DeploymentStatus {
        match s {
            "Pending" => DeploymentStatus::Pending,
            "Building" => DeploymentStatus::Building,
            "Pushing" => DeploymentStatus::Pushing,
            "Pushed" => DeploymentStatus::Pushed,
            "Deploying" => DeploymentStatus::Deploying,
            "Healthy" => DeploymentStatus::Healthy,
            "Unhealthy" => DeploymentStatus::Unhealthy,
            "Cancelling" => DeploymentStatus::Cancelling,
            "Cancelled" => DeploymentStatus::Cancelled,
            "Terminating" => DeploymentStatus::Terminating,
            "Stopped" => DeploymentStatus::Stopped,
            "Superseded" => DeploymentStatus::Superseded,
            "Failed" => DeploymentStatus::Failed,
            "Expired" => DeploymentStatus::Expired,
            _ => panic!("Unknown status: {}", s),
        }
    }

    /// Test that PostgreSQL is_terminal() function matches Rust is_terminal() function
    #[sqlx::test]
    async fn db_is_terminal_matches_rust_is_terminal(pool: PgPool) {
        // Test all deployment statuses
        let statuses = vec![
            ("Pending", false),
            ("Building", false),
            ("Pushing", false),
            ("Pushed", false),
            ("Deploying", false),
            ("Healthy", false),
            ("Unhealthy", false),
            ("Cancelling", false),
            ("Terminating", false),
            ("Cancelled", true),
            ("Stopped", true),
            ("Superseded", true),
            ("Failed", true),
            ("Expired", true),
        ];

        for (status_str, expected) in statuses {
            // Test PostgreSQL function
            let result: bool = sqlx::query_scalar("SELECT is_terminal($1)")
                .bind(status_str)
                .fetch_one(&pool)
                .await
                .unwrap();

            assert_eq!(
                result, expected,
                "is_terminal({}) returned {} but expected {}",
                status_str, result, expected
            );

            // Also verify Rust function matches
            let status = str_to_status(status_str);
            assert_eq!(
                state_machine::is_terminal(&status),
                expected,
                "Rust is_terminal mismatch for {}",
                status_str
            );
        }
    }

    /// Test that PostgreSQL is_cancellable() function matches Rust is_cancellable() function
    #[sqlx::test]
    async fn db_is_cancellable_matches_rust_is_cancellable(pool: PgPool) {
        let statuses = vec![
            ("Pending", true),
            ("Building", true),
            ("Pushing", true),
            ("Pushed", true),
            ("Deploying", true),
            ("Healthy", false),
            ("Unhealthy", false),
            ("Cancelling", false),
            ("Terminating", false),
            ("Cancelled", false),
            ("Stopped", false),
            ("Superseded", false),
            ("Failed", false),
            ("Expired", false),
        ];

        for (status_str, expected) in statuses {
            // Test PostgreSQL function
            let result: bool = sqlx::query_scalar("SELECT is_cancellable($1)")
                .bind(status_str)
                .fetch_one(&pool)
                .await
                .unwrap();

            assert_eq!(
                result, expected,
                "is_cancellable({}) returned {} but expected {}",
                status_str, result, expected
            );

            // Also verify Rust function matches
            let status = str_to_status(status_str);
            assert_eq!(
                state_machine::is_cancellable(&status),
                expected,
                "Rust is_cancellable mismatch for {}",
                status_str
            );
        }
    }

    /// Test that PostgreSQL is_active() function matches Rust is_active() function
    #[sqlx::test]
    async fn db_is_active_matches_rust_is_active(pool: PgPool) {
        let statuses = vec![
            ("Pending", false),
            ("Building", false),
            ("Pushing", false),
            ("Pushed", false),
            ("Deploying", false),
            ("Healthy", true),
            ("Unhealthy", true),
            ("Cancelling", false),
            ("Terminating", false),
            ("Cancelled", false),
            ("Stopped", false),
            ("Superseded", false),
            ("Failed", false),
            ("Expired", false),
        ];

        for (status_str, expected) in statuses {
            // Test PostgreSQL function
            let result: bool = sqlx::query_scalar("SELECT is_active($1)")
                .bind(status_str)
                .fetch_one(&pool)
                .await
                .unwrap();

            assert_eq!(
                result, expected,
                "is_active({}) returned {} but expected {}",
                status_str, result, expected
            );

            // Also verify Rust function matches
            let status = str_to_status(status_str);
            assert_eq!(
                state_machine::is_active(&status),
                expected,
                "Rust is_active mismatch for {}",
                status_str
            );
        }
    }

    /// Test that PostgreSQL is_protected() function includes all terminal and cleanup states
    #[sqlx::test]
    async fn db_is_protected_includes_terminal_and_cleanup(pool: PgPool) {
        let statuses = vec![
            ("Pending", false),
            ("Building", false),
            ("Pushing", false),
            ("Pushed", false),
            ("Deploying", false),
            ("Healthy", false),
            ("Unhealthy", false),
            ("Cancelling", true),  // Cleanup state
            ("Terminating", true), // Cleanup state
            ("Cancelled", true),   // Terminal
            ("Stopped", true),     // Terminal
            ("Superseded", true),  // Terminal
            ("Failed", true),      // Terminal
            ("Expired", true),     // Terminal
        ];

        for (status_str, expected) in statuses {
            let result: bool = sqlx::query_scalar("SELECT is_protected($1)")
                .bind(status_str)
                .fetch_one(&pool)
                .await
                .unwrap();

            assert_eq!(
                result, expected,
                "is_protected({}) returned {} but expected {}",
                status_str, result, expected
            );
        }
    }

    /// Test that deploying_started_at is set on first transition to Deploying and not overwritten
    #[sqlx::test]
    async fn deploying_started_at_set_once_on_deploying_transition(pool: PgPool) {
        use uuid::Uuid;

        // Create a test project and user
        let project_id = Uuid::new_v4();
        let user_id = Uuid::new_v4();

        // Create user first
        sqlx::query!(
            "INSERT INTO users (id, email) VALUES ($1, $2)",
            user_id,
            "test@example.com"
        )
        .execute(&pool)
        .await
        .unwrap();

        // Create project
        sqlx::query!(
            "INSERT INTO projects (id, name, owner_user_id, access_class, status) VALUES ($1, $2, $3, $4, $5)",
            project_id,
            "test-project",
            user_id,
            "public",
            "Stopped"
        )
        .execute(&pool)
        .await
        .unwrap();

        // Create deployment in Pushed status so transition to Deploying is valid
        let deployment = create(
            &pool,
            CreateDeploymentParams {
                deployment_id: "test-deploy",
                project_id,
                created_by_id: user_id,
                status: DeploymentStatus::Pushed,
                image: None,
                image_digest: None,
                rolled_back_from_deployment_id: None,
                deployment_group: "default",
                environment_id: None,
                expires_at: None,
                http_port: 8080,
                is_active: false,
                job_url: None,
                pull_request_url: None,
                git_repository_url: None,
                replicas: 1,
                cpu: "500m",
                memory: "256Mi",
                identity_audiences: serde_json::json!({}),
            },
        )
        .await
        .unwrap();

        // Verify deploying_started_at is NULL initially
        assert!(deployment.deploying_started_at.is_none());

        // Transition to Deploying
        let deployment = update_status(&pool, deployment.id, DeploymentStatus::Deploying)
            .await
            .unwrap();

        // Verify deploying_started_at is now set
        assert!(deployment.deploying_started_at.is_some());
        let first_timestamp = deployment.deploying_started_at.unwrap();

        // Wait a bit to ensure time has passed
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Transition to Deploying again (same-state transition is valid and should not overwrite)
        let deployment = update_status(&pool, deployment.id, DeploymentStatus::Deploying)
            .await
            .unwrap();

        // Verify deploying_started_at is unchanged
        assert_eq!(deployment.deploying_started_at, Some(first_timestamp));

        // Transition to Healthy (valid transition from Deploying)
        let deployment = update_status(&pool, deployment.id, DeploymentStatus::Healthy)
            .await
            .unwrap();

        // Verify deploying_started_at remains unchanged across valid non-Deploying transitions
        assert_eq!(deployment.deploying_started_at, Some(first_timestamp));
    }

    #[cfg(feature = "backend")]
    #[sqlx::test]
    async fn first_healthy_at_set_once_on_healthy_transition(pool: PgPool) {
        use uuid::Uuid;

        let project_id = Uuid::new_v4();
        let user_id = Uuid::new_v4();

        sqlx::query!(
            "INSERT INTO users (id, email) VALUES ($1, $2)",
            user_id,
            "test@example.com"
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query!(
            "INSERT INTO projects (id, name, owner_user_id, access_class, status) VALUES ($1, $2, $3, $4, $5)",
            project_id,
            "test-project",
            user_id,
            "public",
            "Stopped"
        )
        .execute(&pool)
        .await
        .unwrap();

        let deployment = create(
            &pool,
            CreateDeploymentParams {
                deployment_id: "test-deploy",
                project_id,
                created_by_id: user_id,
                status: DeploymentStatus::Deploying,
                image: None,
                image_digest: None,
                rolled_back_from_deployment_id: None,
                deployment_group: "default",
                environment_id: None,
                expires_at: None,
                http_port: 8080,
                is_active: false,
                job_url: None,
                pull_request_url: None,
                git_repository_url: None,
                replicas: 1,
                cpu: "500m",
                memory: "256Mi",
                identity_audiences: serde_json::json!({}),
            },
        )
        .await
        .unwrap();

        assert!(deployment.first_healthy_at.is_none());

        let deployment = mark_healthy(&pool, deployment.id).await.unwrap();
        let first_healthy_at = deployment.first_healthy_at.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let deployment = mark_unhealthy(&pool, deployment.id, "temporary failure".to_string())
            .await
            .unwrap();
        assert_eq!(deployment.first_healthy_at, Some(first_healthy_at));

        let deployment = mark_healthy(&pool, deployment.id).await.unwrap();
        assert_eq!(deployment.first_healthy_at, Some(first_healthy_at));
    }
}
