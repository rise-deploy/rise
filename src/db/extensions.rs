use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use super::models::ProjectExtension;

/// Create or update extension for project
/// Create a new extension (fails if already exists)
pub async fn create(
    pool: &PgPool,
    project_id: Uuid,
    extension: &str,
    extension_type: &str,
    spec: &Value,
) -> Result<ProjectExtension> {
    sqlx::query_as!(
        ProjectExtension,
        r#"
        INSERT INTO project_extensions (project_id, extension, extension_type, spec)
        VALUES ($1, $2, $3, $4)
        RETURNING project_id, extension, extension_type,
                  spec as "spec: Value",
                  status as "status: Value",
                  created_at, updated_at, deleted_at
        "#,
        project_id,
        extension,
        extension_type,
        spec
    )
    .fetch_one(pool)
    .await
    .context("Failed to create project extension")
}

/// List all extensions for a project (including soft-deleted ones)
pub async fn list_by_project(pool: &PgPool, project_id: Uuid) -> Result<Vec<ProjectExtension>> {
    sqlx::query_as!(
        ProjectExtension,
        r#"
        SELECT project_id, extension, extension_type,
               spec as "spec: Value",
               status as "status: Value",
               created_at, updated_at, deleted_at
        FROM project_extensions
        WHERE project_id = $1
        ORDER BY created_at ASC
        "#,
        project_id
    )
    .fetch_all(pool)
    .await
    .context("Failed to list project extensions")
}

/// List all extensions with a specific extension name (across all projects)
#[allow(dead_code)]
pub async fn list_by_extension_name(
    pool: &PgPool,
    extension_name: &str,
) -> Result<Vec<ProjectExtension>> {
    sqlx::query_as!(
        ProjectExtension,
        r#"
        SELECT project_id, extension, extension_type,
               spec as "spec: Value",
               status as "status: Value",
               created_at, updated_at, deleted_at
        FROM project_extensions
        WHERE extension = $1
        ORDER BY created_at ASC
        "#,
        extension_name
    )
    .fetch_all(pool)
    .await
    .context("Failed to list extensions by name")
}

/// Get extension by project and name (including soft-deleted ones)
pub async fn find_by_project_and_name(
    pool: &PgPool,
    project_id: Uuid,
    extension: &str,
) -> Result<Option<ProjectExtension>> {
    sqlx::query_as!(
        ProjectExtension,
        r#"
        SELECT project_id, extension, extension_type,
               spec as "spec: Value",
               status as "status: Value",
               created_at, updated_at, deleted_at
        FROM project_extensions
        WHERE project_id = $1 AND extension = $2
        "#,
        project_id,
        extension
    )
    .fetch_optional(pool)
    .await
    .context("Failed to find project extension")
}

/// Mark extension for deletion
pub async fn mark_deleted(
    pool: &PgPool,
    project_id: Uuid,
    extension: &str,
) -> Result<ProjectExtension> {
    sqlx::query_as!(
        ProjectExtension,
        r#"
        UPDATE project_extensions
        SET deleted_at = NOW()
        WHERE project_id = $1 AND extension = $2 AND deleted_at IS NULL
        RETURNING project_id, extension, extension_type,
                  spec as "spec: Value",
                  status as "status: Value",
                  created_at, updated_at, deleted_at
        "#,
        project_id,
        extension
    )
    .fetch_one(pool)
    .await
    .context("Failed to mark extension as deleted")
}

/// Update extension status
#[allow(dead_code)]
pub async fn update_status(
    pool: &PgPool,
    project_id: Uuid,
    extension: &str,
    status: &Value,
) -> Result<()> {
    sqlx::query!(
        r#"
        UPDATE project_extensions
        SET status = $3, updated_at = NOW()
        WHERE project_id = $1 AND extension = $2
        "#,
        project_id,
        extension,
        status
    )
    .execute(pool)
    .await
    .context("Failed to update extension status")?;

    Ok(())
}

/// Update extension spec
pub async fn update_spec(
    pool: &PgPool,
    project_id: Uuid,
    extension: &str,
    spec: &Value,
) -> Result<()> {
    sqlx::query!(
        r#"
        UPDATE project_extensions
        SET spec = $3, updated_at = NOW()
        WHERE project_id = $1 AND extension = $2
        "#,
        project_id,
        extension,
        spec
    )
    .execute(pool)
    .await
    .context("Failed to update extension spec")?;

    Ok(())
}

/// Permanently delete extension record
#[allow(dead_code)]
pub async fn delete_permanently(pool: &PgPool, project_id: Uuid, extension: &str) -> Result<()> {
    sqlx::query!(
        r#"
        DELETE FROM project_extensions
        WHERE project_id = $1 AND extension = $2
        "#,
        project_id,
        extension
    )
    .execute(pool)
    .await
    .context("Failed to permanently delete extension")?;

    Ok(())
}

/// List all extensions with a specific extension type (across all projects)
#[allow(dead_code)]
pub async fn list_by_extension_type(
    pool: &PgPool,
    extension_type: &str,
) -> Result<Vec<ProjectExtension>> {
    sqlx::query_as!(
        ProjectExtension,
        r#"
        SELECT project_id, extension, extension_type,
               spec as "spec: Value",
               status as "status: Value",
               created_at, updated_at, deleted_at
        FROM project_extensions
        WHERE extension_type = $1
        ORDER BY created_at ASC
        "#,
        extension_type
    )
    .fetch_all(pool)
    .await
    .context("Failed to list extensions by type")
}
