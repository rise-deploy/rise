use anyhow::{Context, Result};
use sqlx::PgPool;
use std::collections::HashMap;
use uuid::Uuid;

use crate::db::deployments;
use crate::db::models::{Project, ProjectStatus};

/// List all projects (optionally filtered by owner)
pub async fn list(pool: &PgPool, owner_user_id: Option<Uuid>) -> Result<Vec<Project>> {
    let projects = if let Some(user_id) = owner_user_id {
        sqlx::query_as!(
            Project,
            r#"
            SELECT
                id, name,
                status as "status: ProjectStatus",
                access_class,
                owner_user_id, owner_team_id,
                finalizers, source_url,
                created_at, updated_at
            FROM projects
            WHERE owner_user_id = $1
            ORDER BY created_at DESC
            "#,
            user_id
        )
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query_as!(
            Project,
            r#"
            SELECT
                id, name,
                status as "status: ProjectStatus",
                access_class,
                owner_user_id, owner_team_id,
                finalizers, source_url,
                created_at, updated_at
            FROM projects
            ORDER BY created_at DESC
            "#
        )
        .fetch_all(pool)
        .await?
    };

    Ok(projects)
}

/// List all projects accessible by a user (owned directly, via team membership, or via service account)
pub async fn list_accessible_by_user(pool: &PgPool, user_id: Uuid) -> Result<Vec<Project>> {
    let projects = sqlx::query_as!(
        Project,
        r#"
        SELECT DISTINCT
            p.id, p.name,
            p.status as "status: ProjectStatus",
            p.access_class,
            p.owner_user_id, p.owner_team_id,
            p.finalizers, p.source_url,
            p.created_at, p.updated_at
        FROM projects p
        WHERE
            p.owner_user_id = $1
            OR EXISTS(
                SELECT 1 FROM team_members tm
                WHERE tm.team_id = p.owner_team_id
                AND tm.user_id = $1
            )
            OR EXISTS(
                SELECT 1 FROM service_accounts sa
                WHERE sa.project_id = p.id
                AND sa.user_id = $1
                AND sa.deleted_at IS NULL
            )
        ORDER BY p.created_at DESC
        "#,
        user_id
    )
    .fetch_all(pool)
    .await
    .context("Failed to list accessible projects")?;

    Ok(projects)
}

/// Find project by name
pub async fn find_by_name(pool: &PgPool, name: &str) -> Result<Option<Project>> {
    let project = sqlx::query_as!(
        Project,
        r#"
        SELECT
            id, name,
            status as "status: ProjectStatus",
            access_class,
            owner_user_id, owner_team_id,
            finalizers, source_url,
            created_at, updated_at
        FROM projects
        WHERE name = $1
        "#,
        name
    )
    .fetch_optional(pool)
    .await
    .context("Failed to find project by name")?;

    Ok(project)
}

/// Find project by ID
pub async fn find_by_id(pool: &PgPool, id: Uuid) -> Result<Option<Project>> {
    let project = sqlx::query_as!(
        Project,
        r#"
        SELECT
            id, name,
            status as "status: ProjectStatus",
            access_class,
            owner_user_id, owner_team_id,
            finalizers, source_url,
            created_at, updated_at
        FROM projects
        WHERE id = $1
        "#,
        id
    )
    .fetch_optional(pool)
    .await
    .context("Failed to find project by ID")?;

    Ok(project)
}

/// Find multiple projects by their IDs in a single query
#[allow(dead_code)]
pub async fn find_by_ids(pool: &PgPool, ids: &[Uuid]) -> Result<Vec<Project>> {
    let projects = sqlx::query_as!(
        Project,
        r#"
        SELECT
            id, name,
            status as "status: ProjectStatus",
            access_class,
            owner_user_id, owner_team_id,
            finalizers, source_url,
            created_at, updated_at
        FROM projects
        WHERE id = ANY($1)
        "#,
        ids
    )
    .fetch_all(pool)
    .await
    .context("Failed to find projects by IDs")?;

    Ok(projects)
}

/// Create a new project
pub async fn create<'a, E>(
    executor: E,
    name: &str,
    status: ProjectStatus,
    access_class: String,
    owner_user_id: Option<Uuid>,
    owner_team_id: Option<Uuid>,
    source_url: Option<&str>,
) -> Result<Project>
where
    E: sqlx::Executor<'a, Database = sqlx::Postgres>,
{
    let status_str = status.to_string();

    let project = sqlx::query_as!(
        Project,
        r#"
        INSERT INTO projects (name, status, access_class, owner_user_id, owner_team_id, source_url)
        VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING
            id, name,
            status as "status: ProjectStatus",
            access_class,
            owner_user_id, owner_team_id,
            finalizers, source_url,
            created_at, updated_at
        "#,
        name,
        status_str,
        access_class,
        owner_user_id,
        owner_team_id,
        source_url
    )
    .fetch_one(executor)
    .await
    .context("Failed to create project")?;

    Ok(project)
}

/// Update project status
pub async fn update_status(pool: &PgPool, id: Uuid, status: ProjectStatus) -> Result<Project> {
    let status_str = status.to_string();

    let project = sqlx::query_as!(
        Project,
        r#"
        UPDATE projects
        SET status = $2
        WHERE id = $1
        RETURNING
            id, name,
            status as "status: ProjectStatus",
            access_class,
            owner_user_id, owner_team_id,
            finalizers, source_url,
            created_at, updated_at
        "#,
        id,
        status_str
    )
    .fetch_one(pool)
    .await
    .context("Failed to update project status")?;

    Ok(project)
}

/// Update project access class
pub async fn update_access_class(pool: &PgPool, id: Uuid, access_class: String) -> Result<Project> {
    let project = sqlx::query_as!(
        Project,
        r#"
        UPDATE projects
        SET access_class = $2
        WHERE id = $1
        RETURNING
            id, name,
            status as "status: ProjectStatus",
            access_class,
            owner_user_id, owner_team_id,
            finalizers, source_url,
            created_at, updated_at
        "#,
        id,
        access_class
    )
    .fetch_one(pool)
    .await
    .context("Failed to update project access class")?;

    Ok(project)
}

/// Update project owner (either user or team, mutually exclusive)
pub async fn update_owner(
    pool: &PgPool,
    id: Uuid,
    owner_user_id: Option<Uuid>,
    owner_team_id: Option<Uuid>,
) -> Result<Project> {
    let project = sqlx::query_as!(
        Project,
        r#"
        UPDATE projects
        SET owner_user_id = $2, owner_team_id = $3
        WHERE id = $1
        RETURNING
            id, name,
            status as "status: ProjectStatus",
            access_class,
            owner_user_id, owner_team_id,
            finalizers, source_url,
            created_at, updated_at
        "#,
        id,
        owner_user_id,
        owner_team_id
    )
    .fetch_one(pool)
    .await
    .context("Failed to update project owner")?;

    Ok(project)
}

/// Update project source URL
pub async fn update_source_url(
    pool: &PgPool,
    id: Uuid,
    source_url: Option<String>,
) -> Result<Project> {
    let project = sqlx::query_as!(
        Project,
        r#"
        UPDATE projects
        SET source_url = $2
        WHERE id = $1
        RETURNING
            id, name,
            status as "status: ProjectStatus",
            access_class,
            owner_user_id, owner_team_id,
            finalizers, source_url,
            created_at, updated_at
        "#,
        id,
        source_url
    )
    .fetch_one(pool)
    .await
    .context("Failed to update project source URL")?;

    Ok(project)
}

/// Delete project by ID
pub async fn delete(pool: &PgPool, id: Uuid) -> Result<()> {
    sqlx::query!("DELETE FROM projects WHERE id = $1", id)
        .execute(pool)
        .await
        .context("Failed to delete project")?;

    Ok(())
}

/// Count projects owned by a team
pub async fn count_owned_by_team(pool: &PgPool, team_id: Uuid) -> Result<i64> {
    let result = sqlx::query!(
        r#"SELECT COUNT(*) as "count!" FROM projects WHERE owner_team_id = $1"#,
        team_id
    )
    .fetch_one(pool)
    .await
    .context("Failed to count projects owned by team")?;

    Ok(result.count)
}

/// Check if user can access project (directly or via team)
pub async fn user_can_access(pool: &PgPool, project_id: Uuid, user_id: Uuid) -> Result<bool> {
    let result = sqlx::query!(
        r#"
        SELECT EXISTS(
            SELECT 1 FROM projects p
            WHERE p.id = $1 AND (
                p.owner_user_id = $2
                OR EXISTS(
                    SELECT 1 FROM team_members tm
                    WHERE tm.team_id = p.owner_team_id
                    AND tm.user_id = $2
                )
            )
        ) as "exists!"
        "#,
        project_id,
        user_id
    )
    .fetch_one(pool)
    .await
    .context("Failed to check project access")?;

    Ok(result.exists)
}

/// Calculate and update project status based on active deployment and last deployment
pub async fn update_calculated_status(pool: &PgPool, project_id: Uuid) -> Result<Project> {
    use crate::db::models::DeploymentStatus;

    // Get current project to check if it's in a protected lifecycle state
    let project = find_by_id(pool, project_id)
        .await?
        .context("Project not found")?;

    // Don't recalculate status for projects in deletion lifecycle
    // The deletion controller manages transitions for these states
    if matches!(
        project.status,
        ProjectStatus::Deleting | ProjectStatus::Terminated
    ) {
        return Ok(project);
    }

    // Get active deployment from the "default" group only
    // Other deployment groups (e.g., for merge requests) don't affect project status
    let active_deployment = deployments::find_active_deployment_for_group(
        pool,
        project_id,
        crate::server::deployment::models::DEFAULT_DEPLOYMENT_GROUP,
    )
    .await?;

    // Determine status based on active deployment first, then check for in-progress deployments
    let status = if let Some(active) = active_deployment.as_ref() {
        // Active deployment determines project status
        match active.status {
            DeploymentStatus::Healthy => ProjectStatus::Running,
            DeploymentStatus::Unhealthy => ProjectStatus::Failed,
            // Termination/cancellation in progress - show as Deploying (transitional)
            DeploymentStatus::Terminating | DeploymentStatus::Cancelling => {
                ProjectStatus::Deploying
            }
            // Other in-progress states
            DeploymentStatus::Pending
            | DeploymentStatus::Building
            | DeploymentStatus::Pushing
            | DeploymentStatus::Pushed
            | DeploymentStatus::Deploying => ProjectStatus::Deploying,
            // Terminal states shouldn't be active, but handle gracefully
            DeploymentStatus::Stopped
            | DeploymentStatus::Cancelled
            | DeploymentStatus::Superseded
            | DeploymentStatus::Failed
            | DeploymentStatus::Expired => ProjectStatus::Stopped,
        }
    } else {
        // No active deployment - check last deployment for in-progress or recent activity
        let last_deployment = deployments::find_last_for_project_and_group(
            pool,
            project_id,
            crate::server::deployment::models::DEFAULT_DEPLOYMENT_GROUP,
        )
        .await?;

        if let Some(last) = last_deployment.as_ref() {
            match last.status {
                // In-progress states - show as Deploying
                DeploymentStatus::Pending
                | DeploymentStatus::Building
                | DeploymentStatus::Pushing
                | DeploymentStatus::Pushed
                | DeploymentStatus::Deploying => ProjectStatus::Deploying,

                // Cancellation/Termination in progress
                DeploymentStatus::Cancelling | DeploymentStatus::Terminating => {
                    ProjectStatus::Deploying
                }

                // Terminal states with no active deployment - project is stopped
                DeploymentStatus::Failed
                | DeploymentStatus::Cancelled
                | DeploymentStatus::Stopped
                | DeploymentStatus::Superseded
                | DeploymentStatus::Expired => ProjectStatus::Stopped,

                // Running states without being active (shouldn't happen, but treat as stopped)
                DeploymentStatus::Healthy | DeploymentStatus::Unhealthy => ProjectStatus::Stopped,
            }
        } else {
            // No deployments in default group at all
            ProjectStatus::Stopped
        }
    };

    update_status(pool, project_id, status).await
}

/// Active deployment info returned by batch queries
#[derive(Debug, Clone)]
pub struct ActiveDeploymentInfo {
    pub id: Uuid,
    pub status: crate::db::models::DeploymentStatus,
}

/// Get active deployment info (deployment_id and status) for multiple projects (batch operation)
/// Returns a map of project_id -> ActiveDeploymentInfo
/// Fetches the active deployment from the default deployment group using the is_active flag
pub async fn get_active_deployment_info_batch(
    pool: &PgPool,
    project_ids: &[Uuid],
) -> Result<HashMap<Uuid, Option<ActiveDeploymentInfo>>> {
    let results = sqlx::query!(
        r#"
        SELECT
            p.id as project_id,
            d.id as "id?",
            d.status as "status?: crate::db::models::DeploymentStatus"
        FROM unnest($1::uuid[]) AS p(id)
        LEFT JOIN deployments d ON d.project_id = p.id
            AND d.is_active = TRUE
            AND d.deployment_group = 'default'
        "#,
        project_ids
    )
    .fetch_all(pool)
    .await
    .context("Failed to get active deployment info in batch")?;

    Ok(results
        .into_iter()
        .filter_map(|r| {
            r.project_id.map(|project_id| {
                // In sqlx 0.8, LEFT JOIN makes fields Option<T> (already nullable)
                let info = match (r.id, r.status) {
                    (Some(id), Some(status)) => Some(ActiveDeploymentInfo { id, status }),
                    _ => None,
                };
                (project_id, info)
            })
        })
        .collect())
}

/// Mark project as deleting
pub async fn mark_deleting(pool: &PgPool, id: Uuid) -> Result<Project> {
    let project = sqlx::query_as!(
        Project,
        r#"
        UPDATE projects
        SET status = 'Deleting', updated_at = NOW()
        WHERE id = $1
        RETURNING
            id, name,
            status as "status: ProjectStatus",
            access_class,
            owner_user_id, owner_team_id,
            finalizers, source_url,
            created_at, updated_at
        "#,
        id
    )
    .fetch_one(pool)
    .await
    .context("Failed to mark project as deleting")?;

    Ok(project)
}

/// Find all projects in Deleting status
pub async fn find_deleting(pool: &PgPool, limit: i64) -> Result<Vec<Project>> {
    let projects = sqlx::query_as!(
        Project,
        r#"
        SELECT
            id, name,
            status as "status: ProjectStatus",
            access_class,
            owner_user_id, owner_team_id,
            finalizers, source_url,
            created_at, updated_at
        FROM projects
        WHERE status = 'Deleting'
        ORDER BY updated_at ASC
        LIMIT $1
        "#,
        limit
    )
    .fetch_all(pool)
    .await
    .context("Failed to find deleting projects")?;

    Ok(projects)
}

// ==================== Finalizer Operations ====================

/// Add a finalizer to a project (idempotent - won't add if already exists)
#[cfg(feature = "backend")]
pub async fn add_finalizer(pool: &PgPool, id: Uuid, finalizer: &str) -> Result<()> {
    sqlx::query!(
        r#"
        UPDATE projects
        SET finalizers = CASE
            WHEN $2 = ANY(finalizers) THEN finalizers
            ELSE array_append(finalizers, $2)
        END
        WHERE id = $1
        "#,
        id,
        finalizer
    )
    .execute(pool)
    .await
    .context("Failed to add finalizer")?;

    Ok(())
}

/// Remove a finalizer from a project
#[cfg(feature = "backend")]
pub async fn remove_finalizer(pool: &PgPool, id: Uuid, finalizer: &str) -> Result<()> {
    sqlx::query!(
        r#"
        UPDATE projects
        SET finalizers = array_remove(finalizers, $2)
        WHERE id = $1
        "#,
        id,
        finalizer
    )
    .execute(pool)
    .await
    .context("Failed to remove finalizer")?;

    Ok(())
}

/// Find projects in Deleting status that have a specific finalizer
#[cfg(feature = "backend")]
pub async fn find_deleting_with_finalizer(
    pool: &PgPool,
    finalizer: &str,
    limit: i64,
) -> Result<Vec<Project>> {
    let projects = sqlx::query_as!(
        Project,
        r#"
        SELECT
            id, name,
            status as "status: ProjectStatus",
            access_class,
            owner_user_id, owner_team_id,
            finalizers, source_url,
            created_at, updated_at
        FROM projects
        WHERE status = 'Deleting' AND $1 = ANY(finalizers)
        ORDER BY updated_at ASC
        LIMIT $2
        "#,
        finalizer,
        limit
    )
    .fetch_all(pool)
    .await
    .context("Failed to find deleting projects with finalizer")?;

    Ok(projects)
}

/// Check if a project has any finalizers remaining
pub async fn has_finalizers(pool: &PgPool, id: Uuid) -> Result<bool> {
    let result = sqlx::query!(
        r#"
        SELECT cardinality(finalizers) > 0 as "has_finalizers!"
        FROM projects
        WHERE id = $1
        "#,
        id
    )
    .fetch_one(pool)
    .await
    .context("Failed to check project finalizers")?;

    Ok(result.has_finalizers)
}

/// List all active projects (not in Deleting or Terminated status)
#[cfg(feature = "backend")]
pub async fn list_active(pool: &PgPool) -> Result<Vec<Project>> {
    let projects = sqlx::query_as!(
        Project,
        r#"
        SELECT
            id, name,
            status as "status: ProjectStatus",
            access_class,
            owner_user_id, owner_team_id,
            finalizers, source_url,
            created_at, updated_at
        FROM projects
        WHERE status NOT IN ('Deleting', 'Terminated')
        ORDER BY created_at DESC
        "#
    )
    .fetch_all(pool)
    .await
    .context("Failed to list active projects")?;

    Ok(projects)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::deployments::CreateDeploymentParams;
    use crate::db::models::{DeploymentStatus, ProjectStatus};
    use crate::server::deployment::models::DEFAULT_DEPLOYMENT_GROUP;

    /// Test that project status is based on active deployment, not the latest deployment
    #[sqlx::test]
    async fn test_project_status_with_active_and_failed_deployments(pool: PgPool) {
        // Create a test user
        let user = crate::db::users::create(&pool, "test@example.com")
            .await
            .expect("Failed to create test user");

        // Create a test project
        let project = create(
            &pool,
            "test-project",
            ProjectStatus::Stopped,
            "default".to_string(),
            Some(user.id),
            None,
            None,
        )
        .await
        .expect("Failed to create test project");

        // Create a healthy deployment (older, but active)
        let healthy_deployment = deployments::create(
            &pool,
            CreateDeploymentParams {
                deployment_id: "20251220-100000",
                project_id: project.id,
                created_by_id: user.id,
                status: DeploymentStatus::Healthy,
                image: Some("test:v1"),
                image_digest: None,
                rolled_back_from_deployment_id: None,
                deployment_group: DEFAULT_DEPLOYMENT_GROUP,
                environment_id: None,
                expires_at: None,
                http_port: 8080,
                is_active: false, // Initially not active
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
        .expect("Failed to create healthy deployment");

        // Mark the healthy deployment as active
        deployments::mark_as_active(
            &pool,
            healthy_deployment.id,
            project.id,
            DEFAULT_DEPLOYMENT_GROUP,
        )
        .await
        .expect("Failed to mark deployment as active");

        // Update project status based on active deployment
        let project = update_calculated_status(&pool, project.id)
            .await
            .expect("Failed to update project status");

        // Project should be Running because the active deployment is Healthy
        assert_eq!(
            project.status,
            ProjectStatus::Running,
            "Project should be Running with active healthy deployment"
        );

        // Now create a newer failed deployment (not active)
        let _failed_deployment = deployments::create(
            &pool,
            CreateDeploymentParams {
                deployment_id: "20251220-110000",
                project_id: project.id,
                created_by_id: user.id,
                status: DeploymentStatus::Failed,
                image: Some("test:v2"),
                image_digest: None,
                rolled_back_from_deployment_id: None,
                deployment_group: DEFAULT_DEPLOYMENT_GROUP,
                environment_id: None,
                expires_at: None,
                http_port: 8080,
                is_active: false, // This is NOT active
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
        .expect("Failed to create failed deployment");

        // Update project status again
        let project = update_calculated_status(&pool, project.id)
            .await
            .expect("Failed to update project status");

        // Project should STILL be Running because the ACTIVE deployment is Healthy
        // even though the latest deployment failed
        assert_eq!(
            project.status,
            ProjectStatus::Running,
            "Project should remain Running with active healthy deployment, even when latest deployment failed"
        );
    }

    /// Test setting and clearing source_url on a project
    #[sqlx::test]
    async fn test_update_source_url(pool: PgPool) {
        let user = crate::db::users::create(&pool, "test@example.com")
            .await
            .expect("Failed to create test user");

        let project = create(
            &pool,
            "source-url-test",
            ProjectStatus::Stopped,
            "default".to_string(),
            Some(user.id),
            None,
            None,
        )
        .await
        .expect("Failed to create test project");

        assert!(
            project.source_url.is_none(),
            "source_url should be None initially"
        );

        // Set source_url
        let project = update_source_url(
            &pool,
            project.id,
            Some("https://github.com/example/repo".to_string()),
        )
        .await
        .expect("Failed to set source_url");

        assert_eq!(
            project.source_url.as_deref(),
            Some("https://github.com/example/repo"),
            "source_url should be set"
        );

        // Clear source_url
        let project = update_source_url(&pool, project.id, None)
            .await
            .expect("Failed to clear source_url");

        assert!(project.source_url.is_none(), "source_url should be cleared");
    }

    /// Test that project status is Stopped when no active deployment but has failed deployment
    #[sqlx::test]
    async fn test_project_status_with_only_failed_deployment(pool: PgPool) {
        // Create a test user
        let user = crate::db::users::create(&pool, "test@example.com")
            .await
            .expect("Failed to create test user");

        // Create a test project
        let project = create(
            &pool,
            "test-project-2",
            ProjectStatus::Stopped,
            "default".to_string(),
            Some(user.id),
            None,
            None,
        )
        .await
        .expect("Failed to create test project");

        // Create a failed deployment (not active)
        let _failed_deployment = deployments::create(
            &pool,
            CreateDeploymentParams {
                deployment_id: "20251220-120000",
                project_id: project.id,
                created_by_id: user.id,
                status: DeploymentStatus::Failed,
                image: Some("test:v1"),
                image_digest: None,
                rolled_back_from_deployment_id: None,
                deployment_group: DEFAULT_DEPLOYMENT_GROUP,
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
        .expect("Failed to create failed deployment");

        // Update project status
        let project = update_calculated_status(&pool, project.id)
            .await
            .expect("Failed to update project status");

        // Project should be Stopped (not Failed) when no active deployment exists
        // Terminal states without active deployments mean the project is stopped
        assert_eq!(
            project.status,
            ProjectStatus::Stopped,
            "Project should be Stopped when only failed deployment exists and no active deployment"
        );
    }
}
