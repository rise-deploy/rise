//! Deployment rows and the status transitions written against them.
//!
//! Every status writer here is a single statement in three parts: a `prev` CTE
//! that locks the target row and reads its current status, the guarded
//! `UPDATE`, and an `INSERT` into `deployment_events` captioning the move with
//! `from`/`to`. `prev` exists because the event has to name the status the
//! write moved *off*, and `FOR UPDATE` makes that read exact: a concurrent
//! writer blocks on the lock, so nothing can change between the read and the
//! update. `RETURNING OLD.status` would say the same thing in one clause, but
//! it requires PostgreSQL 18 and the supported floor is 16.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::db::models::{Deployment, DeploymentStatus, TerminationReason};
use crate::server::deployment::state_machine;
use rise_backend_core::SupersessionOutcome;

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
    /// Multi-container side-data (`Vec<ContainerSpec>` as JSONB) or `None` for legacy.
    /// Inserted atomically with the deployments row so we can't end up with a
    /// multi-container deployment whose container JSON failed to persist.
    pub containers: Option<&'a serde_json::Value>,
    /// Ingress route map (`Vec<RouteSpec>` as JSONB) or `None`. Always `None`
    /// when `containers` is `None`.
    pub routes: Option<&'a serde_json::Value>,
    /// Present when the environment's `max_deployment_expiration` capped
    /// `expires_at`. Recorded on the creation event so the timeline shows the
    /// cap alongside what was actually asked for.
    pub expiration_cap: Option<&'a rise_backend_core::expiration::ExpirationCap>,
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
            identity_audiences as "identity_audiences: serde_json::Value",
            containers as "containers: serde_json::Value",
            routes as "routes: serde_json::Value"
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
            identity_audiences as "identity_audiences: serde_json::Value",
            containers as "containers: serde_json::Value",
            routes as "routes: serde_json::Value"
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
            identity_audiences as "identity_audiences: serde_json::Value",
            containers as "containers: serde_json::Value",
            routes as "routes: serde_json::Value"
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
            identity_audiences as "identity_audiences: serde_json::Value",
            containers as "containers: serde_json::Value",
            routes as "routes: serde_json::Value"
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
            identity_audiences as "identity_audiences: serde_json::Value",
            containers as "containers: serde_json::Value",
            routes as "routes: serde_json::Value"
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
            identity_audiences as "identity_audiences: serde_json::Value",
            containers as "containers: serde_json::Value",
            routes as "routes: serde_json::Value"
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
        WITH ins AS (
            INSERT INTO deployments (deployment_id, project_id, created_by_id, status, image, image_digest, rolled_back_from_deployment_id, deployment_group, environment_id, expires_at, http_port, is_active, job_url, pull_request_url, git_repository_url, replicas, cpu, memory, identity_audiences, containers, routes)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21)
            RETURNING *
        ),
        ev AS (
            -- A deployment's first status is set by this INSERT, so there is
            -- no transition into it and nothing else would record that the
            -- deployment was accepted. Without this the timeline begins at
            -- the second thing that happened.
            INSERT INTO deployment_events (
                deployment_id, occurred_at, kind, severity, source, attributes
            )
            SELECT
                ins.id, ins.created_at, 'status_changed', 'info', 'control-plane',
                -- The transition always fits; the detail is only kept if it
                -- does. `attributes` carries a size CHECK, and this INSERT is
                -- part of the statement that creates the deployment, so an
                -- oversized detail block would fail the deployment itself.
                -- `image`, `job_url` and `pull_request_url` are caller-supplied
                -- and length-unbounded, and the container list grows with the
                -- project, so the budget is enforced here rather than assumed.
                -- `from` is omitted, not null: nothing preceded creation, and a
                -- stored null reads as a prior status we failed to record.
                jsonb_strip_nulls(jsonb_build_object('from', NULL, 'to', ins.status))
                    || CASE
                        WHEN pg_column_size(detail.attrs) <= 8192 THEN detail.attrs
                        ELSE jsonb_build_object('detail_omitted', true)
                    END
            FROM ins
            LEFT JOIN users u ON u.id = ins.created_by_id
            CROSS JOIN LATERAL (SELECT jsonb_strip_nulls(jsonb_build_object(
                    -- What was asked for, recorded where it is known: the row
                    -- can be edited later, the event says what was requested.
                    'created_by', u.email,
                    'group', ins.deployment_group,
                    -- `replicas`, `cpu` and `memory` are per container. The
                    -- columns on `deployments` are the single-container view of
                    -- them, so reporting them beside a list of container names
                    -- would attribute one container's size to all of them.
                    -- They are recorded here only when there is no per-container
                    -- side data to be more precise than.
                    'replicas', CASE WHEN NOT $22 THEN to_jsonb(ins.replicas) END,
                    'cpu', CASE WHEN NOT $22 THEN to_jsonb(ins.cpu) END,
                    'memory', CASE WHEN NOT $22 THEN to_jsonb(ins.memory) END,
                    'image', ins.image,
                    -- `containers` is a versioned side-data envelope
                    -- ({version, items}), not a bare array. The type guard is
                    -- not defensive dressing: jsonb_array_elements raises on a
                    -- non-array, and this runs inside the INSERT, so a shape
                    -- this does not expect would fail the deployment itself.
                    -- Recording nothing is the only acceptable failure mode for
                    -- an event describing a write.
                    -- Each container with its own size, rather than a bare
                    -- list of names beside one set of numbers.
                    'containers', CASE
                        WHEN $22 THEN (
                            SELECT jsonb_agg(jsonb_strip_nulls(jsonb_build_object(
                                'container', c->>'name',
                                'replicas', c->'replicas',
                                'cpu', c->>'cpu',
                                'memory', c->>'memory'
                            )))
                            FROM jsonb_array_elements(ins.containers->'items') c
                        )
                    END,
                    'rolled_back_from', ins.rolled_back_from_deployment_id,
                    'job_url', ins.job_url,
                    'pull_request_url', ins.pull_request_url,
                    -- The deployment's expiration, and — only when the
                    -- environment's max_deployment_expiration capped it —
                    -- what was actually requested and what capped it.
                    'expires_at', ins.expires_at,
                    'requested_expires_at', $23::timestamptz,
                    'max_deployment_expiration', $24::text,
                    'expiration_limited_by', $25::text
            )) AS attrs) detail
        )
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
            identity_audiences as "identity_audiences: serde_json::Value",
            containers as "containers: serde_json::Value",
            routes as "routes: serde_json::Value"
        FROM ins
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
        params.identity_audiences,
        params.containers,
        params.routes,
        // Whether per-container side data exists, decided once in Rust rather
        // than re-derived in three CASE arms.
        params
            .containers
            .and_then(|c| c.get("items"))
            .is_some_and(serde_json::Value::is_array),
        params.expiration_cap.and_then(|c| c.requested_expires_at),
        params
            .expiration_cap
            .map(|c| c.max_deployment_expiration.as_str()),
        params.expiration_cap.map(|c| c.environment.as_str()),
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
/// Move a deployment to `status`, recording the transition.
///
/// `attributes` is reporter-supplied detail about *this* transition, merged
/// into the event. `None` records the transition alone — every caller that does
/// not observe anything worth reporting, and every CLI too old to send it,
/// lands here and stays correct.
pub async fn update_status(
    pool: &PgPool,
    id: Uuid,
    status: DeploymentStatus,
    attributes: Option<&serde_json::Value>,
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
            identity_audiences as "identity_audiences: serde_json::Value",
            containers as "containers: serde_json::Value",
            routes as "routes: serde_json::Value"
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
        WITH prev AS (
            SELECT id, status FROM deployments WHERE id = $1 FOR UPDATE
        ),
        upd AS (
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
            FROM prev
            WHERE deployments.id = prev.id
              AND is_valid_transition(deployments.status, $2)
            RETURNING deployments.*, prev.status AS from_status
        ),
        ev AS (
            -- This writer owns the build path and the handoff into
            -- `Deploying`, so it is where a rollout's first event comes
            -- from. Severity is derived here rather than passed in: the
            -- target is a bind parameter, so the caller does not know it
            -- as a literal.
            INSERT INTO deployment_events (
                deployment_id, occurred_at, kind, severity, source, attributes
            )
            SELECT
                id, NOW(), 'status_changed',
                CASE
                    WHEN status = 'Failed' THEN 'error'
                    WHEN status = 'Unhealthy' THEN 'warning'
                    ELSE 'info'
                END,
                'control-plane',
                -- Reporter detail first, so `from`/`to` cannot be shadowed by
                -- a caller sending those keys: the transition is the one thing
                -- the log must be able to state on its own authority.
                -- Reporter detail is bounded here as well as at the edge: the
                -- edge counts JSON text bytes and the column's CHECK counts
                -- jsonb storage bytes, which are not the same size. This INSERT
                -- shares its statement with the status UPDATE, so an
                -- over-budget payload would roll the transition back rather
                -- than merely losing its detail.
                CASE
                    WHEN pg_column_size(COALESCE($3::jsonb, '{}'::jsonb)) <= 8192
                        THEN COALESCE($3::jsonb, '{}'::jsonb)
                    ELSE jsonb_build_object('detail_omitted', true)
                END || jsonb_strip_nulls(jsonb_build_object(
                    'from', from_status, 'to', status,
                    -- `error_message` is not cleared when a deployment recovers,
                    -- so it belongs to this transition only when this transition
                    -- is the failure. Otherwise a return to `Healthy` would be
                    -- captioned with the fault it just recovered from.
                    'reason', CASE WHEN status = 'Failed' THEN error_message END
                ))
            FROM upd
            WHERE from_status IS DISTINCT FROM status
        )
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
            identity_audiences as "identity_audiences: serde_json::Value",
            containers as "containers: serde_json::Value",
            routes as "routes: serde_json::Value"
        FROM upd
        "#,
        id,
        status_str,
        attributes
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

/// Mark deployment as failed.
///
/// Guarded by `is_valid_transition`: returns `None` (no-op) rather than
/// clobbering a status a routine caller must not overwrite — e.g. a
/// deployment a concurrent request already moved to `Terminating`.
pub async fn mark_failed(
    pool: &PgPool,
    id: Uuid,
    error_message: &str,
) -> Result<Option<Deployment>> {
    let deployment = sqlx::query_as!(
        Deployment,
        r#"
        WITH prev AS (
            SELECT id, status FROM deployments WHERE id = $1 FOR UPDATE
        ),
        upd AS (
            UPDATE deployments
            SET status = 'Failed', completed_at = COALESCE(completed_at, NOW()), error_message = $2
            FROM prev
            WHERE deployments.id = prev.id
              AND is_valid_transition(deployments.status, 'Failed')
            RETURNING deployments.*, prev.status AS from_status
        ),
        ev AS (
            -- Only a real move is an event: `is_valid_transition` admits
            -- `from = to` so a writer can refresh `updated_at` without the
            -- status having moved, and several of these run per tick.
            INSERT INTO deployment_events (
                deployment_id, occurred_at, kind, severity, source, attributes
            )
            SELECT
                id, NOW(), 'status_changed', 'error', 'control-plane',
                jsonb_build_object(
                    'from', from_status, 'to', status,
                    'reason', error_message
                )
            FROM upd
            WHERE from_status IS DISTINCT FROM status
        )
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
            identity_audiences as "identity_audiences: serde_json::Value",
            containers as "containers: serde_json::Value",
            routes as "routes: serde_json::Value"
        FROM upd
        "#,
        id,
        error_message
    )
    .fetch_optional(pool)
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
            identity_audiences as "identity_audiences: serde_json::Value",
            containers as "containers: serde_json::Value",
            routes as "routes: serde_json::Value"
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

/// Mark deployment as cancelled.
///
/// Guarded by `is_valid_transition`: returns `None` (no-op) if the
/// deployment is no longer in `Cancelling`.
#[cfg(feature = "backend")]
pub async fn mark_cancelled(pool: &PgPool, id: Uuid) -> Result<Option<Deployment>> {
    let deployment = sqlx::query_as!(
        Deployment,
        r#"
        WITH prev AS (
            SELECT id, status FROM deployments WHERE id = $1 FOR UPDATE
        ),
        upd AS (
            UPDATE deployments
            SET
                status = 'Cancelled',
                termination_reason = 'Cancelled',
                controller_metadata = '{}',
                completed_at = COALESCE(completed_at, NOW()),
                updated_at = NOW()
            FROM prev
            WHERE deployments.id = prev.id
              AND is_valid_transition(deployments.status, 'Cancelled')
            RETURNING deployments.*, prev.status AS from_status
        ),
        ev AS (
            -- Only a real move is an event: `is_valid_transition` admits
            -- `from = to` so a writer can refresh `updated_at` without the
            -- status having moved, and several of these run per tick.
            INSERT INTO deployment_events (
                deployment_id, occurred_at, kind, severity, source, attributes
            )
            SELECT
                id, NOW(), 'status_changed', 'info', 'control-plane',
                jsonb_build_object(
                    'from', from_status, 'to', status,
                    'reason', termination_reason::text
                )
            FROM upd
            WHERE from_status IS DISTINCT FROM status
        )
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
            identity_audiences as "identity_audiences: serde_json::Value",
            containers as "containers: serde_json::Value",
            routes as "routes: serde_json::Value"
        FROM upd
        "#,
        id
    )
    .fetch_optional(pool)
    .await
    .context("Failed to mark deployment as cancelled")?;

    Ok(deployment)
}

/// Mark deployment as stopped (user-initiated termination).
///
/// Guarded by `is_valid_transition`: returns `None` (no-op) if the
/// deployment is no longer in `Terminating`.
#[cfg(feature = "backend")]
pub async fn mark_stopped(pool: &PgPool, id: Uuid) -> Result<Option<Deployment>> {
    let deployment = sqlx::query_as!(
        Deployment,
        r#"
        WITH prev AS (
            SELECT id, status FROM deployments WHERE id = $1 FOR UPDATE
        ),
        upd AS (
            UPDATE deployments
            SET
                status = 'Stopped',
                termination_reason = 'UserStopped',
                controller_metadata = '{}',
                completed_at = COALESCE(completed_at, NOW()),
                updated_at = NOW()
            FROM prev
            WHERE deployments.id = prev.id
              AND is_valid_transition(deployments.status, 'Stopped')
            RETURNING deployments.*, prev.status AS from_status
        ),
        ev AS (
            -- Only a real move is an event: `is_valid_transition` admits
            -- `from = to` so a writer can refresh `updated_at` without the
            -- status having moved, and several of these run per tick.
            INSERT INTO deployment_events (
                deployment_id, occurred_at, kind, severity, source, attributes
            )
            SELECT
                id, NOW(), 'status_changed', 'info', 'control-plane',
                jsonb_build_object(
                    'from', from_status, 'to', status,
                    'reason', termination_reason::text
                )
            FROM upd
            WHERE from_status IS DISTINCT FROM status
        )
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
            identity_audiences as "identity_audiences: serde_json::Value",
            containers as "containers: serde_json::Value",
            routes as "routes: serde_json::Value"
        FROM upd
        "#,
        id
    )
    .fetch_optional(pool)
    .await
    .context("Failed to mark deployment as stopped")?;

    Ok(deployment)
}

/// Mark deployment as superseded (replaced by newer deployment).
///
/// Guarded by `is_valid_transition`: returns `None` (no-op) if the
/// deployment is no longer in `Terminating`.
#[cfg(feature = "backend")]
pub async fn mark_superseded(pool: &PgPool, id: Uuid) -> Result<Option<Deployment>> {
    let deployment = sqlx::query_as!(
        Deployment,
        r#"
        WITH prev AS (
            SELECT id, status FROM deployments WHERE id = $1 FOR UPDATE
        ),
        upd AS (
            UPDATE deployments
            SET
                status = 'Superseded',
                termination_reason = 'Superseded',
                controller_metadata = '{}',
                completed_at = COALESCE(completed_at, NOW()),
                updated_at = NOW()
            FROM prev
            WHERE deployments.id = prev.id
              AND is_valid_transition(deployments.status, 'Superseded')
            RETURNING deployments.*, prev.status AS from_status
        ),
        ev AS (
            -- Only a real move is an event: `is_valid_transition` admits
            -- `from = to` so a writer can refresh `updated_at` without the
            -- status having moved, and several of these run per tick.
            INSERT INTO deployment_events (
                deployment_id, occurred_at, kind, severity, source, attributes
            )
            SELECT
                upd.id, NOW(), 'status_changed', 'info', 'control-plane',
                jsonb_strip_nulls(jsonb_build_object(
                    'from', upd.from_status, 'to', upd.status,
                    'reason', upd.termination_reason::text,
                    -- Carried from the `Terminating` event, which is where the
                    -- replacement was in scope. `Superseded` is the row a reader
                    -- lands on, so it is the row that has to answer "by what?".
                    'superseded_by', (
                        SELECT e.attributes->'superseded_by'
                        FROM deployment_events e
                        WHERE e.deployment_id = upd.id
                          -- Only a transition can say what replaced this
                          -- deployment. A passed-through runtime event may
                          -- carry its own `to`, and must not be mistaken for one.
                          AND e.kind = 'status_changed'
                          AND e.attributes->>'to' = 'Terminating'
                        ORDER BY e.id DESC
                        LIMIT 1
                    )
                ))
            FROM upd
            WHERE upd.from_status IS DISTINCT FROM upd.status
        )
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
            identity_audiences as "identity_audiences: serde_json::Value",
            containers as "containers: serde_json::Value",
            routes as "routes: serde_json::Value"
        FROM upd
        "#,
        id
    )
    .fetch_optional(pool)
    .await
    .context("Failed to mark deployment as superseded")?;

    Ok(deployment)
}

/// Mark a deployment as expired (terminal state for deployments that timed out).
///
/// Guarded by `is_valid_transition`: returns `None` (no-op) if the
/// deployment is no longer in `Terminating`.
#[cfg(feature = "backend")]
pub async fn mark_expired(pool: &PgPool, id: Uuid) -> Result<Option<Deployment>> {
    let deployment = sqlx::query_as!(
        Deployment,
        r#"
        WITH prev AS (
            SELECT id, status FROM deployments WHERE id = $1 FOR UPDATE
        ),
        upd AS (
            UPDATE deployments
            SET
                status = 'Expired',
                termination_reason = 'Expired',
                controller_metadata = '{}',
                completed_at = COALESCE(completed_at, NOW()),
                updated_at = NOW()
            FROM prev
            WHERE deployments.id = prev.id
              AND is_valid_transition(deployments.status, 'Expired')
            RETURNING deployments.*, prev.status AS from_status
        ),
        ev AS (
            -- Only a real move is an event: `is_valid_transition` admits
            -- `from = to` so a writer can refresh `updated_at` without the
            -- status having moved, and several of these run per tick.
            INSERT INTO deployment_events (
                deployment_id, occurred_at, kind, severity, source, attributes
            )
            SELECT
                id, NOW(), 'status_changed', 'info', 'control-plane',
                jsonb_build_object(
                    'from', from_status, 'to', status,
                    'reason', termination_reason::text
                )
            FROM upd
            WHERE from_status IS DISTINCT FROM status
        )
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
            identity_audiences as "identity_audiences: serde_json::Value",
            containers as "containers: serde_json::Value",
            routes as "routes: serde_json::Value"
        FROM upd
        "#,
        id
    )
    .fetch_optional(pool)
    .await
    .context("Failed to mark deployment as expired")?;

    Ok(deployment)
}

/// Mark deployment as healthy.
///
/// Guarded by `is_valid_transition`: returns `None` (no-op) rather than
/// reviving a deployment a concurrent request already moved to a status a
/// routine health-check pass must never overwrite (e.g. `Terminating` from
/// a user's stop request).
#[cfg(feature = "backend")]
pub async fn mark_healthy(pool: &PgPool, id: Uuid) -> Result<Option<Deployment>> {
    let deployment = sqlx::query_as!(
        Deployment,
        r#"
        WITH prev AS (
            SELECT id, status FROM deployments WHERE id = $1 FOR UPDATE
        ),
        upd AS (
            UPDATE deployments
            SET
                status = 'Healthy',
                error_message = NULL,
                first_healthy_at = COALESCE(first_healthy_at, NOW()),
                updated_at = NOW()
            FROM prev
            WHERE deployments.id = prev.id
              AND is_valid_transition(deployments.status, 'Healthy')
            RETURNING deployments.*, prev.status AS from_status
        ),
        ev AS (
            -- Only a real move is an event. `is_valid_transition` admits
            -- `from = to` so a routine health check can refresh `updated_at`,
            -- and this writer runs on every reconcile tick while healthy —
            -- without the filter that is one row per tick per deployment.
            INSERT INTO deployment_events (
                deployment_id, occurred_at, kind, severity, source, attributes
            )
            SELECT
                id, NOW(), 'status_changed', 'info', 'control-plane',
                jsonb_build_object('from', from_status, 'to', status)
            FROM upd
            WHERE from_status IS DISTINCT FROM status
        )
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
            identity_audiences as "identity_audiences: serde_json::Value",
            containers as "containers: serde_json::Value",
            routes as "routes: serde_json::Value"
        FROM upd
        "#,
        id
    )
    .fetch_optional(pool)
    .await
    .context("Failed to mark deployment as healthy")?;

    Ok(deployment)
}

/// Mark deployment as unhealthy with reason.
///
/// Guarded by `is_valid_transition`: returns `None` (no-op) rather than
/// overwriting a status a routine health-check pass must never touch.
#[cfg(feature = "backend")]
pub async fn mark_unhealthy(pool: &PgPool, id: Uuid, reason: String) -> Result<Option<Deployment>> {
    let deployment = sqlx::query_as!(
        Deployment,
        r#"
        WITH prev AS (
            SELECT id, status FROM deployments WHERE id = $1 FOR UPDATE
        ),
        upd AS (
            UPDATE deployments
            SET
                status = 'Unhealthy',
                error_message = $2,
                updated_at = NOW()
            FROM prev
            WHERE deployments.id = prev.id
              AND is_valid_transition(deployments.status, 'Unhealthy')
            RETURNING deployments.*, prev.status AS from_status
        ),
        ev AS (
            -- Only a real move is an event: `is_valid_transition` admits
            -- `from = to` so a writer can refresh `updated_at` without the
            -- status having moved, and several of these run per tick.
            INSERT INTO deployment_events (
                deployment_id, occurred_at, kind, severity, source, attributes
            )
            SELECT
                id, NOW(), 'status_changed', 'warning', 'control-plane',
                jsonb_build_object(
                    'from', from_status, 'to', status,
                    'reason', error_message
                )
            FROM upd
            WHERE from_status IS DISTINCT FROM status
        )
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
            identity_audiences as "identity_audiences: serde_json::Value",
            containers as "containers: serde_json::Value",
            routes as "routes: serde_json::Value"
        FROM upd
        "#,
        id,
        reason
    )
    .fetch_optional(pool)
    .await
    .context("Failed to mark deployment as unhealthy")?;

    Ok(deployment)
}

/// Mark deployment as terminating with reason.
///
/// Guarded by `is_valid_transition`: returns `None` (no-op) if the
/// deployment is no longer in `Healthy`/`Unhealthy` (e.g. it already
/// finished terminating, or moved there itself).
/// `superseded_by` is the `deployment_id` of the deployment taking this one's
/// place, and is only meaningful with `TerminationReason::Superseded`. It is
/// recorded here because here is where it is known: by the time termination
/// completes, the caller has only the reason, not the replacement.
pub async fn mark_terminating(
    pool: &PgPool,
    id: Uuid,
    reason: TerminationReason,
    superseded_by: Option<&str>,
) -> Result<Option<Deployment>> {
    let deployment = sqlx::query_as!(
        Deployment,
        r#"
        WITH prev AS (
            SELECT id, status FROM deployments WHERE id = $1 FOR UPDATE
        ),
        upd AS (
            UPDATE deployments
            SET
                status = 'Terminating',
                termination_reason = $2,
                updated_at = NOW()
            FROM prev
            WHERE deployments.id = prev.id
              AND is_valid_transition(deployments.status, 'Terminating')
            RETURNING deployments.*, prev.status AS from_status
        ),
        ev AS (
            -- Only a real move is an event: `is_valid_transition` admits
            -- `from = to` so a writer can refresh `updated_at` without the
            -- status having moved, and several of these run per tick.
            INSERT INTO deployment_events (
                deployment_id, occurred_at, kind, severity, source, attributes
            )
            SELECT
                id, NOW(), 'status_changed', 'info', 'control-plane',
                jsonb_strip_nulls(jsonb_build_object(
                    'from', from_status, 'to', status,
                    'reason', termination_reason::text,
                    'superseded_by', CASE
                        WHEN $3::text IS NOT NULL
                        THEN jsonb_build_object('kind', 'deployment', 'name', $3::text)
                    END
                ))
            FROM upd
            WHERE from_status IS DISTINCT FROM status
        )
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
            identity_audiences as "identity_audiences: serde_json::Value",
            containers as "containers: serde_json::Value",
            routes as "routes: serde_json::Value"
        FROM upd
        "#,
        id,
        reason as TerminationReason,
        superseded_by
    )
    .fetch_optional(pool)
    .await
    .context("Failed to mark deployment as terminating")?;

    Ok(deployment)
}

/// Mark deployment as cancelling.
///
/// Guarded by `is_valid_transition`: returns `None` (no-op) if the
/// deployment is no longer in a pre-infrastructure state.
pub async fn mark_cancelling(pool: &PgPool, id: Uuid) -> Result<Option<Deployment>> {
    let deployment = sqlx::query_as!(
        Deployment,
        r#"
        WITH prev AS (
            SELECT id, status FROM deployments WHERE id = $1 FOR UPDATE
        ),
        upd AS (
            UPDATE deployments
            SET
                status = 'Cancelling',
                termination_reason = 'Cancelled',
                updated_at = NOW()
            FROM prev
            WHERE deployments.id = prev.id
              AND is_valid_transition(deployments.status, 'Cancelling')
            RETURNING deployments.*, prev.status AS from_status
        ),
        ev AS (
            -- Only a real move is an event: `is_valid_transition` admits
            -- `from = to` so a writer can refresh `updated_at` without the
            -- status having moved, and several of these run per tick.
            INSERT INTO deployment_events (
                deployment_id, occurred_at, kind, severity, source, attributes
            )
            SELECT
                id, NOW(), 'status_changed', 'info', 'control-plane',
                jsonb_build_object(
                    'from', from_status, 'to', status,
                    'reason', termination_reason::text
                )
            FROM upd
            WHERE from_status IS DISTINCT FROM status
        )
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
            identity_audiences as "identity_audiences: serde_json::Value",
            containers as "containers: serde_json::Value",
            routes as "routes: serde_json::Value"
        FROM upd
        "#,
        id
    )
    .fetch_optional(pool)
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
            identity_audiences as "identity_audiences: serde_json::Value",
            containers as "containers: serde_json::Value",
            routes as "routes: serde_json::Value"
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
            identity_audiences as "identity_audiences: serde_json::Value",
            containers as "containers: serde_json::Value",
            routes as "routes: serde_json::Value"
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
            identity_audiences as "identity_audiences: serde_json::Value",
            containers as "containers: serde_json::Value",
            routes as "routes: serde_json::Value"
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
            identity_audiences as "identity_audiences: serde_json::Value",
            containers as "containers: serde_json::Value",
            routes as "routes: serde_json::Value"
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
                identity_audiences as "identity_audiences: serde_json::Value",
                containers as "containers: serde_json::Value",
                routes as "routes: serde_json::Value"
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
                identity_audiences as "identity_audiences: serde_json::Value",
                containers as "containers: serde_json::Value",
                routes as "routes: serde_json::Value"
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
    let updated = sqlx::query!(
        "UPDATE deployments
         SET is_active = TRUE, updated_at = NOW()
         WHERE id = $1
           AND project_id = $2
           AND deployment_group = $3
           AND NOT is_terminal(status)",
        deployment_id,
        project_id,
        deployment_group,
    )
    .execute(&mut *tx)
    .await?;

    if updated.rows_affected() != 1 {
        anyhow::bail!(
            "deployment {deployment_id} is not a non-terminal member of project {project_id} group '{deployment_group}'"
        );
    }

    tx.commit().await?;

    Ok(())
}

/// Atomically mark a deployment healthy and, if some other deployment is
/// currently the group's active (most-recently-created `Healthy`)
/// deployment, mark that one `Terminating(Superseded)` in the same
/// transaction. See `rise_backend_core::store::SupersessionOutcome` for the
/// `became_healthy: false` contract when the guard rejects the write.
#[cfg(feature = "backend")]
pub async fn mark_healthy_and_supersede(
    pool: &PgPool,
    deployment_id: Uuid,
    project_id: Uuid,
    deployment_group: &str,
) -> Result<SupersessionOutcome> {
    let mut tx = pool.begin().await?;

    let healthy = sqlx::query_as!(
        Deployment,
        r#"
        WITH prev AS (
            SELECT id, status FROM deployments WHERE id = $1 FOR UPDATE
        ),
        upd AS (
            UPDATE deployments
            SET
                status = 'Healthy',
                error_message = NULL,
                first_healthy_at = COALESCE(first_healthy_at, NOW()),
                updated_at = NOW()
            FROM prev
            WHERE deployments.id = prev.id
              AND is_valid_transition(deployments.status, 'Healthy')
            RETURNING deployments.*, prev.status AS from_status
        ),
        ev AS (
            INSERT INTO deployment_events (
                deployment_id, occurred_at, kind, severity, source, attributes
            )
            SELECT
                id, NOW(), 'status_changed', 'info', 'control-plane',
                jsonb_build_object('from', from_status, 'to', status)
            FROM upd
            WHERE from_status IS DISTINCT FROM status
        )
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
            identity_audiences as "identity_audiences: serde_json::Value",
            containers as "containers: serde_json::Value",
            routes as "routes: serde_json::Value"
        FROM upd
        "#,
        deployment_id
    )
    .fetch_optional(&mut *tx)
    .await
    .context("Failed to mark deployment as healthy")?;

    if healthy.is_none() {
        // Nothing to roll back — the guard rejected before any write ran.
        // Explicit rollback documents intent to a future reader/refactor
        // rather than relying on `tx`'s drop behavior.
        tx.rollback().await?;
        return Ok(SupersessionOutcome {
            became_healthy: false,
            superseded: None,
        });
    }

    // Same selection as the old `find_active_for_project_and_group`: the
    // single most-recently-created other `Healthy` deployment in the group.
    // Scoped to `status = 'Healthy'` (not `is_active(status)`) to preserve
    // today's exact selection semantics — an old-active deployment currently
    // sitting in `Unhealthy` is caught by `handle_deployment_became_healthy`'s
    // straggler self-healing loop instead, same as before this change.
    let superseded = sqlx::query_as!(
        Deployment,
        r#"
        WITH prev AS (
            SELECT id, status FROM deployments
            WHERE project_id = $2
              AND deployment_group = $3
              AND id != $1
              AND status = 'Healthy'
            ORDER BY created_at DESC
            LIMIT 1
            FOR UPDATE
        ),
        upd AS (
            UPDATE deployments
            SET
                status = 'Terminating',
                termination_reason = 'Superseded',
                updated_at = NOW()
            FROM prev
            WHERE deployments.id = prev.id
              -- `prev` holds the row lock, so nothing moves this deployment
              -- between selecting it and updating it. The guard still stands
              -- as the authoritative check on what the row may become.
              AND is_valid_transition(deployments.status, 'Terminating')
            RETURNING deployments.*, prev.status AS from_status
        ),
        ev AS (
            -- `from` comes from the locked row `prev` read, so it is the
            -- status this write moved off.
            INSERT INTO deployment_events (
                deployment_id, occurred_at, kind, severity, source, attributes
            )
            SELECT
                id, NOW(), 'status_changed', 'info', 'control-plane',
                jsonb_build_object(
                    'from', from_status, 'to', status,
                    'reason', termination_reason::text,
                    -- $1 is the deployment taking this one's place. Named by
                    -- its `deployment_id`, not its UUID: it is what a reader
                    -- follows, and what the URL is built from.
                    'superseded_by', jsonb_build_object(
                        'kind', 'deployment',
                        'name', (SELECT deployment_id FROM deployments WHERE id = $1)
                    )
                )
            FROM upd
        )
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
            identity_audiences as "identity_audiences: serde_json::Value",
            containers as "containers: serde_json::Value",
            routes as "routes: serde_json::Value"
        FROM upd
        "#,
        deployment_id,
        project_id,
        deployment_group
    )
    .fetch_optional(&mut *tx)
    .await
    .context("Failed to supersede previous active deployment")?;

    tx.commit().await?;

    Ok(SupersessionOutcome {
        became_healthy: true,
        superseded,
    })
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
            identity_audiences as "identity_audiences: serde_json::Value",
            containers as "containers: serde_json::Value",
            routes as "routes: serde_json::Value"
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

    /// Insert the minimum rows a deployment needs, returning its id.
    async fn seed_deployment_for_events(pool: &PgPool, status: &str) -> Uuid {
        let user: Uuid = sqlx::query_scalar(
            "INSERT INTO users (email) VALUES ('events@test.local') RETURNING id",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        let project: Uuid = sqlx::query_scalar(
            "INSERT INTO projects (name, status, access_class, owner_user_id)
             VALUES ('events-test', 'Stopped', 'public', $1) RETURNING id",
        )
        .bind(user)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query_scalar(
            "INSERT INTO deployments (deployment_id, project_id, created_by_id, status)
             VALUES ('20260830-000001', $1, $2, $3) RETURNING id",
        )
        .bind(project)
        .bind(user)
        .bind(status)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// Creation is the first thing that happened to a deployment, so it is the
    /// first row in its log. Recording it as a transition out of nothing means a
    /// reader walks one uniform sequence rather than special-casing the origin.
    #[sqlx::test]
    async fn creating_a_deployment_opens_its_log(pool: PgPool) {
        let (project_id, user_id) = seed_project_and_user(&pool).await;
        // The real stored shape: a versioned side-data envelope, not a bare
        // array. `encode_side_data` produces this.
        let containers = serde_json::json!({
            "version": 1,
            "items": [
                { "name": "web", "replicas": 2, "cpu": "500m", "memory": "256Mi" },
                { "name": "worker", "replicas": 1, "cpu": "250m", "memory": "128Mi" },
            ],
        });

        let created = create(
            &pool,
            CreateDeploymentParams {
                deployment_id: "20260830-000009",
                project_id,
                created_by_id: user_id,
                status: DeploymentStatus::Pending,
                image: Some("registry.test/app:v1"),
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
                replicas: 2,
                cpu: "500m",
                memory: "256Mi",
                identity_audiences: serde_json::json!({}),
                containers: Some(&containers),
                routes: None,
                expiration_cap: None,
            },
        )
        .await
        .unwrap();

        let opened: Vec<(Option<String>, String)> = sqlx::query_as(
            "SELECT attributes->>'from', attributes->>'to'
             FROM deployment_events
             WHERE deployment_id = $1 AND kind = 'status_changed'
             ORDER BY id",
        )
        .bind(created.id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            opened,
            vec![(None, "Pending".to_string())],
            "the log opens with the status the deployment was created in, \
             out of no prior status",
        );

        let attributes: serde_json::Value =
            sqlx::query_scalar("SELECT attributes FROM deployment_events WHERE deployment_id = $1")
                .bind(created.id)
                .fetch_one(&pool)
                .await
                .unwrap();

        // What was *requested*, captured where it is still known. The row can be
        // edited afterwards; the event keeps the ask.
        assert_eq!(attributes["to"], "Pending");
        assert_eq!(attributes["created_by"], format!("{user_id}@example.com"));
        assert_eq!(attributes["group"], "default");
        assert_eq!(attributes["image"], "registry.test/app:v1");

        // Size belongs to each container, not to the deployment: one set of
        // numbers beside two container names would attribute one container's
        // size to both.
        assert_eq!(
            attributes["containers"],
            serde_json::json!([
                { "container": "web", "replicas": 2, "cpu": "500m", "memory": "256Mi" },
                { "container": "worker", "replicas": 1, "cpu": "250m", "memory": "128Mi" },
            ]),
        );
        for absent in ["replicas", "cpu", "memory"] {
            assert!(
                attributes.get(absent).is_none(),
                "{absent} is per-container here, so the row's value must not stand in for it",
            );
        }
        assert!(
            attributes.get("from").is_none(),
            "nothing preceded creation, and jsonb_strip_nulls drops the key rather \
             than storing a null that reads as a real prior status",
        );
        assert!(
            attributes.get("job_url").is_none(),
            "absent optionals are omitted, not stored as null",
        );
    }

    #[sqlx::test]
    async fn creation_event_records_expiration_cap_only_when_applied(pool: PgPool) {
        let (project_id, user_id) = seed_project_and_user(&pool).await;
        let requested_expires_at = chrono::Utc::now() + chrono::Duration::days(30);
        let capped_expires_at = chrono::Utc::now() + chrono::Duration::days(7);
        let cap = rise_backend_core::expiration::ExpirationCap {
            requested_expires_at: Some(requested_expires_at),
            max_deployment_expiration: "7d".to_string(),
            environment: "staging".to_string(),
        };

        let capped = create(
            &pool,
            CreateDeploymentParams {
                deployment_id: "20260830-000010",
                project_id,
                created_by_id: user_id,
                status: DeploymentStatus::Pending,
                image: Some("registry.test/app:v1"),
                image_digest: None,
                rolled_back_from_deployment_id: None,
                deployment_group: "mr/123",
                environment_id: None,
                expires_at: Some(capped_expires_at),
                http_port: 8080,
                is_active: false,
                job_url: None,
                pull_request_url: None,
                git_repository_url: None,
                replicas: 1,
                cpu: "500m",
                memory: "256Mi",
                identity_audiences: serde_json::json!({}),
                containers: None,
                routes: None,
                expiration_cap: Some(&cap),
            },
        )
        .await
        .unwrap();

        let capped_attrs: serde_json::Value =
            sqlx::query_scalar("SELECT attributes FROM deployment_events WHERE deployment_id = $1")
                .bind(capped.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_close(&capped_attrs["expires_at"], capped_expires_at);
        assert_close(&capped_attrs["requested_expires_at"], requested_expires_at);
        assert_eq!(capped_attrs["max_deployment_expiration"], "7d");
        assert_eq!(capped_attrs["expiration_limited_by"], "staging");

        let uncapped = create(
            &pool,
            CreateDeploymentParams {
                deployment_id: "20260830-000011",
                project_id,
                created_by_id: user_id,
                status: DeploymentStatus::Pending,
                image: Some("registry.test/app:v1"),
                image_digest: None,
                rolled_back_from_deployment_id: None,
                deployment_group: "default",
                environment_id: None,
                expires_at: Some(requested_expires_at),
                http_port: 8080,
                is_active: false,
                job_url: None,
                pull_request_url: None,
                git_repository_url: None,
                replicas: 1,
                cpu: "500m",
                memory: "256Mi",
                identity_audiences: serde_json::json!({}),
                containers: None,
                routes: None,
                expiration_cap: None,
            },
        )
        .await
        .unwrap();

        let uncapped_attrs: serde_json::Value =
            sqlx::query_scalar("SELECT attributes FROM deployment_events WHERE deployment_id = $1")
                .bind(uncapped.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_close(&uncapped_attrs["expires_at"], requested_expires_at);
        for absent in [
            "requested_expires_at",
            "max_deployment_expiration",
            "expiration_limited_by",
        ] {
            assert!(
                uncapped_attrs.get(absent).is_none(),
                "{absent} is only recorded when the environment's max_deployment_expiration \
                 actually capped expires_at",
            );
        }
    }

    /// The creation event describes the write it rides along with, so nothing
    /// about assembling it may be able to fail that write. A `containers` value
    /// this does not recognise costs the event one attribute, not the
    /// deployment.
    #[sqlx::test]
    async fn an_unreadable_container_shape_still_creates_the_deployment(pool: PgPool) {
        let (project_id, user_id) = seed_project_and_user(&pool).await;

        for (label, containers) in [
            (
                "an envelope with no items",
                serde_json::json!({"version": 1}),
            ),
            ("a bare array", serde_json::json!([{"name": "web"}])),
            ("a scalar", serde_json::json!("web")),
        ] {
            let deployment_id = format!("20260830-0000{}", containers.to_string().len());
            let created = create(
                &pool,
                CreateDeploymentParams {
                    deployment_id: &deployment_id,
                    project_id,
                    created_by_id: user_id,
                    status: DeploymentStatus::Pending,
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
                    containers: Some(&containers),
                    routes: None,
                    expiration_cap: None,
                },
            )
            .await
            .unwrap_or_else(|e| panic!("{label} must not fail deployment creation: {e:?}"));

            let attributes: serde_json::Value = sqlx::query_scalar(
                "SELECT attributes FROM deployment_events WHERE deployment_id = $1",
            )
            .bind(created.id)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(attributes["to"], "Pending", "{label} still opens the log");
            assert!(
                attributes.get("containers").is_none(),
                "{label} contributes no container list",
            );
        }
    }

    /// Reported detail rides along with the transition it describes.
    #[sqlx::test]
    async fn reported_attributes_are_merged_into_the_transition(pool: PgPool) {
        let id = seed_deployment_for_events(&pool, "Pushing").await;
        let reported = serde_json::json!({
            "registry": "registry.test",
            "images": [{ "container": "web", "build_ms": 8123, "push_ms": 1400 }],
        });

        update_status(&pool, id, DeploymentStatus::Pushed, Some(&reported))
            .await
            .unwrap();

        let attributes: serde_json::Value =
            sqlx::query_scalar("SELECT attributes FROM deployment_events WHERE deployment_id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();

        assert_eq!(attributes["from"], "Pushing");
        assert_eq!(attributes["to"], "Pushed");
        assert_eq!(attributes["registry"], "registry.test");
        assert_eq!(attributes["images"][0]["container"], "web");
        assert_eq!(attributes["images"][0]["build_ms"], 8123);
    }

    /// A reporter cannot rewrite what actually happened. The transition is the
    /// one claim the log makes on its own authority.
    #[sqlx::test]
    async fn reported_attributes_cannot_overwrite_the_transition(pool: PgPool) {
        let id = seed_deployment_for_events(&pool, "Pushing").await;
        let lying = serde_json::json!({ "from": "Healthy", "to": "Healthy" });

        update_status(&pool, id, DeploymentStatus::Pushed, Some(&lying))
            .await
            .unwrap();

        assert_eq!(
            status_events(&pool, id).await,
            vec![("Pushing".to_string(), "Pushed".to_string())],
        );
    }

    /// A CLI too old to report anything still records a correct transition.
    #[sqlx::test]
    async fn a_transition_without_reported_detail_is_still_recorded(pool: PgPool) {
        let id = seed_deployment_for_events(&pool, "Pushing").await;

        update_status(&pool, id, DeploymentStatus::Pushed, None)
            .await
            .unwrap();

        let attributes: serde_json::Value =
            sqlx::query_scalar("SELECT attributes FROM deployment_events WHERE deployment_id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();

        assert_eq!(attributes["to"], "Pushed");
        assert!(
            attributes.get("images").is_none(),
            "nothing is invented when nothing was reported",
        );
    }

    /// The replacement is in scope when a deployment is marked `Terminating`
    /// and gone by the time termination completes — but `Superseded` is the row
    /// a reader lands on, so the answer has to survive the trip.
    #[sqlx::test]
    async fn superseded_events_name_the_deployment_that_replaced_them(pool: PgPool) {
        let id = seed_deployment_for_events(&pool, "Healthy").await;

        mark_terminating(
            &pool,
            id,
            TerminationReason::Superseded,
            Some("20260830-999999"),
        )
        .await
        .unwrap()
        .expect("a Healthy deployment can start terminating");
        mark_superseded(&pool, id)
            .await
            .unwrap()
            .expect("and finish");

        // The successor is a self-describing reference, so a reader can link it
        // without knowing which key names a deployment.
        let named: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT attributes->>'to',
                    attributes->'superseded_by'->>'kind',
                    attributes->'superseded_by'->>'name'
             FROM deployment_events
             WHERE deployment_id = $1 AND kind = 'status_changed'
             ORDER BY id",
        )
        .bind(id)
        .fetch_all(&pool)
        .await
        .unwrap();

        let successor = || {
            (
                Some("deployment".to_string()),
                Some("20260830-999999".to_string()),
            )
        };
        assert_eq!(
            named,
            vec![
                ("Terminating".to_string(), successor().0, successor().1),
                ("Superseded".to_string(), successor().0, successor().1),
            ],
        );
    }

    /// Terminating for any other reason names nobody, rather than storing a
    /// null that reads as "superseded by something we lost".
    #[sqlx::test]
    async fn other_terminations_name_no_successor(pool: PgPool) {
        let id = seed_deployment_for_events(&pool, "Healthy").await;

        mark_terminating(&pool, id, TerminationReason::UserStopped, None)
            .await
            .unwrap()
            .unwrap();
        mark_stopped(&pool, id).await.unwrap().unwrap();

        let keys: Vec<String> = sqlx::query_scalar(
            "SELECT jsonb_object_keys(attributes) FROM deployment_events WHERE deployment_id = $1",
        )
        .bind(id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(
            !keys.contains(&"superseded_by".to_string()),
            "no successor key at all, not a null one: {keys:?}",
        );
    }

    async fn status_events(pool: &PgPool, id: Uuid) -> Vec<(String, String)> {
        sqlx::query_as::<_, (String, String)>(
            "SELECT attributes->>'from', attributes->>'to'
             FROM deployment_events
             WHERE deployment_id = $1 AND kind = 'status_changed'
             ORDER BY id",
        )
        .bind(id)
        .fetch_all(pool)
        .await
        .unwrap()
    }

    /// A real transition records one event carrying both ends of it.
    #[sqlx::test]
    async fn marking_healthy_records_the_transition_it_performed(pool: PgPool) {
        let id = seed_deployment_for_events(&pool, "Deploying").await;

        let updated = mark_healthy(&pool, id).await.unwrap();
        assert!(
            updated.is_some(),
            "the row callers depend on is still returned"
        );
        assert_eq!(updated.unwrap().status, DeploymentStatus::Healthy);

        assert_eq!(
            status_events(&pool, id).await,
            vec![("Deploying".to_string(), "Healthy".to_string())],
        );
    }

    /// `is_valid_transition` admits `from = to` so a routine health check can
    /// refresh `updated_at`, and this writer runs on every reconcile tick while
    /// healthy. Without the filter that is one row per tick per deployment, so
    /// the self-transition must return the row and record nothing.
    #[sqlx::test]
    async fn marking_healthy_again_returns_the_row_but_records_nothing(pool: PgPool) {
        let id = seed_deployment_for_events(&pool, "Deploying").await;

        mark_healthy(&pool, id).await.unwrap();
        for _ in 0..5 {
            let repeat = mark_healthy(&pool, id).await.unwrap();
            assert!(repeat.is_some(), "a self-transition still updates the row");
        }

        assert_eq!(
            status_events(&pool, id).await.len(),
            1,
            "only the real transition is an event",
        );
    }

    /// A deployment that flaps records both moves: they are two genuine
    /// occurrences, which is why status events carry no dedupe key.
    #[sqlx::test]
    async fn flapping_records_every_real_move(pool: PgPool) {
        let id = seed_deployment_for_events(&pool, "Deploying").await;

        mark_healthy(&pool, id).await.unwrap();
        mark_unhealthy(&pool, id, "probe failing".to_string())
            .await
            .unwrap();
        mark_healthy(&pool, id).await.unwrap();

        assert_eq!(
            status_events(&pool, id).await,
            vec![
                ("Deploying".to_string(), "Healthy".to_string()),
                ("Healthy".to_string(), "Unhealthy".to_string()),
                ("Unhealthy".to_string(), "Healthy".to_string()),
            ],
            "every real move is recorded; a snapshot could show only the last",
        );
    }

    /// The whole build path runs through `update_status`, so a rollout's first
    /// events come from a different writer than the `mark_*` family.
    #[sqlx::test]
    async fn the_build_path_records_each_phase(pool: PgPool) {
        let id = seed_deployment_for_events(&pool, "Pending").await;

        for status in [
            DeploymentStatus::Building,
            DeploymentStatus::Pushing,
            DeploymentStatus::Pushed,
            DeploymentStatus::Deploying,
        ] {
            update_status(&pool, id, status, None).await.unwrap();
        }

        assert_eq!(
            status_events(&pool, id).await,
            vec![
                ("Pending".to_string(), "Building".to_string()),
                ("Building".to_string(), "Pushing".to_string()),
                ("Pushing".to_string(), "Pushed".to_string()),
                ("Pushed".to_string(), "Deploying".to_string()),
            ],
        );
    }

    /// Severity is a property of the occurrence: the same writer family
    /// produces `error` for a failure and `info` for a routine stop.
    #[sqlx::test]
    async fn severity_reflects_what_happened(pool: PgPool) {
        let failed = seed_deployment_for_events(&pool, "Deploying").await;
        mark_failed(&pool, failed, "image pull backoff")
            .await
            .unwrap();

        let row: (String, Option<String>) = sqlx::query_as(
            "SELECT severity, attributes->>'reason' FROM deployment_events
             WHERE deployment_id = $1 AND kind = 'status_changed'",
        )
        .bind(failed)
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(row.0, "error");
        assert_eq!(row.1.as_deref(), Some("image pull backoff"));
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

    /// Test that PostgreSQL is_valid_transition() agrees with Rust
    /// state_machine::is_valid_transition() for every status pair — the
    /// guard every `mark_*` write below relies on, so a drift between the
    /// two representations must fail the build, not surface as a silently
    /// wrong no-op in production.
    #[sqlx::test]
    async fn db_is_valid_transition_matches_rust_is_valid_transition(pool: PgPool) {
        let statuses = [
            "Pending",
            "Building",
            "Pushing",
            "Pushed",
            "Deploying",
            "Healthy",
            "Unhealthy",
            "Cancelling",
            "Cancelled",
            "Terminating",
            "Stopped",
            "Superseded",
            "Failed",
            "Expired",
        ];

        for from_str in statuses {
            for to_str in statuses {
                let result: bool = sqlx::query_scalar("SELECT is_valid_transition($1, $2)")
                    .bind(from_str)
                    .bind(to_str)
                    .fetch_one(&pool)
                    .await
                    .unwrap();

                let expected = state_machine::is_valid_transition(
                    &str_to_status(from_str),
                    &str_to_status(to_str),
                );

                assert_eq!(
                    result, expected,
                    "is_valid_transition({from_str}, {to_str}) returned {result} but Rust expected {expected}"
                );
            }
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
                containers: None,
                routes: None,
                expiration_cap: None,
            },
        )
        .await
        .unwrap();

        // Verify deploying_started_at is NULL initially
        assert!(deployment.deploying_started_at.is_none());

        // Transition to Deploying
        let deployment = update_status(&pool, deployment.id, DeploymentStatus::Deploying, None)
            .await
            .unwrap();

        // Verify deploying_started_at is now set
        assert!(deployment.deploying_started_at.is_some());
        let first_timestamp = deployment.deploying_started_at.unwrap();

        // Wait a bit to ensure time has passed
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Transition to Deploying again (same-state transition is valid and should not overwrite)
        let deployment = update_status(&pool, deployment.id, DeploymentStatus::Deploying, None)
            .await
            .unwrap();

        // Verify deploying_started_at is unchanged
        assert_eq!(deployment.deploying_started_at, Some(first_timestamp));

        // Transition to Healthy (valid transition from Deploying)
        let deployment = update_status(&pool, deployment.id, DeploymentStatus::Healthy, None)
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
                containers: None,
                routes: None,
                expiration_cap: None,
            },
        )
        .await
        .unwrap();

        assert!(deployment.first_healthy_at.is_none());

        let deployment = mark_healthy(&pool, deployment.id).await.unwrap().unwrap();
        let first_healthy_at = deployment.first_healthy_at.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let deployment = mark_unhealthy(&pool, deployment.id, "temporary failure".to_string())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(deployment.first_healthy_at, Some(first_healthy_at));

        let deployment = mark_healthy(&pool, deployment.id).await.unwrap().unwrap();
        assert_eq!(deployment.first_healthy_at, Some(first_healthy_at));
    }

    /// Guarded `mark_healthy`/`mark_unhealthy` must no-op (not clobber) a
    /// deployment that already moved to a protected status — the concrete
    /// race this guard closes: a user's stop request racing a reconciler's
    /// routine health-check pass.
    #[sqlx::test]
    async fn mark_healthy_is_a_noop_on_a_protected_deployment(pool: PgPool) {
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
            "private",
            "Running"
        )
        .execute(&pool)
        .await
        .unwrap();

        let deployment = create(
            &pool,
            CreateDeploymentParams {
                deployment_id: "20260101-000000",
                project_id,
                created_by_id: user_id,
                status: DeploymentStatus::Healthy,
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
                containers: None,
                routes: None,
                expiration_cap: None,
            },
        )
        .await
        .unwrap();

        // A concurrent stop request moves it to Terminating.
        mark_terminating(&pool, deployment.id, TerminationReason::UserStopped, None)
            .await
            .unwrap()
            .unwrap();

        // A stale reconciler tick still believes it's about to become
        // healthy — this must be a no-op, not a resurrection.
        let result = mark_healthy(&pool, deployment.id).await.unwrap();
        assert!(result.is_none());

        let current = find_by_id(&pool, deployment.id).await.unwrap().unwrap();
        assert_eq!(current.status, DeploymentStatus::Terminating);

        // Same for mark_unhealthy.
        let result = mark_unhealthy(&pool, deployment.id, "flapping".to_string())
            .await
            .unwrap();
        assert!(result.is_none());
        let current = find_by_id(&pool, deployment.id).await.unwrap().unwrap();
        assert_eq!(current.status, DeploymentStatus::Terminating);
    }

    /// Compare a jsonb-stored timestamp against the `DateTime<Utc>` it was
    /// written from. Postgres truncates `timestamptz` to microsecond
    /// precision, so an exact string match against chrono's (sub-microsecond)
    /// `to_rfc3339()` would be brittle; a sub-millisecond tolerance is not.
    fn assert_close(actual: &serde_json::Value, expected: chrono::DateTime<chrono::Utc>) {
        let actual = actual
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .unwrap_or_else(|| panic!("expected an RFC3339 timestamp, got {actual:?}"))
            .with_timezone(&chrono::Utc);
        let diff = (actual - expected)
            .num_microseconds()
            .unwrap_or(i64::MAX)
            .abs();
        assert!(
            diff < 1000,
            "expected {expected} and stored {actual} to be within 1ms, differed by {diff}us",
        );
    }

    async fn seed_project_and_user(pool: &PgPool) -> (Uuid, Uuid) {
        let project_id = Uuid::new_v4();
        let user_id = Uuid::new_v4();
        sqlx::query!(
            "INSERT INTO users (id, email) VALUES ($1, $2)",
            user_id,
            format!("{user_id}@example.com")
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query!(
            "INSERT INTO projects (id, name, owner_user_id, access_class, status) VALUES ($1, $2, $3, $4, $5)",
            project_id,
            format!("test-project-{project_id}"),
            user_id,
            "private",
            "Running"
        )
        .execute(pool)
        .await
        .unwrap();
        (project_id, user_id)
    }

    async fn seed_deployment(
        pool: &PgPool,
        project_id: Uuid,
        user_id: Uuid,
        deployment_id: &str,
        group: &str,
        status: DeploymentStatus,
    ) -> Deployment {
        create(
            pool,
            CreateDeploymentParams {
                deployment_id,
                project_id,
                created_by_id: user_id,
                status,
                image: None,
                image_digest: None,
                rolled_back_from_deployment_id: None,
                deployment_group: group,
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
                containers: None,
                routes: None,
                expiration_cap: None,
            },
        )
        .await
        .unwrap()
    }

    /// The core atomicity proof: both rows move together, in one call.
    #[sqlx::test]
    async fn mark_healthy_and_supersede_marks_both_rows_together(pool: PgPool) {
        let (project_id, user_id) = seed_project_and_user(&pool).await;
        let old = seed_deployment(
            &pool,
            project_id,
            user_id,
            "20260101-000000",
            "default",
            DeploymentStatus::Healthy,
        )
        .await;
        let new = seed_deployment(
            &pool,
            project_id,
            user_id,
            "20260101-000001",
            "default",
            DeploymentStatus::Deploying,
        )
        .await;

        let outcome = mark_healthy_and_supersede(&pool, new.id, project_id, "default")
            .await
            .unwrap();

        assert!(outcome.became_healthy);
        let superseded = outcome.superseded.expect("old deployment was superseded");
        assert_eq!(superseded.id, old.id);

        let new_row = find_by_id(&pool, new.id).await.unwrap().unwrap();
        assert_eq!(new_row.status, DeploymentStatus::Healthy);
        let old_row = find_by_id(&pool, old.id).await.unwrap().unwrap();
        assert_eq!(old_row.status, DeploymentStatus::Terminating);
        assert_eq!(
            old_row.termination_reason,
            Some(TerminationReason::Superseded)
        );
    }

    /// When the primary write is rejected (deployment already moved to a
    /// protected status), the sibling supersession write must provably
    /// never have run — the realistic `#[sqlx::test]` proxy for "the two
    /// writes are atomic", since a literal mid-transaction crash isn't
    /// reproducible here.
    #[sqlx::test]
    async fn mark_healthy_and_supersede_does_not_touch_the_sibling_when_rejected(pool: PgPool) {
        let (project_id, user_id) = seed_project_and_user(&pool).await;
        let sibling = seed_deployment(
            &pool,
            project_id,
            user_id,
            "20260101-000000",
            "default",
            DeploymentStatus::Healthy,
        )
        .await;
        let new = seed_deployment(
            &pool,
            project_id,
            user_id,
            "20260101-000001",
            "default",
            DeploymentStatus::Deploying,
        )
        .await;
        // A concurrent stop request moves the incoming deployment to
        // Cancelling (the valid stop-path from Deploying — pre-infrastructure
        // states go through Cancelling, not Terminating) before the stale
        // reconciler tick calls this.
        mark_cancelling(&pool, new.id).await.unwrap().unwrap();

        let outcome = mark_healthy_and_supersede(&pool, new.id, project_id, "default")
            .await
            .unwrap();

        assert!(!outcome.became_healthy);
        assert!(outcome.superseded.is_none());

        let sibling_row = find_by_id(&pool, sibling.id).await.unwrap().unwrap();
        assert_eq!(
            sibling_row.status,
            DeploymentStatus::Healthy,
            "the sibling must be untouched when the primary write is rejected"
        );
    }

    /// A `Healthy` deployment in a different group must never be superseded.
    #[sqlx::test]
    async fn mark_healthy_and_supersede_leaves_other_groups_untouched(pool: PgPool) {
        let (project_id, user_id) = seed_project_and_user(&pool).await;
        let other_group = seed_deployment(
            &pool,
            project_id,
            user_id,
            "20260101-000000",
            "canary",
            DeploymentStatus::Healthy,
        )
        .await;
        let new = seed_deployment(
            &pool,
            project_id,
            user_id,
            "20260101-000001",
            "default",
            DeploymentStatus::Deploying,
        )
        .await;

        let outcome = mark_healthy_and_supersede(&pool, new.id, project_id, "default")
            .await
            .unwrap();

        assert!(outcome.became_healthy);
        assert!(outcome.superseded.is_none());
        let other_row = find_by_id(&pool, other_group.id).await.unwrap().unwrap();
        assert_eq!(other_row.status, DeploymentStatus::Healthy);
    }
}
