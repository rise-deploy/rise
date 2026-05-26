use anyhow::{Context, Result};
use sqlx::PgPool;
use std::collections::HashMap;
use uuid::Uuid;

use crate::db::models::CustomDomain;

/// List all custom domains for a project (all environments)
pub async fn list_project_custom_domains(
    pool: &PgPool,
    project_id: Uuid,
) -> Result<Vec<CustomDomain>> {
    let domains = sqlx::query_as!(
        CustomDomain,
        r#"
        SELECT id, project_id, environment_id, domain, is_primary, created_at, updated_at
        FROM project_custom_domains
        WHERE project_id = $1
        ORDER BY domain ASC
        "#,
        project_id
    )
    .fetch_all(pool)
    .await
    .context("Failed to list project custom domains")?;

    Ok(domains)
}

/// List custom domains for a single environment
#[allow(dead_code)]
pub async fn list_environment_custom_domains(
    pool: &PgPool,
    environment_id: Uuid,
) -> Result<Vec<CustomDomain>> {
    let domains = sqlx::query_as!(
        CustomDomain,
        r#"
        SELECT id, project_id, environment_id, domain, is_primary, created_at, updated_at
        FROM project_custom_domains
        WHERE environment_id = $1
        ORDER BY domain ASC
        "#,
        environment_id
    )
    .fetch_all(pool)
    .await
    .context("Failed to list environment custom domains")?;

    Ok(domains)
}

/// Get a specific custom domain for a project
pub async fn get_custom_domain(
    pool: &PgPool,
    project_id: Uuid,
    domain: &str,
) -> Result<Option<CustomDomain>> {
    let domain = sqlx::query_as!(
        CustomDomain,
        r#"
        SELECT id, project_id, environment_id, domain, is_primary, created_at, updated_at
        FROM project_custom_domains
        WHERE project_id = $1 AND domain = $2
        "#,
        project_id,
        domain
    )
    .fetch_optional(pool)
    .await
    .context("Failed to get custom domain")?;

    Ok(domain)
}

/// Add a new custom domain to a project, attached to a specific environment.
pub async fn add_custom_domain(
    pool: &PgPool,
    project_id: Uuid,
    environment_id: Uuid,
    domain: &str,
) -> Result<CustomDomain> {
    let domain = sqlx::query_as!(
        CustomDomain,
        r#"
        INSERT INTO project_custom_domains (project_id, environment_id, domain)
        VALUES ($1, $2, $3)
        RETURNING id, project_id, environment_id, domain, is_primary, created_at, updated_at
        "#,
        project_id,
        environment_id,
        domain
    )
    .fetch_one(pool)
    .await
    .context("Failed to add custom domain")?;

    Ok(domain)
}

/// Delete a custom domain from a project
pub async fn delete_custom_domain(pool: &PgPool, project_id: Uuid, domain: &str) -> Result<bool> {
    let result = sqlx::query!(
        r#"
        DELETE FROM project_custom_domains
        WHERE project_id = $1 AND domain = $2
        "#,
        project_id,
        domain
    )
    .execute(pool)
    .await
    .context("Failed to delete custom domain")?;

    Ok(result.rows_affected() > 0)
}

/// Get all custom domains for multiple projects in one query
/// Returns a HashMap mapping project_id to a vector of custom domains
#[allow(dead_code)]
pub async fn get_custom_domains_batch(
    pool: &PgPool,
    project_ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<CustomDomain>>> {
    let domains = sqlx::query_as!(
        CustomDomain,
        r#"
        SELECT id, project_id, environment_id, domain, is_primary, created_at, updated_at
        FROM project_custom_domains
        WHERE project_id = ANY($1)
        ORDER BY project_id, domain ASC
        "#,
        project_ids
    )
    .fetch_all(pool)
    .await
    .context("Failed to fetch custom domains batch")?;

    let mut map: HashMap<Uuid, Vec<CustomDomain>> = HashMap::new();
    for domain in domains {
        map.entry(domain.project_id).or_default().push(domain);
    }

    Ok(map)
}

/// Reassign a custom domain to a different environment. The domain's
/// `is_primary` flag is cleared as part of the move so the new environment
/// keeps its own primary intact and the partial unique index doesn't block the
/// update.
pub async fn update_custom_domain_environment(
    pool: &PgPool,
    project_id: Uuid,
    domain: &str,
    new_environment_id: Uuid,
) -> Result<CustomDomain> {
    let domain = sqlx::query_as!(
        CustomDomain,
        r#"
        UPDATE project_custom_domains
        SET environment_id = $3,
            is_primary = false
        WHERE project_id = $1 AND domain = $2
        RETURNING id, project_id, environment_id, domain, is_primary, created_at, updated_at
        "#,
        project_id,
        domain,
        new_environment_id
    )
    .fetch_one(pool)
    .await
    .context("Failed to update custom domain environment")?;

    Ok(domain)
}

/// Set a custom domain as primary within its environment.
/// Unsets any existing primary domain in the same environment.
pub async fn set_primary_domain(
    pool: &PgPool,
    project_id: Uuid,
    domain: &str,
) -> Result<CustomDomain> {
    let mut tx = pool.begin().await.context("Failed to begin transaction")?;

    let target = sqlx::query!(
        r#"
        SELECT environment_id
        FROM project_custom_domains
        WHERE project_id = $1 AND domain = $2
        "#,
        project_id,
        domain
    )
    .fetch_one(&mut *tx)
    .await
    .context("Failed to look up custom domain for primary assignment")?;

    sqlx::query!(
        r#"
        UPDATE project_custom_domains
        SET is_primary = false
        WHERE environment_id = $1 AND is_primary = true
        "#,
        target.environment_id
    )
    .execute(&mut *tx)
    .await
    .context("Failed to unset existing primary domain")?;

    let domain = sqlx::query_as!(
        CustomDomain,
        r#"
        UPDATE project_custom_domains
        SET is_primary = true
        WHERE project_id = $1 AND domain = $2
        RETURNING id, project_id, environment_id, domain, is_primary, created_at, updated_at
        "#,
        project_id,
        domain
    )
    .fetch_one(&mut *tx)
    .await
    .context("Failed to set primary domain")?;

    tx.commit().await.context("Failed to commit transaction")?;

    Ok(domain)
}

/// Unset the primary status of a custom domain
pub async fn unset_primary_domain(pool: &PgPool, project_id: Uuid, domain: &str) -> Result<bool> {
    let result = sqlx::query!(
        r#"
        UPDATE project_custom_domains
        SET is_primary = false
        WHERE project_id = $1 AND domain = $2 AND is_primary = true
        "#,
        project_id,
        domain
    )
    .execute(pool)
    .await
    .context("Failed to unset primary domain")?;

    Ok(result.rows_affected() > 0)
}
