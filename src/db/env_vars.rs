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
    value: &str,
    expected_value: &str,
) -> Result<ProjectEnvVar> {
    let existing = get_project_env_var(pool, project_id, key, from_environment_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Environment variable not found"))?;

    let env_var = sqlx::query_as::<_, ProjectEnvVar>(
        "UPDATE project_env_vars SET environment_id = $2, value = $3, updated_at = NOW() WHERE id = $1 AND value = $4 RETURNING *"
    ).bind(existing.id).bind(to_environment_id).bind(value).bind(expected_value)
        .fetch_one(pool).await.context("Failed to update environment variable environment")?;

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

/// Snapshot effective project variables, resealing secrets for the target deployment.
pub async fn copy_project_env_vars_to_deployment(
    pool: &PgPool,
    project_id: Uuid,
    deployment_id: Uuid,
    environment_id: Uuid,
    environment_name: &str,
    provider: Option<&dyn rise_backend_core::EncryptionProvider>,
) -> Result<u64> {
    use rise_backend_core::EncryptionContext;
    let vars = list_project_env_vars(pool, project_id, Some(environment_id)).await?;
    let mut resolved = std::collections::BTreeMap::new();
    for var in vars {
        if !resolved.contains_key(&var.key) || var.environment_id.is_some() {
            resolved.insert(var.key.clone(), var);
        }
    }
    let mut snapshot = Vec::new();
    for var in resolved.into_values() {
        let value = reseal_snapshot(
            provider,
            &var.value,
            var.is_secret,
            &EncryptionContext::project_env(project_id, var.environment_id, &var.key),
            deployment_id,
            &var.key,
        )
        .await?;
        let source = if var.environment_id.is_some() {
            format!("env:{environment_name}")
        } else {
            "global".into()
        };
        snapshot.push(SnapshotValue {
            key: var.key,
            value,
            is_secret: var.is_secret,
            is_protected: var.is_protected,
            source,
        });
    }
    persist_snapshot(pool, deployment_id, snapshot, false).await
}

pub async fn copy_deployment_env_vars_to_deployment(
    pool: &PgPool,
    source_deployment_id: Uuid,
    target_deployment_id: Uuid,
    provider: Option<&dyn rise_backend_core::EncryptionProvider>,
) -> Result<u64> {
    copy_snapshot(
        pool,
        source_deployment_id,
        target_deployment_id,
        false,
        provider,
    )
    .await
}

pub async fn copy_config_env_vars_to_deployment(
    pool: &PgPool,
    source_deployment_id: Uuid,
    target_deployment_id: Uuid,
    provider: Option<&dyn rise_backend_core::EncryptionProvider>,
) -> Result<u64> {
    copy_snapshot(
        pool,
        source_deployment_id,
        target_deployment_id,
        true,
        provider,
    )
    .await
}

async fn copy_snapshot(
    pool: &PgPool,
    source: Uuid,
    target: Uuid,
    config_only: bool,
    provider: Option<&dyn rise_backend_core::EncryptionProvider>,
) -> Result<u64> {
    use rise_backend_core::EncryptionContext;
    let vars = list_deployment_env_vars(pool, source).await?;
    let mut snapshot = Vec::new();
    for var in vars {
        if config_only && !matches!(var.source.as_str(), "toml" | "cli") {
            continue;
        }
        let value = reseal_snapshot(
            provider,
            &var.value,
            var.is_secret,
            &EncryptionContext::deployment_env(source, &var.key),
            target,
            &var.key,
        )
        .await?;
        snapshot.push(SnapshotValue {
            key: var.key,
            value,
            is_secret: var.is_secret,
            is_protected: var.is_protected,
            source: var.source,
        });
    }
    persist_snapshot(pool, target, snapshot, config_only).await
}

struct SnapshotValue {
    key: String,
    value: String,
    is_secret: bool,
    is_protected: bool,
    source: String,
}

const SNAPSHOT_BATCH_SIZE: usize = 256;

async fn persist_snapshot(
    pool: &PgPool,
    target: Uuid,
    snapshot: Vec<SnapshotValue>,
    replace: bool,
) -> Result<u64> {
    let count = snapshot.len() as u64;
    let mut transaction = pool.begin().await?;
    for batch in snapshot.chunks(SNAPSHOT_BATCH_SIZE) {
        let mut query = sqlx::QueryBuilder::new("INSERT INTO deployment_env_vars (deployment_id, key, value, is_secret, is_protected, source) ");
        query.push_values(batch, |mut row, var| {
            row.push_bind(target)
                .push_bind(&var.key)
                .push_bind(&var.value)
                .push_bind(var.is_secret)
                .push_bind(var.is_protected)
                .push_bind(&var.source);
        });
        if replace {
            query.push(" ON CONFLICT (deployment_id, key) DO UPDATE SET value = EXCLUDED.value, is_secret = EXCLUDED.is_secret, is_protected = EXCLUDED.is_protected, source = EXCLUDED.source, updated_at = NOW()");
        }
        query
            .build()
            .execute(&mut *transaction)
            .await
            .context("Failed to persist credential snapshot")?;
    }
    transaction.commit().await?;
    Ok(count)
}

async fn reseal_snapshot(
    provider: Option<&dyn rise_backend_core::EncryptionProvider>,
    value: &str,
    secret: bool,
    source_context: &rise_backend_core::EncryptionContext,
    target: Uuid,
    key: &str,
) -> Result<String> {
    if !secret {
        return Ok(value.into());
    }
    let provider = provider.context("Secret snapshot requires an encryption provider")?;
    let plaintext = provider
        .decrypt(value, source_context)
        .await
        .context("Failed to authenticate snapshot source")?;
    provider
        .encrypt(
            &plaintext,
            &rise_backend_core::EncryptionContext::deployment_env(target, key),
        )
        .await
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
pub async fn upsert_deployment_env_var(
    pool: &PgPool,
    deployment_id: Uuid,
    key: &str,
    value: &str,
    is_secret: bool,
    is_protected: bool,
    source: &str,
) -> Result<DeploymentEnvVar> {
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
    .fetch_one(pool)
    .await
    .context("Failed to upsert deployment environment variable")?;

    Ok(env_var)
}

/// Database regressions for variable snapshots and resource-bound credentials.
#[cfg(feature = "backend")]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::deployments::{self, CreateDeploymentParams};
    use crate::db::models::{DeploymentStatus, ProjectStatus};
    use crate::server::deployment::models::DEFAULT_DEPLOYMENT_GROUP;

    #[sqlx::test]
    async fn snapshot_batches_preserve_upserts_and_rollback_every_batch(pool: PgPool) {
        let user = crate::db::users::create(&pool, "batch@example.com")
            .await
            .unwrap();
        let project = crate::db::projects::create(
            &pool,
            "batch-project",
            ProjectStatus::Stopped,
            "default".into(),
            Some(user.id),
            None,
            None,
        )
        .await
        .unwrap();
        let environment = crate::db::environments::test_environment_id(&pool, project.id).await;
        let target: Uuid = sqlx::query_scalar("INSERT INTO deployments (deployment_id, project_id, created_by_id, status, environment_id) VALUES ('batch', $1, $2, 'Healthy', $3) RETURNING id")
            .bind(project.id).bind(user.id).bind(environment).fetch_one(&pool).await.unwrap();
        let count = SNAPSHOT_BATCH_SIZE * 2 + 3;
        let values = |value: &str| {
            (0..count)
                .map(|i| SnapshotValue {
                    key: format!("KEY_{i}"),
                    value: value.into(),
                    is_secret: true,
                    is_protected: true,
                    source: "cli".into(),
                })
                .collect::<Vec<_>>()
        };
        let mut invalid = values("first");
        invalid.last_mut().unwrap().key = "KEY_0".into();
        assert!(persist_snapshot(&pool, target, invalid, false)
            .await
            .is_err());
        assert!(list_deployment_env_vars(&pool, target)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            persist_snapshot(&pool, target, values("first"), false)
                .await
                .unwrap(),
            count as u64
        );
        let mut invalid = values("second");
        let last = invalid.last_mut().unwrap();
        last.key = "DUPLICATE".into();
        invalid[count - 2].key = "DUPLICATE".into();
        assert!(persist_snapshot(&pool, target, invalid, true)
            .await
            .is_err());
        let rows = list_deployment_env_vars(&pool, target).await.unwrap();
        assert_eq!(rows.len(), count);
        assert!(rows.iter().all(|v| v.value == "first"));
        assert_eq!(
            persist_snapshot(&pool, target, values("second"), true)
                .await
                .unwrap(),
            count as u64
        );
        let rows = list_deployment_env_vars(&pool, target).await.unwrap();
        assert_eq!(rows.len(), count);
        assert!(rows
            .iter()
            .all(|v| v.value == "second" && v.is_protected && v.is_secret && v.source == "cli"));
    }

    /// Configuration snapshots are published only after scoped secret resealing succeeds.
    #[sqlx::test]
    async fn config_variables_reseal_before_deployment_is_published(pool: PgPool) {
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
        for deployment_id in [
            "20261024-000001",
            "20261024-000002",
            "20261024-000003",
            "20261024-000004",
        ] {
            let params = CreateDeploymentParams {
                deployment_id,
                project_id: project.id,
                created_by_id: user.id,
                status: if ids.is_empty() {
                    DeploymentStatus::Healthy
                } else {
                    crate::server::deployment::utils::PREPARING_IMAGE_STATUS
                },
                image: Some("test:v1"),
                image_digest: Some("registry.example/app@sha256:abc"),
                rolled_back_from_deployment_id: ids.first().copied(),
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
            };
            let d = if ids.is_empty() {
                deployments::create(&pool, params).await
            } else {
                deployments::create_preparing(&pool, params).await
            }
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

        use crate::server::encryption::providers::local::LocalEncryptionProvider;
        use crate::server::encryption::{EncryptionContext, EncryptionProvider};
        let provider =
            LocalEncryptionProvider::new("AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=").unwrap();
        let encrypted = provider
            .encrypt(
                "secret",
                &EncryptionContext::deployment_env(source, "TOKEN"),
            )
            .await
            .unwrap();
        upsert_deployment_env_var(&pool, source, "TOKEN", &encrypted, true, true, "cli")
            .await
            .unwrap();

        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };
        struct BlockedReseal {
            inner: LocalEncryptionProvider,
            started: tokio::sync::Notify,
            release: tokio::sync::Notify,
            block: AtomicBool,
            fail: AtomicBool,
        }
        #[async_trait::async_trait]
        impl EncryptionProvider for BlockedReseal {
            async fn encrypt(&self, value: &str, context: &EncryptionContext) -> Result<String> {
                if self.fail.load(Ordering::SeqCst) {
                    anyhow::bail!("Simulated resealing failure");
                }
                if self.block.swap(false, Ordering::SeqCst) {
                    self.started.notify_one();
                    self.release.notified().await;
                }
                self.inner.encrypt(value, context).await
            }
            async fn decrypt(&self, value: &str, context: &EncryptionContext) -> Result<String> {
                self.inner.decrypt(value, context).await
            }
        }
        let blocked = Arc::new(BlockedReseal {
            inner: LocalEncryptionProvider::new("AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=")
                .unwrap(),
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            block: AtomicBool::new(true),
            fail: AtomicBool::new(false),
        });
        let copying = tokio::spawn({
            let pool = pool.clone();
            let blocked = blocked.clone();
            async move {
                copy_config_env_vars_to_deployment(&pool, source, target, Some(blocked.as_ref()))
                    .await
            }
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            blocked.started.notified(),
        )
        .await
        .unwrap();
        let preparing = deployments::find_by_id(&pool, target)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(preparing.status, DeploymentStatus::Pushing);
        assert!(!rise_backend_core::should_have_infrastructure(&preparing));
        assert!(preparing.preparation_incomplete());
        assert!(!rise_backend_core::state_machine::can_create_from(
            &preparing
        ));
        assert!(
            deployments::update_status(&pool, target, DeploymentStatus::Pushed, None)
                .await
                .is_err()
        );
        let runtime_metadata = serde_json::json!({"runtime":"working", (rise_backend_core::models::PREPARATION_INCOMPLETE_KEY): false});
        let preserved = deployments::update_controller_metadata(&pool, target, &runtime_metadata)
            .await
            .unwrap();
        assert!(preserved.preparation_incomplete());
        assert_eq!(
            preserved.controller_metadata[rise_backend_core::models::PREPARATION_INCOMPLETE_KEY],
            true
        );

        assert!(get_deployment_env_var(&pool, target, "TOKEN")
            .await
            .unwrap()
            .is_none());
        blocked.release.notify_one();
        copying.await.unwrap().unwrap();
        crate::server::deployment::utils::publish_prepared_deployment(&pool, target)
            .await
            .unwrap();
        let ready = deployments::find_by_id(&pool, target)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ready.status, DeploymentStatus::Pushed);
        assert!(rise_backend_core::should_have_infrastructure(&ready));
        assert!(!ready.preparation_incomplete());
        let stale =
            deployments::update_controller_metadata(&pool, target, &preparing.controller_metadata)
                .await
                .unwrap();
        assert!(
            !stale.preparation_incomplete(),
            "a stale runtime write cannot reintroduce the marker"
        );

        blocked.fail.store(true, Ordering::SeqCst);
        assert!(
            copy_config_env_vars_to_deployment(&pool, source, ids[2], Some(blocked.as_ref()))
                .await
                .is_err()
        );
        deployments::mark_failed(&pool, ids[2], "Preparation failed")
            .await
            .unwrap()
            .unwrap();
        let failed_preparation = deployments::find_by_id(&pool, ids[2])
            .await
            .unwrap()
            .unwrap();
        assert!(!rise_backend_core::should_have_infrastructure(
            &failed_preparation
        ));
        assert!(failed_preparation.preparation_incomplete());
        assert!(!rise_backend_core::state_machine::can_create_from(
            &failed_preparation
        ));
        assert!(list_deployment_env_vars(&pool, ids[2])
            .await
            .unwrap()
            .is_empty());

        blocked.fail.store(false, Ordering::SeqCst);
        blocked.block.store(true, Ordering::SeqCst);
        let cancelling_id = ids[3];
        let copying = tokio::spawn({
            let pool = pool.clone();
            let blocked = blocked.clone();
            async move {
                copy_config_env_vars_to_deployment(
                    &pool,
                    source,
                    cancelling_id,
                    Some(blocked.as_ref()),
                )
                .await
            }
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            blocked.started.notified(),
        )
        .await
        .unwrap();
        deployments::update_status(&pool, cancelling_id, DeploymentStatus::Cancelling, None)
            .await
            .unwrap();
        blocked.release.notify_one();
        copying.await.unwrap().unwrap();
        assert!(
            crate::server::deployment::utils::publish_prepared_deployment(&pool, cancelling_id)
                .await
                .is_err()
        );
        let cancelled = deployments::mark_cancelled(&pool, cancelling_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cancelled.status, DeploymentStatus::Cancelled);
        assert!(cancelled.preparation_incomplete());
        assert!(!rise_backend_core::state_machine::can_create_from(
            &cancelled
        ));

        let vars: std::collections::HashMap<String, (String, String)> =
            list_deployment_env_vars(&pool, target)
                .await
                .unwrap()
                .into_iter()
                .map(|v| (v.key, (v.value, v.source)))
                .collect();
        let token = &vars["TOKEN"].0;
        assert_ne!(token, &encrypted);
        assert_eq!(
            provider
                .decrypt(token, &EncryptionContext::deployment_env(target, "TOKEN"))
                .await
                .unwrap(),
            "secret"
        );
        assert!(provider
            .decrypt(token, &EncryptionContext::deployment_env(source, "TOKEN"))
            .await
            .is_err());

        assert_eq!(vars["LOG_LEVEL"], ("debug".into(), "toml".into()));
        assert_eq!(vars["FEATURE_X"], ("on".into(), "cli".into()));
        assert_eq!(vars["DATABASE_URL"], ("current".into(), "global".into()));
        assert!(!vars.contains_key("PORT"));
        let prepared_failure = deployments::mark_failed(&pool, target, "Failure after preparation")
            .await
            .unwrap()
            .unwrap();
        assert!(!prepared_failure.preparation_incomplete());
        assert!(rise_backend_core::state_machine::can_create_from(
            &prepared_failure
        ));
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
            },
        )
        .await
        .unwrap();

        copy_project_env_vars_to_deployment(
            &pool,
            project.id,
            deployment.id,
            env.id,
            "staging",
            None,
        )
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
}
