use anyhow::{bail, Context, Result};
use sqlx::PgPool;
use uuid::Uuid;

use crate::db::models::Environment;

/// Create a new environment for a project
#[allow(clippy::too_many_arguments)]
pub async fn create<'a, E>(
    executor: E,
    project_id: Uuid,
    name: &str,
    primary_deployment_group: Option<&str>,
    is_production: bool,
    color: &str,
    max_deployment_expiration: Option<&str>,
) -> Result<Environment>
where
    E: sqlx::Executor<'a, Database = sqlx::Postgres>,
{
    let env = sqlx::query_as!(
        Environment,
        r#"
        INSERT INTO environments (project_id, name, primary_deployment_group, is_production, color, max_deployment_expiration)
        VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING id, project_id, name, primary_deployment_group, is_production, color,
                 min_replicas, max_replicas, min_cpu, max_cpu, min_memory, max_memory,
                 max_deployment_expiration,
                 created_at, updated_at
        "#,
        project_id,
        name,
        primary_deployment_group,
        is_production,
        color,
        max_deployment_expiration
    )
    .fetch_one(executor)
    .await
    .context("Failed to create environment")?;

    Ok(env)
}

/// List all environments for a project
pub async fn list_for_project(pool: &PgPool, project_id: Uuid) -> Result<Vec<Environment>> {
    let envs = sqlx::query_as!(
        Environment,
        r#"
        SELECT id, project_id, name, primary_deployment_group, is_production, color,
               min_replicas, max_replicas, min_cpu, max_cpu, min_memory, max_memory,
               max_deployment_expiration,
               created_at, updated_at
        FROM environments
        WHERE project_id = $1
        ORDER BY
            is_production DESC,
            name ASC
        "#,
        project_id
    )
    .fetch_all(pool)
    .await
    .context("Failed to list environments for project")?;

    Ok(envs)
}

/// Find an environment by name within a project
pub async fn find_by_name(
    pool: &PgPool,
    project_id: Uuid,
    name: &str,
) -> Result<Option<Environment>> {
    let env = sqlx::query_as!(
        Environment,
        r#"
        SELECT id, project_id, name, primary_deployment_group, is_production, color,
               min_replicas, max_replicas, min_cpu, max_cpu, min_memory, max_memory,
               max_deployment_expiration,
               created_at, updated_at
        FROM environments
        WHERE project_id = $1 AND name = $2
        "#,
        project_id,
        name
    )
    .fetch_optional(pool)
    .await
    .context("Failed to find environment by name")?;

    Ok(env)
}

/// Find an environment by its primary deployment group within a project
pub async fn find_by_primary_group(
    pool: &PgPool,
    project_id: Uuid,
    group: &str,
) -> Result<Option<Environment>> {
    let env = sqlx::query_as!(
        Environment,
        r#"
        SELECT id, project_id, name, primary_deployment_group, is_production, color,
               min_replicas, max_replicas, min_cpu, max_cpu, min_memory, max_memory,
               max_deployment_expiration,
               created_at, updated_at
        FROM environments
        WHERE project_id = $1 AND primary_deployment_group = $2
        "#,
        project_id,
        group
    )
    .fetch_optional(pool)
    .await
    .context("Failed to find environment by primary group")?;

    Ok(env)
}

/// Find the production environment for a project
#[allow(dead_code)]
pub async fn find_production(pool: &PgPool, project_id: Uuid) -> Result<Option<Environment>> {
    let env = sqlx::query_as!(
        Environment,
        r#"
        SELECT id, project_id, name, primary_deployment_group, is_production, color,
               min_replicas, max_replicas, min_cpu, max_cpu, min_memory, max_memory,
               max_deployment_expiration,
               created_at, updated_at
        FROM environments
        WHERE project_id = $1 AND is_production = true
        "#,
        project_id
    )
    .fetch_optional(pool)
    .await
    .context("Failed to find production environment")?;

    Ok(env)
}

/// Find an environment by ID
pub async fn find_by_id(pool: &PgPool, id: Uuid) -> Result<Option<Environment>> {
    let env = sqlx::query_as!(
        Environment,
        r#"
        SELECT id, project_id, name, primary_deployment_group, is_production, color,
               min_replicas, max_replicas, min_cpu, max_cpu, min_memory, max_memory,
               max_deployment_expiration,
               created_at, updated_at
        FROM environments
        WHERE id = $1
        "#,
        id
    )
    .fetch_optional(pool)
    .await
    .context("Failed to find environment by ID")?;

    Ok(env)
}

/// Clear `is_production` flag from other environments in the same project.
/// Used by both [`update`] and [`create_with_flag_swap`] to ensure
/// the partial unique index is never violated.
///
/// `exclude_id` is the environment being updated (or `Uuid::nil()` during create).
async fn clear_exclusive_flags(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    project_id: Uuid,
    exclude_id: Uuid,
    set_production: bool,
) -> Result<()> {
    if set_production {
        sqlx::query!(
            "UPDATE environments SET is_production = false, updated_at = NOW() WHERE project_id = $1 AND id != $2 AND is_production = true",
            project_id,
            exclude_id
        )
        .execute(&mut **tx)
        .await
        .context("Failed to clear is_production flag")?;
    }

    Ok(())
}

/// Update an environment.
///
/// Uses a transaction to atomically swap `is_production` flag
/// when setting it on this environment (clearing it from any other environment first).
#[allow(clippy::too_many_arguments)]
pub async fn update(
    pool: &PgPool,
    id: Uuid,
    project_id: Uuid,
    name: Option<&str>,
    primary_deployment_group: Option<Option<&str>>,
    is_production: Option<bool>,
    color: Option<&str>,
    max_deployment_expiration: Option<Option<&str>>,
) -> Result<Environment> {
    let mut tx = pool.begin().await.context("Failed to begin transaction")?;

    clear_exclusive_flags(&mut tx, project_id, id, is_production == Some(true)).await?;

    let env = sqlx::query_as!(
        Environment,
        r#"
        UPDATE environments
        SET
            name = COALESCE($2, name),
            primary_deployment_group = CASE WHEN $3 THEN $4 ELSE primary_deployment_group END,
            is_production = COALESCE($5, is_production),
            color = COALESCE($6, color),
            max_deployment_expiration = CASE WHEN $7 THEN $8 ELSE max_deployment_expiration END,
            updated_at = NOW()
        WHERE id = $1
        RETURNING id, project_id, name, primary_deployment_group, is_production, color,
                 min_replicas, max_replicas, min_cpu, max_cpu, min_memory, max_memory,
                 max_deployment_expiration,
                 created_at, updated_at
        "#,
        id,
        name,
        primary_deployment_group.is_some(), // $3: whether to update primary_deployment_group
        primary_deployment_group.flatten(), // $4: the new value (can be NULL)
        is_production,
        color,
        max_deployment_expiration.is_some(), // $7: whether to update max_deployment_expiration
        max_deployment_expiration.flatten(), // $8: the new value (can be NULL)
    )
    .fetch_one(&mut *tx)
    .await
    .context("Failed to update environment")?;

    tx.commit().await.context("Failed to commit transaction")?;

    Ok(env)
}

/// Create a new environment, atomically swapping `is_production` flag
/// from other environments in the same project when the flag is set.
#[allow(clippy::too_many_arguments)]
pub async fn create_with_flag_swap(
    pool: &PgPool,
    project_id: Uuid,
    name: &str,
    primary_deployment_group: Option<&str>,
    is_production: bool,
    color: &str,
    max_deployment_expiration: Option<&str>,
) -> Result<Environment> {
    let mut tx = pool.begin().await.context("Failed to begin transaction")?;

    clear_exclusive_flags(&mut tx, project_id, Uuid::nil(), is_production).await?;

    let env = create(
        &mut *tx,
        project_id,
        name,
        primary_deployment_group,
        is_production,
        color,
        max_deployment_expiration,
    )
    .await?;

    tx.commit().await.context("Failed to commit transaction")?;

    Ok(env)
}

/// Update per-environment deployment constraints (admin only)
#[allow(clippy::too_many_arguments)]
pub async fn update_deployment_constraints(
    pool: &PgPool,
    id: Uuid,
    min_replicas: Option<i32>,
    max_replicas: Option<i32>,
    min_cpu: Option<String>,
    max_cpu: Option<String>,
    min_memory: Option<String>,
    max_memory: Option<String>,
) -> Result<Environment> {
    let env = sqlx::query_as!(
        Environment,
        r#"
        UPDATE environments
        SET min_replicas = $2, max_replicas = $3, min_cpu = $4, max_cpu = $5,
            min_memory = $6, max_memory = $7, updated_at = NOW()
        WHERE id = $1
        RETURNING id, project_id, name, primary_deployment_group, is_production, color,
                 min_replicas, max_replicas, min_cpu, max_cpu, min_memory, max_memory,
                 max_deployment_expiration,
                 created_at, updated_at
        "#,
        id,
        min_replicas,
        max_replicas,
        min_cpu,
        max_cpu,
        min_memory,
        max_memory
    )
    .fetch_one(pool)
    .await
    .context("Failed to update deployment constraints")?;

    Ok(env)
}

/// Delete an environment by ID.
///
/// Returns an error if the environment has `is_production` set, since that
/// flag must be transferred to another environment first.
pub async fn delete(pool: &PgPool, id: Uuid) -> Result<bool> {
    // Check flags before deleting
    let env = find_by_id(pool, id).await?;
    if let Some(ref env) = env {
        if env.is_production {
            bail!("Cannot delete the production environment. Transfer the production flag to another environment first.");
        }
    }

    let result = sqlx::query!("DELETE FROM environments WHERE id = $1", id)
        .execute(pool)
        .await
        .context("Failed to delete environment")?;

    Ok(result.rows_affected() > 0)
}

/// Bootstrap the default "production" environment for a newly created project.
///
/// Creates a single environment named "production" with `is_production=true`
/// and `primary_deployment_group="default"`.
pub async fn create_default_for_project<'a, E>(executor: E, project_id: Uuid) -> Result<Environment>
where
    E: sqlx::Executor<'a, Database = sqlx::Postgres>,
{
    create(
        executor,
        project_id,
        "production",
        Some("default"),
        true,
        "green",
        None,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{models::ProjectStatus, projects, users};

    #[sqlx::test]
    async fn test_create_and_find_environment(pool: PgPool) -> Result<()> {
        let user = users::create(&pool, "env-test@example.com").await?;
        let project = projects::create(
            &pool,
            "env-test-project",
            ProjectStatus::Stopped,
            "public".to_string(),
            Some(user.id),
            None,
            None,
        )
        .await?;

        let env = create(
            &pool,
            project.id,
            "staging",
            Some("staging"),
            false,
            "green",
            None,
        )
        .await?;
        assert_eq!(env.name, "staging");
        assert_eq!(env.primary_deployment_group.as_deref(), Some("staging"));
        assert!(!env.is_production);

        let found = find_by_name(&pool, project.id, "staging").await?;
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, env.id);

        Ok(())
    }

    #[sqlx::test]
    async fn test_create_round_trips_max_deployment_expiration(pool: PgPool) -> Result<()> {
        let user = users::create(&pool, "env-test@example.com").await?;
        let project = projects::create(
            &pool,
            "env-test-project",
            ProjectStatus::Stopped,
            "public".to_string(),
            Some(user.id),
            None,
            None,
        )
        .await?;

        let env = create(
            &pool,
            project.id,
            "staging",
            Some("staging"),
            false,
            "green",
            Some("7d"),
        )
        .await?;
        assert_eq!(env.max_deployment_expiration.as_deref(), Some("7d"));

        let found = find_by_name(&pool, project.id, "staging").await?.unwrap();
        assert_eq!(found.max_deployment_expiration.as_deref(), Some("7d"));

        Ok(())
    }

    #[sqlx::test]
    async fn test_update_max_deployment_expiration_set_leave_and_clear(pool: PgPool) -> Result<()> {
        let user = users::create(&pool, "env-test@example.com").await?;
        let project = projects::create(
            &pool,
            "env-test-project",
            ProjectStatus::Stopped,
            "public".to_string(),
            Some(user.id),
            None,
            None,
        )
        .await?;

        let env = create(
            &pool,
            project.id,
            "staging",
            Some("staging"),
            false,
            "green",
            None,
        )
        .await?;
        assert_eq!(env.max_deployment_expiration, None);

        // `Some(Some(_))` sets it.
        let env = update(
            &pool,
            env.id,
            project.id,
            None,
            None,
            None,
            None,
            Some(Some("7d")),
        )
        .await?;
        assert_eq!(env.max_deployment_expiration.as_deref(), Some("7d"));

        // `None` leaves it unchanged.
        let env = update(&pool, env.id, project.id, None, None, None, None, None).await?;
        assert_eq!(env.max_deployment_expiration.as_deref(), Some("7d"));

        // `Some(None)` clears it.
        let env = update(
            &pool,
            env.id,
            project.id,
            None,
            None,
            None,
            None,
            Some(None),
        )
        .await?;
        assert_eq!(env.max_deployment_expiration, None);

        Ok(())
    }

    #[sqlx::test]
    async fn test_invalid_max_deployment_expiration_violates_check_constraint(
        pool: PgPool,
    ) -> Result<()> {
        let user = users::create(&pool, "env-test@example.com").await?;
        let project = projects::create(
            &pool,
            "env-test-project",
            ProjectStatus::Stopped,
            "public".to_string(),
            Some(user.id),
            None,
            None,
        )
        .await?;

        let result = create(
            &pool,
            project.id,
            "staging",
            Some("staging"),
            false,
            "green",
            Some("7w"),
        )
        .await;
        let err = result.unwrap_err();
        assert!(
            format!("{err:#}").contains("valid_max_deployment_expiration"),
            "expected the CHECK constraint violation, got: {err:#}",
        );

        Ok(())
    }

    #[sqlx::test]
    async fn test_create_default_for_project(pool: PgPool) -> Result<()> {
        let user = users::create(&pool, "env-test@example.com").await?;
        let project = projects::create(
            &pool,
            "env-test-project",
            ProjectStatus::Stopped,
            "public".to_string(),
            Some(user.id),
            None,
            None,
        )
        .await?;

        let env = create_default_for_project(&pool, project.id).await?;
        assert_eq!(env.name, "production");
        assert!(env.is_production);
        assert_eq!(env.primary_deployment_group.as_deref(), Some("default"));

        Ok(())
    }

    #[sqlx::test]
    async fn test_unique_production(pool: PgPool) -> Result<()> {
        let user = users::create(&pool, "env-test@example.com").await?;
        let project = projects::create(
            &pool,
            "env-test-project",
            ProjectStatus::Stopped,
            "public".to_string(),
            Some(user.id),
            None,
            None,
        )
        .await?;

        // Create production env
        let prod = create(
            &pool,
            project.id,
            "production",
            Some("default"),
            true,
            "green",
            None,
        )
        .await?;

        // Create staging env
        let _staging = create(
            &pool,
            project.id,
            "staging",
            Some("staging"),
            false,
            "green",
            None,
        )
        .await?;

        // Create a new env with is_production=true — should swap from prod
        let canary = create_with_flag_swap(
            &pool,
            project.id,
            "canary",
            Some("canary"),
            true,
            "yellow",
            None,
        )
        .await?;
        assert!(canary.is_production);

        // Verify prod lost production flag
        let prod = find_by_id(&pool, prod.id).await?.unwrap();
        assert!(!prod.is_production);

        Ok(())
    }

    #[sqlx::test]
    async fn test_create_with_flag_swap(pool: PgPool) -> Result<()> {
        let user = users::create(&pool, "env-test@example.com").await?;
        let project = projects::create(
            &pool,
            "env-test-project",
            ProjectStatus::Stopped,
            "public".to_string(),
            Some(user.id),
            None,
            None,
        )
        .await?;

        // Create initial production env
        let prod = create_default_for_project(&pool, project.id).await?;
        assert!(prod.is_production);

        // Create a new env with is_production=false
        let staging = create_with_flag_swap(
            &pool,
            project.id,
            "staging",
            Some("staging"),
            false,
            "blue",
            None,
        )
        .await?;
        assert!(!staging.is_production);

        // Verify prod still has production flag
        let prod = find_by_id(&pool, prod.id).await?.unwrap();
        assert!(prod.is_production);

        // Create another env with is_production=true — should swap from prod
        let canary = create_with_flag_swap(
            &pool,
            project.id,
            "canary",
            Some("canary"),
            true,
            "yellow",
            None,
        )
        .await?;
        assert!(canary.is_production);

        // Verify prod lost production flag
        let prod = find_by_id(&pool, prod.id).await?.unwrap();
        assert!(!prod.is_production);

        Ok(())
    }

    #[sqlx::test]
    async fn test_cannot_delete_production_environment(pool: PgPool) -> Result<()> {
        let user = users::create(&pool, "env-test@example.com").await?;
        let project = projects::create(
            &pool,
            "env-test-project",
            ProjectStatus::Stopped,
            "public".to_string(),
            Some(user.id),
            None,
            None,
        )
        .await?;

        let env = create_default_for_project(&pool, project.id).await?;

        let result = delete(&pool, env.id);
        assert!(result.await.is_err());

        Ok(())
    }

    #[sqlx::test]
    async fn test_find_by_primary_group(pool: PgPool) -> Result<()> {
        let user = users::create(&pool, "env-test@example.com").await?;
        let project = projects::create(
            &pool,
            "env-test-project",
            ProjectStatus::Stopped,
            "public".to_string(),
            Some(user.id),
            None,
            None,
        )
        .await?;

        create(
            &pool,
            project.id,
            "production",
            Some("default"),
            true,
            "green",
            None,
        )
        .await?;
        create(
            &pool,
            project.id,
            "staging",
            Some("staging"),
            false,
            "green",
            None,
        )
        .await?;

        let found = find_by_primary_group(&pool, project.id, "default").await?;
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "production");

        let found = find_by_primary_group(&pool, project.id, "staging").await?;
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "staging");

        let found = find_by_primary_group(&pool, project.id, "nonexistent").await?;
        assert!(found.is_none());

        Ok(())
    }

    #[sqlx::test]
    async fn test_list_for_project_ordered(pool: PgPool) -> Result<()> {
        let user = users::create(&pool, "env-test@example.com").await?;
        let project = projects::create(
            &pool,
            "env-test-project",
            ProjectStatus::Stopped,
            "public".to_string(),
            Some(user.id),
            None,
            None,
        )
        .await?;

        create(&pool, project.id, "dev", None, false, "green", None).await?;
        create(
            &pool,
            project.id,
            "production",
            Some("default"),
            true,
            "green",
            None,
        )
        .await?;
        create(
            &pool,
            project.id,
            "staging",
            Some("staging"),
            false,
            "green",
            None,
        )
        .await?;

        let envs = list_for_project(&pool, project.id).await?;
        assert_eq!(envs.len(), 3);
        // production first (is_production=true), then dev, then staging (alphabetical)
        assert_eq!(envs[0].name, "production");
        assert_eq!(envs[1].name, "dev");
        assert_eq!(envs[2].name, "staging");

        Ok(())
    }
}
