use anyhow::{Context, Result};
use sqlx::PgPool;
use uuid::Uuid;

use crate::db::models::{DeploymentEnvVar, ProjectEnvVar};

/// List all environment variables for a project.
///
/// If `environment_id` is provided, returns both global vars (environment_id IS NULL) and
/// environment-specific vars for that environment. If not provided, returns all vars.
pub async fn list_project_env_vars(
    pool: &PgPool,
    project_id: Uuid,
    environment_id: Option<Uuid>,
) -> Result<Vec<ProjectEnvVar>> {
    let env_vars = if let Some(env_id) = environment_id {
        sqlx::query_as!(
            ProjectEnvVar,
            r#"
            SELECT id, project_id, key, value, is_secret, is_protected, environment_id, created_at, updated_at
            FROM project_env_vars
            WHERE project_id = $1 AND (environment_id IS NULL OR environment_id = $2)
            ORDER BY key ASC, environment_id NULLS LAST
            "#,
            project_id,
            env_id
        )
        .fetch_all(pool)
        .await
        .context("Failed to list project environment variables")?
    } else {
        sqlx::query_as!(
            ProjectEnvVar,
            r#"
            SELECT id, project_id, key, value, is_secret, is_protected, environment_id, created_at, updated_at
            FROM project_env_vars
            WHERE project_id = $1
            ORDER BY key ASC, environment_id NULLS LAST
            "#,
            project_id
        )
        .fetch_all(pool)
        .await
        .context("Failed to list project environment variables")?
    };

    Ok(env_vars)
}

/// Get a specific project environment variable by key and optional environment.
pub async fn get_project_env_var(
    pool: &PgPool,
    project_id: Uuid,
    key: &str,
    environment_id: Option<Uuid>,
) -> Result<Option<ProjectEnvVar>> {
    let env_var = if let Some(env_id) = environment_id {
        sqlx::query_as!(
            ProjectEnvVar,
            r#"
            SELECT id, project_id, key, value, is_secret, is_protected, environment_id, created_at, updated_at
            FROM project_env_vars
            WHERE project_id = $1 AND key = $2 AND environment_id = $3
            "#,
            project_id,
            key,
            env_id
        )
        .fetch_optional(pool)
        .await
        .context("Failed to get project environment variable")?
    } else {
        sqlx::query_as!(
            ProjectEnvVar,
            r#"
            SELECT id, project_id, key, value, is_secret, is_protected, environment_id, created_at, updated_at
            FROM project_env_vars
            WHERE project_id = $1 AND key = $2 AND environment_id IS NULL
            "#,
            project_id,
            key
        )
        .fetch_optional(pool)
        .await
        .context("Failed to get project environment variable")?
    };

    Ok(env_var)
}

/// Create or update a project environment variable (upsert).
///
/// The unique constraint is on `(project_id, key, COALESCE(environment_id, nil_uuid))`.
pub async fn upsert_project_env_var(
    pool: &PgPool,
    project_id: Uuid,
    key: &str,
    value: &str,
    is_secret: bool,
    is_protected: bool,
    environment_id: Option<Uuid>,
) -> Result<ProjectEnvVar> {
    // We use the COALESCE-based unique index for conflict detection.
    // sqlx doesn't support ON CONFLICT on expressions directly, so we use a
    // two-step approach: try to find existing, then insert or update.
    let existing = get_project_env_var(pool, project_id, key, environment_id).await?;

    let env_var = if let Some(existing) = existing {
        sqlx::query_as!(
            ProjectEnvVar,
            r#"
            UPDATE project_env_vars
            SET value = $2, is_secret = $3, is_protected = $4, updated_at = NOW()
            WHERE id = $1
            RETURNING id, project_id, key, value, is_secret, is_protected, environment_id, created_at, updated_at
            "#,
            existing.id,
            value,
            is_secret,
            is_protected
        )
        .fetch_one(pool)
        .await
        .context("Failed to update project environment variable")?
    } else {
        sqlx::query_as!(
            ProjectEnvVar,
            r#"
            INSERT INTO project_env_vars (project_id, key, value, is_secret, is_protected, environment_id)
            VALUES ($1, $2, $3, $4, $5, $6)
            RETURNING id, project_id, key, value, is_secret, is_protected, environment_id, created_at, updated_at
            "#,
            project_id,
            key,
            value,
            is_secret,
            is_protected,
            environment_id
        )
        .fetch_one(pool)
        .await
        .context("Failed to insert project environment variable")?
    };

    Ok(env_var)
}

/// Update the environment of a project environment variable.
pub async fn update_env_var_environment(
    pool: &PgPool,
    project_id: Uuid,
    key: &str,
    from_environment_id: Option<Uuid>,
    to_environment_id: Option<Uuid>,
) -> Result<ProjectEnvVar> {
    let existing = get_project_env_var(pool, project_id, key, from_environment_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Environment variable not found"))?;

    let env_var = sqlx::query_as!(
        ProjectEnvVar,
        r#"
        UPDATE project_env_vars
        SET environment_id = $2, updated_at = NOW()
        WHERE id = $1
        RETURNING id, project_id, key, value, is_secret, is_protected, environment_id, created_at, updated_at
        "#,
        existing.id,
        to_environment_id
    )
    .fetch_one(pool)
    .await
    .context("Failed to update environment variable environment")?;

    Ok(env_var)
}

/// Delete a project environment variable by key and optional environment.
pub async fn delete_project_env_var(
    pool: &PgPool,
    project_id: Uuid,
    key: &str,
    environment_id: Option<Uuid>,
) -> Result<bool> {
    let result = if let Some(env_id) = environment_id {
        sqlx::query!(
            r#"
            DELETE FROM project_env_vars
            WHERE project_id = $1 AND key = $2 AND environment_id = $3
            "#,
            project_id,
            key,
            env_id
        )
        .execute(pool)
        .await
        .context("Failed to delete project environment variable")?
    } else {
        sqlx::query!(
            r#"
            DELETE FROM project_env_vars
            WHERE project_id = $1 AND key = $2 AND environment_id IS NULL
            "#,
            project_id,
            key
        )
        .execute(pool)
        .await
        .context("Failed to delete project environment variable")?
    };

    Ok(result.rows_affected() > 0)
}

/// Copy project environment variables to a deployment, resolving environment-scoped overrides.
///
/// For each key, if an environment-specific value exists for the target environment, it takes
/// priority over the global value. Global vars (environment_id IS NULL) are used as the base.
///
/// The `source` column tracks provenance: `global` for project-level vars and `env:<name>`
/// for environment-scoped vars that won the DISTINCT ON resolution.
pub async fn copy_project_env_vars_to_deployment<'e, E>(
    executor: E,
    project_id: Uuid,
    deployment_id: Uuid,
    environment_id: Uuid,
    environment_name: &str,
) -> Result<u64>
where
    E: sqlx::PgExecutor<'e>,
{
    let env_source = format!("env:{}", environment_name);
    let result = sqlx::query!(
        r#"
        WITH resolved AS (
            SELECT DISTINCT ON (key) key, value, is_secret, is_protected,
                CASE WHEN environment_id IS NOT NULL THEN $4 ELSE 'global' END as source
            FROM project_env_vars
            WHERE project_id = $2 AND (environment_id IS NULL OR environment_id = $3)
            ORDER BY key, CASE WHEN environment_id IS NOT NULL THEN 0 ELSE 1 END
        )
        INSERT INTO deployment_env_vars (deployment_id, key, value, is_secret, is_protected, source)
        SELECT $1, key, value, is_secret, is_protected, source FROM resolved
        "#,
        deployment_id,
        project_id,
        environment_id,
        env_source
    )
    .execute(executor)
    .await
    .context("Failed to copy project environment variables to deployment")?;

    Ok(result.rows_affected())
}

/// Copy a deployment's environment-variable snapshot onto another
/// deployment, except what the target must get fresh: `system` variables
/// (`PORT`, `RISE_*`) and `extension` variables are derived for the target
/// itself, never copied.
///
/// Rows the target already has win (`ON CONFLICT DO NOTHING`), so this can run
/// after extension variables were materialized for the target.
pub async fn copy_snapshot_env_vars_to_deployment<'e, E>(
    executor: E,
    source_deployment_id: Uuid,
    target_deployment_id: Uuid,
) -> Result<u64>
where
    E: sqlx::PgExecutor<'e>,
{
    let result = sqlx::query!(
        r#"
        INSERT INTO deployment_env_vars (deployment_id, key, value, is_secret, is_protected, source)
        SELECT $1, key, value, is_secret, is_protected, source
        FROM deployment_env_vars
        WHERE deployment_id = $2 AND source NOT IN ('system', 'extension')
        ON CONFLICT (deployment_id, key) DO NOTHING
        "#,
        target_deployment_id,
        source_deployment_id
    )
    .execute(executor)
    .await
    .context("Failed to copy deployment environment variables to deployment")?;

    Ok(result.rows_affected())
}

/// Copy the variables a deployment got from its `rise.toml` and from CLI
/// flags onto another deployment, replacing same-named variables there.
///
/// A deployment created from another one reuses its image and the rest of its
/// configuration; these variables belong to that configuration too, while
/// project variables are read fresh.
///
/// `rise.toml` variables scoped to an environment (`toml:<env>`) are carried
/// only when `<env>` is `target_environment_name`: a promotion leaves the
/// source environment's own settings behind.
pub async fn copy_config_env_vars_to_deployment<'e, E>(
    executor: E,
    source_deployment_id: Uuid,
    target_deployment_id: Uuid,
    target_environment_name: &str,
) -> Result<u64>
where
    E: sqlx::PgExecutor<'e>,
{
    let result = sqlx::query!(
        r#"
        INSERT INTO deployment_env_vars (deployment_id, key, value, is_secret, is_protected, source)
        SELECT $1, key, value, is_secret, is_protected, source
        FROM deployment_env_vars
        WHERE deployment_id = $2
          AND (source IN ('toml', 'cli') OR source = 'toml:' || $3)
        ON CONFLICT (deployment_id, key) DO UPDATE
        SET value = EXCLUDED.value,
            is_secret = EXCLUDED.is_secret,
            is_protected = EXCLUDED.is_protected,
            source = EXCLUDED.source,
            updated_at = NOW()
        "#,
        target_deployment_id,
        source_deployment_id,
        target_environment_name
    )
    .execute(executor)
    .await
    .context("Failed to copy rise.toml and CLI variables to deployment")?;

    Ok(result.rows_affected())
}

/// List all environment variables for a deployment
/// This is used by the controller to get env vars for container injection
pub async fn list_deployment_env_vars(
    pool: &PgPool,
    deployment_id: Uuid,
) -> Result<Vec<DeploymentEnvVar>> {
    let env_vars = sqlx::query_as!(
        DeploymentEnvVar,
        r#"
        SELECT id, deployment_id, key, value, is_secret, is_protected, source, created_at, updated_at
        FROM deployment_env_vars
        WHERE deployment_id = $1
        ORDER BY key ASC
        "#,
        deployment_id
    )
    .fetch_all(pool)
    .await
    .context("Failed to list deployment environment variables")?;

    Ok(env_vars)
}

/// Get a specific deployment environment variable by key
pub async fn get_deployment_env_var(
    pool: &PgPool,
    deployment_id: Uuid,
    key: &str,
) -> Result<Option<DeploymentEnvVar>> {
    let env_var = sqlx::query_as!(
        DeploymentEnvVar,
        r#"
        SELECT id, deployment_id, key, value, is_secret, is_protected, source, created_at, updated_at
        FROM deployment_env_vars
        WHERE deployment_id = $1 AND key = $2
        "#,
        deployment_id,
        key
    )
    .fetch_optional(pool)
    .await
    .context("Failed to get deployment environment variable")?;

    Ok(env_var)
}

/// Create or update a deployment environment variable (upsert)
pub async fn upsert_deployment_env_var<'e, E>(
    executor: E,
    deployment_id: Uuid,
    key: &str,
    value: &str,
    is_secret: bool,
    is_protected: bool,
    source: &str,
) -> Result<DeploymentEnvVar>
where
    E: sqlx::PgExecutor<'e>,
{
    let env_var = sqlx::query_as!(
        DeploymentEnvVar,
        r#"
        INSERT INTO deployment_env_vars (deployment_id, key, value, is_secret, is_protected, source)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (deployment_id, key)
        DO UPDATE SET
            value = EXCLUDED.value,
            is_secret = EXCLUDED.is_secret,
            is_protected = EXCLUDED.is_protected,
            source = EXCLUDED.source,
            updated_at = NOW()
        RETURNING id, deployment_id, key, value, is_secret, is_protected, source, created_at, updated_at
        "#,
        deployment_id,
        key,
        value,
        is_secret,
        is_protected,
        source
    )
    .fetch_one(executor)
    .await
    .context("Failed to upsert deployment environment variable")?;

    Ok(env_var)
}

/// Load deployment environment variables with decryption
///
/// This is a shared helper for controllers to load environment variables
/// with automatic decryption of secrets using the provided encryption provider.
///
/// Returns a vector of (key, value) tuples that can be formatted by the caller
/// as needed (e.g., KEY=VALUE for Docker, EnvVar objects for Kubernetes).
#[cfg(feature = "backend")]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::deployments::{self, CreateDeploymentParams};
    use crate::db::models::{DeploymentStatus, ProjectStatus};
    use crate::server::deployment::models::DEFAULT_DEPLOYMENT_GROUP;

    /// A deployment created from another keeps the source's rise.toml and CLI
    /// variables over the project's, and takes nothing else from it.
    #[sqlx::test]
    async fn config_variables_follow_a_redeploy(pool: PgPool) {
        let user = crate::db::users::create(&pool, "config-vars@example.com")
            .await
            .unwrap();
        let project = crate::db::projects::create(
            &pool,
            "config-vars-project",
            ProjectStatus::Stopped,
            "default".to_string(),
            Some(user.id),
            None,
            None,
        )
        .await
        .unwrap();
        let mut ids = Vec::new();
        for deployment_id in ["20261024-000001", "20261024-000002"] {
            let d = deployments::create(
                &pool,
                CreateDeploymentParams {
                    deployment_id,
                    project_id: project.id,
                    created_by_id: user.id,
                    status: DeploymentStatus::Healthy,
                    image: Some("test:v1"),
                    image_digest: None,
                    rolled_back_from_deployment_id: None,
                    deployment_group: DEFAULT_DEPLOYMENT_GROUP,
                    environment_id: crate::db::environments::test_environment_id(&pool, project.id)
                        .await,
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
                    created_via: crate::db::models::CreatedVia::Deploy,
                    promoted_from_deployment_id: None,
                    promoted_from_environment_id: None,
                    migration_version: None,
                    rollback_guard_for_deployment_id: None,
                    extra_event_attributes: None,
                },
            )
            .await
            .unwrap();
            ids.push(d.id);
        }
        let (source, target) = (ids[0], ids[1]);
        for (key, value, origin) in [
            ("LOG_LEVEL", "debug", "toml"),
            ("FEATURE_X", "on", "cli"),
            ("DATABASE_URL", "from-source", "global"),
            ("PORT", "8080", "system"),
        ] {
            upsert_deployment_env_var(&pool, source, key, value, false, false, origin)
                .await
                .unwrap();
        }
        // The new deployment already has the project's current values.
        upsert_deployment_env_var(&pool, target, "LOG_LEVEL", "info", false, false, "global")
            .await
            .unwrap();
        upsert_deployment_env_var(
            &pool,
            target,
            "DATABASE_URL",
            "current",
            false,
            false,
            "global",
        )
        .await
        .unwrap();

        copy_config_env_vars_to_deployment(&pool, source, target, "production")
            .await
            .unwrap();

        let vars: std::collections::HashMap<String, (String, String)> =
            list_deployment_env_vars(&pool, target)
                .await
                .unwrap()
                .into_iter()
                .map(|v| (v.key, (v.value, v.source)))
                .collect();
        assert_eq!(vars["LOG_LEVEL"], ("debug".into(), "toml".into()));
        assert_eq!(vars["FEATURE_X"], ("on".into(), "cli".into()));
        assert_eq!(vars["DATABASE_URL"], ("current".into(), "global".into()));
        assert!(!vars.contains_key("PORT"));
    }

    #[sqlx::test]
    async fn get_deployment_env_var_returns_matching_variable(pool: PgPool) {
        let user = crate::db::users::create(&pool, "env-vars-test@example.com")
            .await
            .expect("Failed to create test user");

        let project = crate::db::projects::create(
            &pool,
            "env-vars-test-project",
            ProjectStatus::Stopped,
            "default".to_string(),
            Some(user.id),
            None,
            None,
        )
        .await
        .expect("Failed to create test project");

        let deployment = deployments::create(
            &pool,
            CreateDeploymentParams {
                deployment_id: "20260316-120000",
                project_id: project.id,
                created_by_id: user.id,
                status: DeploymentStatus::Healthy,
                image: Some("test:v1"),
                image_digest: None,
                rolled_back_from_deployment_id: None,
                deployment_group: DEFAULT_DEPLOYMENT_GROUP,
                environment_id: crate::db::environments::test_environment_id(&pool, project.id)
                    .await,
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
                created_via: crate::db::models::CreatedVia::Deploy,
                promoted_from_deployment_id: None,
                promoted_from_environment_id: None,
                migration_version: None,
                rollback_guard_for_deployment_id: None,
                extra_event_attributes: None,
            },
        )
        .await
        .expect("Failed to create test deployment");

        upsert_deployment_env_var(
            &pool,
            deployment.id,
            "BAZ",
            "secret-value",
            true,
            false,
            "system",
        )
        .await
        .expect("Failed to insert deployment env var");

        let env_var = get_deployment_env_var(&pool, deployment.id, "BAZ")
            .await
            .expect("Failed to fetch deployment env var")
            .expect("Expected deployment env var to exist");

        assert_eq!(env_var.key, "BAZ");
        assert_eq!(env_var.value, "secret-value");
        assert!(env_var.is_secret);
        assert!(!env_var.is_protected);
    }

    #[sqlx::test]
    async fn env_var_environment_scoping(pool: PgPool) {
        let user = crate::db::users::create(&pool, "env-scope-test@example.com")
            .await
            .unwrap();

        let project = crate::db::projects::create(
            &pool,
            "env-scope-test-project",
            ProjectStatus::Stopped,
            "default".to_string(),
            Some(user.id),
            None,
            None,
        )
        .await
        .unwrap();

        let env = crate::db::environments::create(
            &pool,
            project.id,
            "staging",
            Some("staging"),
            false,
            "green",
            None,
        )
        .await
        .unwrap();

        // Set global var
        upsert_project_env_var(&pool, project.id, "DB_URL", "global-db", false, false, None)
            .await
            .unwrap();

        // Set env-specific var with same key
        upsert_project_env_var(
            &pool,
            project.id,
            "DB_URL",
            "staging-db",
            false,
            false,
            Some(env.id),
        )
        .await
        .unwrap();

        // List with environment filter should show both
        let vars = list_project_env_vars(&pool, project.id, Some(env.id))
            .await
            .unwrap();
        assert_eq!(vars.len(), 2);

        // Copy to deployment should resolve: env-specific wins for DB_URL
        let deployment = deployments::create(
            &pool,
            CreateDeploymentParams {
                deployment_id: "20260424-120000",
                project_id: project.id,
                created_by_id: user.id,
                status: DeploymentStatus::Pending,
                image: None,
                image_digest: None,
                rolled_back_from_deployment_id: None,
                deployment_group: "staging",
                environment_id: env.id,
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
                created_via: crate::db::models::CreatedVia::Deploy,
                promoted_from_deployment_id: None,
                promoted_from_environment_id: None,
                migration_version: None,
                rollback_guard_for_deployment_id: None,
                extra_event_attributes: None,
            },
        )
        .await
        .unwrap();

        copy_project_env_vars_to_deployment(&pool, project.id, deployment.id, env.id, "staging")
            .await
            .unwrap();

        let dep_vars = list_deployment_env_vars(&pool, deployment.id)
            .await
            .unwrap();
        assert_eq!(dep_vars.len(), 1);
        assert_eq!(dep_vars[0].key, "DB_URL");
        assert_eq!(dep_vars[0].value, "staging-db");
        assert_eq!(dep_vars[0].source, "env:staging");
    }

    /// Two deployments of a fresh project in its production environment.
    async fn two_deployments(pool: &PgPool, name: &str) -> (Uuid, Uuid) {
        let user = crate::db::users::create(pool, &format!("{name}@example.com"))
            .await
            .unwrap();
        let project = crate::db::projects::create(
            pool,
            name,
            ProjectStatus::Stopped,
            "default".to_string(),
            Some(user.id),
            None,
            None,
        )
        .await
        .unwrap();
        let environment_id = crate::db::environments::test_environment_id(pool, project.id).await;
        let mut ids = Vec::new();
        for deployment_id in ["20261025-000001", "20261025-000002"] {
            let d = deployments::create(
                pool,
                CreateDeploymentParams {
                    deployment_id,
                    project_id: project.id,
                    created_by_id: user.id,
                    status: DeploymentStatus::Pushed,
                    image: Some("test:v1"),
                    image_digest: None,
                    rolled_back_from_deployment_id: None,
                    deployment_group: DEFAULT_DEPLOYMENT_GROUP,
                    environment_id,
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
                    created_via: crate::db::models::CreatedVia::Deploy,
                    promoted_from_deployment_id: None,
                    promoted_from_environment_id: None,
                    migration_version: None,
                    rollback_guard_for_deployment_id: None,
                    extra_event_attributes: None,
                },
            )
            .await
            .unwrap();
            ids.push(d.id);
        }
        (ids[0], ids[1])
    }

    async fn vars_by_key(
        pool: &PgPool,
        deployment_id: Uuid,
    ) -> std::collections::HashMap<String, (String, String)> {
        list_deployment_env_vars(pool, deployment_id)
            .await
            .unwrap()
            .into_iter()
            .map(|v| (v.key, (v.value, v.source)))
            .collect()
    }

    /// Copying a snapshot onto a deployment whose extension variables are
    /// already written must skip system and extension variables, so it does
    /// not conflict with them on the `(deployment_id, key)` unique constraint.
    #[sqlx::test]
    async fn snapshot_copy_skips_system_and_extension(pool: PgPool) {
        let (source, target) = two_deployments(&pool, "snapshot-copy").await;
        for (key, value, origin) in [
            ("OAUTH_CLIENT_ID", "stale", "extension"),
            ("PORT", "8080", "system"),
            ("RISE_APP_URL", "https://old", "system"),
            ("DATABASE_URL", "from-source", "global"),
            ("LOG_LEVEL", "debug", "toml"),
            ("ONLY_STAGING", "x", "toml:staging"),
            ("FEATURE_X", "on", "cli"),
            ("REGION", "eu", "env:production"),
        ] {
            upsert_deployment_env_var(&pool, source, key, value, false, false, origin)
                .await
                .unwrap();
        }
        upsert_deployment_env_var(
            &pool,
            target,
            "OAUTH_CLIENT_ID",
            "fresh",
            false,
            false,
            "extension",
        )
        .await
        .unwrap();

        copy_snapshot_env_vars_to_deployment(&pool, source, target)
            .await
            .unwrap();

        let vars = vars_by_key(&pool, target).await;
        assert_eq!(
            vars["OAUTH_CLIENT_ID"],
            ("fresh".into(), "extension".into())
        );
        assert!(!vars.contains_key("PORT"));
        assert!(!vars.contains_key("RISE_APP_URL"));
        assert_eq!(
            vars["DATABASE_URL"],
            ("from-source".into(), "global".into())
        );
        assert_eq!(vars["LOG_LEVEL"], ("debug".into(), "toml".into()));
        assert_eq!(vars["ONLY_STAGING"], ("x".into(), "toml:staging".into()));
        assert_eq!(vars["FEATURE_X"], ("on".into(), "cli".into()));
        assert_eq!(vars["REGION"], ("eu".into(), "env:production".into()));
    }

    #[sqlx::test]
    async fn config_copy_carries_only_target_env_scoped_toml(pool: PgPool) {
        let (source, target) = two_deployments(&pool, "config-copy-scoped").await;
        for (key, value, origin) in [
            ("LOG_LEVEL", "debug", "toml"),
            ("STAGING_ONLY", "s", "toml:staging"),
            ("PROD_ONLY", "p", "toml:production"),
            ("FEATURE_X", "on", "cli"),
        ] {
            upsert_deployment_env_var(&pool, source, key, value, false, false, origin)
                .await
                .unwrap();
        }

        copy_config_env_vars_to_deployment(&pool, source, target, "production")
            .await
            .unwrap();

        let vars = vars_by_key(&pool, target).await;
        assert_eq!(vars["LOG_LEVEL"], ("debug".into(), "toml".into()));
        assert_eq!(vars["FEATURE_X"], ("on".into(), "cli".into()));
        assert_eq!(vars["PROD_ONLY"], ("p".into(), "toml:production".into()));
        assert!(!vars.contains_key("STAGING_ONLY"));
    }

    /// The copies run inside a caller's transaction; nothing is visible until
    /// it commits.
    #[sqlx::test]
    async fn copies_run_inside_a_transaction(pool: PgPool) {
        let (source, target) = two_deployments(&pool, "copy-in-tx").await;
        upsert_deployment_env_var(&pool, source, "LOG_LEVEL", "debug", false, false, "toml")
            .await
            .unwrap();

        let mut tx = pool.begin().await.unwrap();
        copy_snapshot_env_vars_to_deployment(&mut *tx, source, target)
            .await
            .unwrap();
        upsert_deployment_env_var(&mut *tx, target, "PORT", "8080", false, false, "system")
            .await
            .unwrap();
        tx.rollback().await.unwrap();

        assert!(vars_by_key(&pool, target).await.is_empty());
    }
}
