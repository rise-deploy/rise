//! Database boundary for the startup credential-context migration.

use crate::db::models::{DeploymentEnvVar, ProjectEnvVar, ProjectExtension};
use anyhow::{ensure, Context, Result};
use sqlx::{Connection, PgConnection, PgPool};
use std::collections::HashMap;
use uuid::Uuid;

pub const PAGE_SIZE: i64 = 256;

pub struct CredentialMigration {
    connection: PgConnection,
    validating: bool,
}

impl CredentialMigration {
    pub async fn begin(pool: &PgPool) -> Result<Self> {
        // Detach keeps the session lock out of the pool. Dropping this guard
        // closes its connection, including every early-return and error path.
        let mut connection = pool.acquire().await?.detach();
        sqlx::query("SET lock_timeout = '5s'")
            .execute(&mut connection)
            .await?;
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(0x72697365656e6332_i64)
            .execute(&mut connection)
            .await
            .context("Credential migration startup lock is busy; retry startup")?;
        Ok(Self {
            connection,
            validating: false,
        })
    }

    pub async fn completed(&mut self) -> Result<bool> {
        Ok(sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM credential_context_migrations WHERE version = 2 AND completed_at IS NOT NULL)",
        )
        .fetch_one(&mut self.connection)
        .await?)
    }

    pub async fn project_variables(&mut self, after: Option<Uuid>) -> Result<Vec<ProjectEnvVar>> {
        let mut query = sqlx::QueryBuilder::new("SELECT * FROM project_env_vars WHERE is_secret");
        if let Some(after) = after {
            query.push(" AND id > ").push_bind(after);
        }
        query.push(" ORDER BY id LIMIT ").push_bind(PAGE_SIZE);
        Ok(query
            .build_query_as()
            .fetch_all(&mut self.connection)
            .await?)
    }

    pub async fn deployment_variables(
        &mut self,
        after: Option<Uuid>,
    ) -> Result<Vec<DeploymentEnvVar>> {
        let mut query =
            sqlx::QueryBuilder::new("SELECT * FROM deployment_env_vars WHERE is_secret");
        if let Some(after) = after {
            query.push(" AND id > ").push_bind(after);
        }
        query.push(" ORDER BY id LIMIT ").push_bind(PAGE_SIZE);
        Ok(query
            .build_query_as()
            .fetch_all(&mut self.connection)
            .await?)
    }

    pub async fn extensions(
        &mut self,
        after: Option<(Uuid, &str)>,
    ) -> Result<Vec<ProjectExtension>> {
        let mut query = sqlx::QueryBuilder::new("SELECT * FROM project_extensions");
        if let Some((project, extension)) = after {
            query
                .push(" WHERE (project_id, extension) > (")
                .push_bind(project)
                .push(", ")
                .push_bind(extension)
                .push(")");
        }
        query
            .push(" ORDER BY project_id, extension LIMIT ")
            .push_bind(PAGE_SIZE);
        Ok(query
            .build_query_as()
            .fetch_all(&mut self.connection)
            .await?)
    }

    pub async fn reference_variable(
        &mut self,
        project: Uuid,
        name: &str,
    ) -> Result<Option<ProjectEnvVar>> {
        Ok(sqlx::query_as("SELECT * FROM project_env_vars WHERE project_id = $1 AND key = $2 AND environment_id IS NULL")
            .bind(project).bind(name).fetch_optional(&mut self.connection).await?)
    }

    pub async fn provider_canary(&mut self) -> Result<Option<String>> {
        Ok(sqlx::query_scalar::<_, Option<String>>(
            "SELECT provider_canary FROM credential_context_migrations WHERE version = 2",
        )
        .fetch_optional(&mut self.connection)
        .await?
        .flatten())
    }

    pub async fn has_progress(&mut self) -> Result<bool> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM credential_context_migration_progress WHERE version = 2)",
        )
        .fetch_one(&mut self.connection)
        .await?)
    }

    pub async fn save_provider_canary(&mut self, ciphertext: &str) -> Result<()> {
        sqlx::query("INSERT INTO credential_context_migrations(version, provider_canary) VALUES (2, $1) ON CONFLICT (version) DO UPDATE SET provider_canary = EXCLUDED.provider_canary")
            .bind(ciphertext).execute(&mut self.connection).await?;
        Ok(())
    }

    pub async fn progressed(&mut self, key: &str, fingerprint: &str) -> Result<bool> {
        Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM credential_context_migration_progress WHERE version = 2 AND record_key = $1 AND fingerprint = $2)")
            .bind(key).bind(fingerprint).fetch_one(&mut self.connection).await?)
    }

    pub async fn checkpoints(&mut self, keys: &[String]) -> Result<HashMap<String, String>> {
        Ok(sqlx::query_as::<_, (String, String)>("SELECT record_key, fingerprint FROM credential_context_migration_progress WHERE version = 2 AND record_key = ANY($1)")
            .bind(keys).fetch_all(&mut self.connection).await?.into_iter().collect())
    }

    pub async fn snowflake_parents(
        &mut self,
        extensions: &[ProjectExtension],
    ) -> Result<HashMap<(Uuid, String), (String, String)>> {
        let projects: Vec<_> = extensions.iter().map(|ext| ext.project_id).collect();
        let names: Vec<_> = extensions.iter().map(|ext| ext.extension.clone()).collect();
        let rows: Vec<(Uuid, String, String, String)> = sqlx::query_as("SELECT child.project_id, child.extension, parent.extension, parent.status ->> 'oauth_client_id' FROM unnest($1::uuid[], $2::text[]) AS child(project_id, extension) CROSS JOIN LATERAL (SELECT extension, status FROM project_extensions WHERE project_id = child.project_id AND extension_type = 'snowflake-oauth-provisioner' AND status ->> 'oauth_extension_name' = child.extension AND jsonb_typeof(status -> 'oauth_extension_name') = 'string' AND jsonb_typeof(status -> 'oauth_client_id') = 'string' ORDER BY extension DESC LIMIT 1) parent")
            .bind(projects).bind(names).fetch_all(&mut self.connection).await?;
        Ok(rows
            .into_iter()
            .map(|(project, child, parent, client)| ((project, child), (parent, client)))
            .collect())
    }

    pub async fn update_project_value(
        &mut self,
        source: &ProjectEnvVar,
        value: &str,
        key: &str,
        fingerprint: &str,
    ) -> Result<()> {
        let mut transaction = self.connection.begin().await?;
        let updated = sqlx::query("UPDATE project_env_vars SET value = $2, updated_at = NOW() WHERE id = $1 AND ROW(project_id, key, value, is_secret, is_protected, environment_id, created_at, updated_at) IS NOT DISTINCT FROM ROW($3, $4, $5, $6, $7, $8, $9, $10)")
            .bind(source.id).bind(value).bind(source.project_id).bind(&source.key)
            .bind(&source.value).bind(source.is_secret).bind(source.is_protected)
            .bind(source.environment_id).bind(source.created_at).bind(source.updated_at)
            .execute(&mut *transaction).await?;
        ensure!(updated.rows_affected() == 1, "Credential migration source changed for {key}; stop concurrent writes and retry startup");
        progress(&mut transaction, key, fingerprint).await?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn update_deployment_value(
        &mut self,
        source: &DeploymentEnvVar,
        value: &str,
        key: &str,
        fingerprint: &str,
    ) -> Result<()> {
        let mut transaction = self.connection.begin().await?;
        let updated = sqlx::query("UPDATE deployment_env_vars SET value = $2, updated_at = NOW() WHERE id = $1 AND ROW(deployment_id, key, value, is_secret, is_protected, source, created_at, updated_at) IS NOT DISTINCT FROM ROW($3, $4, $5, $6, $7, $8, $9, $10)")
            .bind(source.id).bind(value).bind(source.deployment_id).bind(&source.key)
            .bind(&source.value).bind(source.is_secret).bind(source.is_protected)
            .bind(&source.source).bind(source.created_at).bind(source.updated_at)
            .execute(&mut *transaction).await?;
        ensure!(updated.rows_affected() == 1, "Credential migration source changed for {key}; stop concurrent writes and retry startup");
        progress(&mut transaction, key, fingerprint).await?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn update_extension(
        &mut self,
        source: &ProjectExtension,
        ext: &ProjectExtension,
        key: &str,
        fingerprint: &str,
    ) -> Result<()> {
        let mut transaction = self.connection.begin().await?;
        let updated = sqlx::query("UPDATE project_extensions SET spec = $3, status = $4, updated_at = NOW() WHERE project_id = $1 AND extension = $2 AND ROW(extension_type, spec, status, created_at, updated_at, deleted_at) IS NOT DISTINCT FROM ROW($5, $6, $7, $8, $9, $10)")
            .bind(source.project_id).bind(&source.extension).bind(&ext.spec).bind(&ext.status)
            .bind(&source.extension_type).bind(&source.spec).bind(&source.status)
            .bind(source.created_at).bind(source.updated_at).bind(source.deleted_at)
            .execute(&mut *transaction).await?;
        ensure!(updated.rows_affected() == 1, "Credential migration source changed for {key}; stop concurrent writes and retry startup");
        progress(&mut transaction, key, fingerprint).await?;
        transaction.commit().await?;
        Ok(())
    }

    /// Hold writes out of the final bounded-page coverage check and marker commit.
    /// Closing the detached connection rolls back this transaction on every error.
    pub async fn lock_for_validation(&mut self) -> Result<()> {
        sqlx::query("BEGIN").execute(&mut self.connection).await?;
        sqlx::query("SET LOCAL lock_timeout = '5s'")
            .execute(&mut self.connection)
            .await?;
        sqlx::query("LOCK TABLE project_env_vars, deployment_env_vars, project_extensions IN SHARE MODE")
            .execute(&mut self.connection).await
            .context("Credential migration could not exclude concurrent writes; stop concurrent writes and retry startup")?;
        self.validating = true;
        Ok(())
    }

    pub async fn finish(mut self) -> Result<()> {
        ensure!(
            self.validating,
            "Credential migration requires locked coverage validation"
        );
        sqlx::query("DELETE FROM oauth_transient_state")
            .execute(&mut self.connection)
            .await?;
        sqlx::query("INSERT INTO credential_context_migrations(version, completed_at) VALUES (2, NOW()) ON CONFLICT (version) DO UPDATE SET completed_at = NOW(), provider_canary = NULL")
            .execute(&mut self.connection)
            .await?;
        sqlx::query("DELETE FROM credential_context_migration_progress WHERE version = 2")
            .execute(&mut self.connection)
            .await?;
        sqlx::query("COMMIT").execute(&mut self.connection).await?;
        self.connection.close().await?;
        Ok(())
    }
}

async fn progress(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    key: &str,
    fingerprint: &str,
) -> Result<()> {
    sqlx::query("INSERT INTO credential_context_migration_progress(version, record_key, fingerprint) VALUES (2, $1, $2) ON CONFLICT (version, record_key) DO UPDATE SET fingerprint = EXCLUDED.fingerprint")
        .bind(key).bind(fingerprint).execute(&mut **transaction).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{env_vars, extensions, projects, users, ProjectStatus};
    use crate::server::encryption::migration::migrate;
    use crate::server::encryption::providers::local::LocalEncryptionProvider;
    use crate::server::encryption::{EncryptionContext, EncryptionProvider};
    use crate::server::settings::Settings;
    use serde_json::json;

    fn settings() -> Settings {
        serde_json::from_value(json!({
            "server": {"host": "127.0.0.1", "port": 3000, "public_url": "https://rise.example.com", "jwt_signing_secret": "test"},
            "auth": {"issuer": "https://idp.example.com", "client_id": "test", "client_secret": "test"}
        })).unwrap()
    }

    fn provider() -> LocalEncryptionProvider {
        LocalEncryptionProvider::new("AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=").unwrap()
    }

    async fn project(pool: &PgPool) -> crate::db::Project {
        let user = users::create(pool, "crypto@example.com").await.unwrap();
        projects::create(
            pool,
            "crypto-test",
            ProjectStatus::Stopped,
            "default".into(),
            Some(user.id),
            None,
            None,
        )
        .await
        .unwrap()
    }

    fn oauth_spec(ciphertext: &str) -> serde_json::Value {
        json!({"provider_name":"Example", "client_id":"client", "issuer_url":"https://idp.example.com", "authorization_endpoint":"https://idp.example.com/auth", "token_endpoint":"https://idp.example.com/token", "client_secret_encrypted":ciphertext})
    }

    #[sqlx::test]
    async fn paged_migration_resumes_after_committed_page_and_covers_all_tables(pool: PgPool) {
        let provider = provider();
        let project = project(&pool).await;
        let legacy = provider.legacy_ciphertext_for_test("paged-secret");
        let count = PAGE_SIZE + 3;
        let environment = crate::db::environments::test_environment_id(&pool, project.id).await;
        let deployment: Uuid = sqlx::query_scalar("INSERT INTO deployments (deployment_id, project_id, created_by_id, status, environment_id) SELECT 'paged', $1, id, 'Healthy', $2 FROM users LIMIT 1 RETURNING deployments.id")
            .bind(project.id).bind(environment).fetch_one(&pool).await.unwrap();
        sqlx::query("INSERT INTO project_env_vars (id, project_id, key, value, is_secret) SELECT lpad(to_hex(i), 32, '0')::uuid, $1, 'VAR_' || i, $2, true FROM generate_series(1, $3::int) i")
            .bind(project.id).bind(&legacy).bind(count as i32).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO deployment_env_vars (id, deployment_id, key, value, is_secret) SELECT lpad(to_hex(i), 32, '0')::uuid, $1, 'VAR_' || i, $2, true FROM generate_series(1, $3::int) i")
            .bind(deployment).bind(&legacy).bind(count as i32).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO project_extensions (project_id, extension, extension_type, spec, status) SELECT $1, 'ext-' || lpad(i::text, 4, '0'), 'test', '{}', jsonb_build_object('secret_encrypted', $2::text) FROM generate_series(1, $3::int) i")
            .bind(project.id).bind(&legacy).bind(count as i32).execute(&pool).await.unwrap();
        let mut migration = CredentialMigration::begin(&pool).await.unwrap();
        let first = migration.project_variables(None).await.unwrap();
        assert_eq!(first.len(), PAGE_SIZE as usize);
        assert_eq!(
            migration
                .project_variables(first.last().map(|v| v.id))
                .await
                .unwrap()
                .len(),
            3
        );
        let first = migration.deployment_variables(None).await.unwrap();
        assert_eq!(first.len(), PAGE_SIZE as usize);
        assert_eq!(
            migration
                .deployment_variables(first.last().map(|v| v.id))
                .await
                .unwrap()
                .len(),
            3
        );
        let first = migration.extensions(None).await.unwrap();
        assert_eq!(first.len(), PAGE_SIZE as usize);
        let last = first.last().unwrap();
        assert_eq!(
            migration
                .extensions(Some((last.project_id, &last.extension)))
                .await
                .unwrap()
                .len(),
            3
        );
        drop(migration);
        let failed_id = Uuid::from_u128((PAGE_SIZE + 1) as u128);
        sqlx::query("UPDATE project_env_vars SET value = 'invalid' WHERE id = $1")
            .bind(failed_id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(migrate(&pool, Some(&provider), &settings()).await.is_err());
        let progress: i64 =
            sqlx::query_scalar("SELECT count(*) FROM credential_context_migration_progress")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(progress, PAGE_SIZE);
        let first = env_vars::get_project_env_var(&pool, project.id, "VAR_1", None)
            .await
            .unwrap()
            .unwrap()
            .value;
        sqlx::query("UPDATE project_env_vars SET value = $2 WHERE id = $1")
            .bind(failed_id)
            .bind(&legacy)
            .execute(&pool)
            .await
            .unwrap();
        migrate(&pool, Some(&provider), &settings()).await.unwrap();
        for var in env_vars::list_project_env_vars(&pool, project.id, None)
            .await
            .unwrap()
        {
            assert_eq!(
                provider
                    .decrypt(
                        &var.value,
                        &EncryptionContext::project_env(project.id, None, &var.key)
                    )
                    .await
                    .unwrap(),
                "paged-secret"
            );
            if var.key == "VAR_1" {
                assert_eq!(var.value, first);
            }
        }
        for var in env_vars::list_deployment_env_vars(&pool, deployment)
            .await
            .unwrap()
        {
            assert_eq!(
                provider
                    .decrypt(
                        &var.value,
                        &EncryptionContext::deployment_env(deployment, &var.key)
                    )
                    .await
                    .unwrap(),
                "paged-secret"
            );
        }
        let rows: Vec<crate::db::models::ProjectExtension> =
            sqlx::query_as("SELECT * FROM project_extensions")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows.len(), count as usize);
        for ext in rows {
            assert_eq!(
                provider
                    .decrypt(
                        ext.status["secret_encrypted"].as_str().unwrap(),
                        &EncryptionContext::extension(
                            project.id,
                            "test",
                            &ext.extension,
                            "/status/secret_encrypted"
                        )
                    )
                    .await
                    .unwrap(),
                "paged-secret"
            );
        }
        assert!(CredentialMigration::begin(&pool)
            .await
            .unwrap()
            .completed()
            .await
            .unwrap());
    }

    #[sqlx::test]
    async fn missing_provider_cannot_commit_checkpoints_or_prevent_retry(pool: PgPool) {
        let provider = provider();
        let project = project(&pool).await;
        extensions::create(&pool, project.id, "a-empty", "test", &json!({}))
            .await
            .unwrap();
        extensions::create(
            &pool,
            project.id,
            "z-secret",
            "test",
            &json!({"secret_encrypted": provider.legacy_ciphertext_for_test("secret")}),
        )
        .await
        .unwrap();
        assert!(migrate(&pool, None, &settings()).await.is_err());
        let mut migration = CredentialMigration::begin(&pool).await.unwrap();
        assert!(!migration.has_progress().await.unwrap());
        assert!(!migration.completed().await.unwrap());
        drop(migration);
        migrate(&pool, Some(&provider), &settings()).await.unwrap();
        migrate(&pool, None, &settings()).await.unwrap();
    }

    #[sqlx::test]
    async fn migration_resumes_bound_rows_and_never_reopens_baseline_import(pool: PgPool) {
        let provider = provider();
        let project = project(&pool).await;
        let legacy = provider.legacy_ciphertext_for_test("environment-secret");
        let var =
            env_vars::upsert_project_env_var(&pool, project.id, "TOKEN", &legacy, true, true, None)
                .await
                .unwrap();
        let context = EncryptionContext::project_env(project.id, None, "TOKEN");
        let wrong = provider
            .encrypt("oauth-secret", &EncryptionContext::internal("wrong"))
            .await
            .unwrap();
        extensions::create(&pool, project.id, "login", "oauth", &oauth_spec(&wrong))
            .await
            .unwrap();
        assert!(migrate(&pool, Some(&provider), &settings()).await.is_err());
        let rebound = env_vars::get_project_env_var(&pool, project.id, "TOKEN", None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            provider.decrypt(&rebound.value, &context).await.unwrap(),
            "environment-secret"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM credential_context_migrations WHERE completed_at IS NOT NULL"
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            0
        );
        let spec = oauth_spec(&provider.legacy_ciphertext_for_test("oauth-secret"));
        extensions::update_spec(&pool, project.id, "login", &spec)
            .await
            .unwrap();
        migrate(&pool, Some(&provider), &settings()).await.unwrap();
        let retained = env_vars::get_project_env_var(&pool, project.id, "TOKEN", None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            retained.value, rebound.value,
            "resumption authenticates v2 without re-encryption"
        );
        // A tenant can store arbitrary opaque values, but cannot make startup import them again.
        extensions::update_spec(&pool, project.id, "login", &oauth_spec(&legacy))
            .await
            .unwrap();
        migrate(&pool, Some(&provider), &settings()).await.unwrap();
        let untouched = extensions::find_by_project_and_name(&pool, project.id, "login")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(untouched.spec["client_secret_encrypted"], legacy);
        assert!(provider.decrypt(&legacy, &context).await.is_err());
        assert_eq!(retained.id, var.id);
    }

    #[sqlx::test]
    async fn concurrent_startup_serializes_and_migrates_reference_only_at_cutover(pool: PgPool) {
        let provider = provider();
        let project = project(&pool).await;
        let legacy = provider.legacy_ciphertext_for_test("oauth-secret");
        env_vars::upsert_project_env_var(
            &pool,
            project.id,
            "OAUTH_SECRET",
            &legacy,
            true,
            true,
            None,
        )
        .await
        .unwrap();
        let mut spec = oauth_spec("unused");
        spec.as_object_mut()
            .unwrap()
            .remove("client_secret_encrypted");
        spec["client_secret_ref"] = json!("OAUTH_SECRET");
        extensions::create(&pool, project.id, "login", "oauth", &spec)
            .await
            .unwrap();
        let settings = settings();
        let (a, b) = tokio::join!(
            migrate(&pool, Some(&provider), &settings),
            migrate(&pool, Some(&provider), &settings)
        );
        a.unwrap();
        b.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM credential_context_migrations WHERE completed_at IS NOT NULL"
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            1
        );
        let ext = extensions::find_by_project_and_name(&pool, project.id, "login")
            .await
            .unwrap()
            .unwrap();
        assert!(ext.spec.get("client_secret_ref").is_none());
        let spec: crate::server::extensions::providers::oauth::models::OAuthExtensionSpec =
            serde_json::from_value(ext.spec).unwrap();
        let context =
            crate::server::extensions::providers::oauth::provider::oauth_client_secret_context(
                project.id,
                "login",
                &spec,
                spec.token_endpoint.as_deref().unwrap(),
            )
            .unwrap();
        assert_eq!(
            provider
                .decrypt(spec.client_secret_encrypted.as_deref().unwrap(), &context)
                .await
                .unwrap(),
            "oauth-secret"
        );
    }
    #[sqlx::test]
    async fn provider_status_slots_and_generated_snowflake_destinations_are_bound(pool: PgPool) {
        use crate::server::extensions::providers::oauth::models::OAuthExtensionSpec;
        use crate::server::extensions::providers::oauth::provider::oauth_client_secret_context;
        let provider = provider();
        let project = project(&pool).await;
        let legacy = provider.legacy_ciphertext_for_test("provisioned-secret");
        extensions::create(
            &pool,
            project.id,
            "database",
            "aws-rds-provisioner",
            &json!({}),
        )
        .await
        .unwrap();
        extensions::update_status(&pool, project.id, "database", &json!({"master_password_encrypted":legacy,"databases":{"a/b~c":{"password_encrypted":legacy}}})).await.unwrap();
        extensions::create(&pool, project.id, "bucket", "aws-s3-bucket", &json!({}))
            .await
            .unwrap();
        extensions::update_status(
            &pool,
            project.id,
            "bucket",
            &json!({"iam_secret_access_key_encrypted":legacy}),
        )
        .await
        .unwrap();
        extensions::create(
            &pool,
            project.id,
            "warehouse",
            "snowflake-oauth-provisioner",
            &json!({}),
        )
        .await
        .unwrap();
        extensions::update_status(&pool, project.id, "warehouse", &json!({"oauth_extension_name":"oauth-warehouse", "oauth_client_id":"snowflake-client", "oauth_client_secret_encrypted":legacy})).await.unwrap();
        let mut child_spec = oauth_spec(&legacy);
        child_spec["client_id"] = json!("snowflake-client");
        extensions::create(&pool, project.id, "oauth-warehouse", "oauth", &child_spec)
            .await
            .unwrap();
        extensions::create(
            &pool,
            project.id,
            "aaa-parent",
            "snowflake-oauth-provisioner",
            &json!({}),
        )
        .await
        .unwrap();
        extensions::update_status(
            &pool,
            project.id,
            "aaa-parent",
            &json!({"oauth_extension_name":"oauth-warehouse", "oauth_client_id":"ignored-client"}),
        )
        .await
        .unwrap();
        extensions::create(
            &pool,
            project.id,
            "zzz-invalid-parent",
            "snowflake-oauth-provisioner",
            &json!({}),
        )
        .await
        .unwrap();
        extensions::update_status(
            &pool,
            project.id,
            "zzz-invalid-parent",
            &json!({"oauth_extension_name":"oauth-warehouse", "oauth_client_id":123}),
        )
        .await
        .unwrap();
        assert!(migrate(&pool, Some(&provider), &settings()).await.is_err());
        let mut settings = settings();
        settings.extensions = Some(serde_json::from_value(json!({"providers":[{"type":"snowflake-oauth-provisioner", "account":"ignored", "user":"operator", "auth_type":"password", "password":"test"}, {"type":"snowflake-oauth-provisioner", "account":"trusted", "user":"operator", "auth_type":"password", "password":"test"}]})).unwrap());
        migrate(&pool, Some(&provider), &settings).await.unwrap();
        for (name, kind, pointers) in [
            (
                "database",
                "aws-rds-provisioner",
                vec![
                    "/master_password_encrypted",
                    "/databases/a~1b~0c/password_encrypted",
                ],
            ),
            (
                "bucket",
                "aws-s3-bucket",
                vec!["/iam_secret_access_key_encrypted"],
            ),
            (
                "warehouse",
                "snowflake-oauth-provisioner",
                vec!["/oauth_client_secret_encrypted"],
            ),
        ] {
            let ext = extensions::find_by_project_and_name(&pool, project.id, name)
                .await
                .unwrap()
                .unwrap();
            for pointer in pointers {
                let ciphertext = ext.status.pointer(pointer).unwrap().as_str().unwrap();
                let context = EncryptionContext::extension(
                    project.id,
                    kind,
                    name,
                    &format!("/status{pointer}"),
                );
                assert_eq!(
                    provider.decrypt(ciphertext, &context).await.unwrap(),
                    "provisioned-secret"
                );
                assert!(provider
                    .decrypt(
                        ciphertext,
                        &context.with_slot("/spec/client_secret_encrypted")
                    )
                    .await
                    .is_err());
            }
        }
        let ext = extensions::find_by_project_and_name(&pool, project.id, "oauth-warehouse")
            .await
            .unwrap()
            .unwrap();
        let actual: OAuthExtensionSpec = serde_json::from_value(ext.spec).unwrap();
        let mut trusted = actual.clone();
        trusted.issuer_url = "https://trusted.snowflakecomputing.com".into();
        trusted.authorization_endpoint = Some(format!("{}/oauth/authorize", trusted.issuer_url));
        trusted.token_endpoint = Some(format!("{}/oauth/token-request", trusted.issuer_url));
        let ciphertext = actual.client_secret_encrypted.as_deref().unwrap();
        let expected = oauth_client_secret_context(
            project.id,
            "oauth-warehouse",
            &trusted,
            trusted.token_endpoint.as_deref().unwrap(),
        )
        .unwrap();
        assert_eq!(
            provider.decrypt(ciphertext, &expected).await.unwrap(),
            "provisioned-secret"
        );
        let untrusted = oauth_client_secret_context(
            project.id,
            "oauth-warehouse",
            &actual,
            actual.token_endpoint.as_deref().unwrap(),
        )
        .unwrap();
        assert!(provider.decrypt(ciphertext, &untrusted).await.is_err());
    }
    #[sqlx::test]
    async fn locked_validation_preserves_statement_budget_and_releases_writers_on_drop(
        pool: PgPool,
    ) {
        let project = project(&pool).await;
        let mut migration = CredentialMigration::begin(&pool).await.unwrap();
        sqlx::query("SET statement_timeout = '60s'")
            .execute(&mut migration.connection)
            .await
            .unwrap();
        let budget: String = sqlx::query_scalar("SHOW statement_timeout")
            .fetch_one(&mut migration.connection)
            .await
            .unwrap();
        migration.lock_for_validation().await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, String>("SHOW statement_timeout")
                .fetch_one(&mut migration.connection)
                .await
                .unwrap(),
            budget
        );
        let mut writer = Box::pin(sqlx::query("INSERT INTO project_env_vars (project_id, key, value, is_secret, is_protected) VALUES ($1, 'AFTER', 'plain', false, false)")
            .bind(project.id).execute(&pool));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut writer)
                .await
                .is_err()
        );
        drop(migration);
        tokio::time::timeout(std::time::Duration::from_secs(5), writer)
            .await
            .unwrap()
            .unwrap();
        let mut migration = CredentialMigration::begin(&pool).await.unwrap();
        assert!(!migration.completed().await.unwrap());
    }

    struct BlockedProvider {
        inner: LocalEncryptionProvider,
        calls: std::sync::atomic::AtomicUsize,
        block_at: usize,
        blocked: tokio::sync::Notify,
        resume: tokio::sync::Notify,
    }

    impl BlockedProvider {
        fn new(block_at: usize) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                inner: provider(),
                calls: std::sync::atomic::AtomicUsize::new(0),
                block_at,
                blocked: tokio::sync::Notify::new(),
                resume: tokio::sync::Notify::new(),
            })
        }

        async fn wait(&self) {
            tokio::time::timeout(std::time::Duration::from_secs(5), self.blocked.notified())
                .await
                .unwrap();
        }
    }

    #[async_trait::async_trait]
    impl EncryptionProvider for BlockedProvider {
        async fn encrypt(&self, plaintext: &str, context: &EncryptionContext) -> Result<String> {
            self.inner.encrypt(plaintext, context).await
        }
        async fn decrypt(&self, ciphertext: &str, context: &EncryptionContext) -> Result<String> {
            self.inner.decrypt(ciphertext, context).await
        }
        async fn decrypt_legacy_for_migration(&self, ciphertext: &str) -> Result<String> {
            if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1 == self.block_at {
                self.blocked.notify_one();
                self.resume.notified().await;
            }
            self.inner.decrypt_legacy_for_migration(ciphertext).await
        }
    }

    fn migrate_blocked(
        pool: &PgPool,
        provider: &std::sync::Arc<BlockedProvider>,
    ) -> tokio::task::JoinHandle<Result<()>> {
        let pool = pool.clone();
        let provider = provider.clone();
        tokio::spawn(async move { migrate(&pool, Some(provider.as_ref()), &settings()).await })
    }

    async fn assert_conflict_and_retry(
        pool: &PgPool,
        provider: &BlockedProvider,
        worker: tokio::task::JoinHandle<Result<()>>,
        expected: &str,
    ) {
        provider.resume.notify_one();
        let error = tokio::time::timeout(std::time::Duration::from_secs(10), worker)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{error:#}");
        let mut migration = CredentialMigration::begin(pool).await.unwrap();
        assert!(!migration.completed().await.unwrap());
        drop(migration);
        migrate(pool, Some(&provider.inner), &settings())
            .await
            .unwrap();
    }

    #[sqlx::test]
    async fn concurrent_secret_to_plain_conversion_is_not_overwritten(pool: PgPool) {
        let project = project(&pool).await;
        let provider = BlockedProvider::new(1);
        let legacy = provider.inner.legacy_ciphertext_for_test("secret");
        let var =
            env_vars::upsert_project_env_var(&pool, project.id, "TOKEN", &legacy, true, true, None)
                .await
                .unwrap();
        let worker = migrate_blocked(&pool, &provider);
        provider.wait().await;
        sqlx::query(
            "UPDATE project_env_vars SET value = 'ordinary', is_secret = false, is_protected = false WHERE id = $1",
        )
        .bind(var.id)
        .execute(&pool)
        .await
        .unwrap();
        assert_conflict_and_retry(&pool, &provider, worker, "source changed").await;
        let current = env_vars::get_project_env_var(&pool, project.id, "TOKEN", None)
            .await
            .unwrap()
            .unwrap();
        assert!(!current.is_secret);
        assert_eq!(current.value, "ordinary");
    }

    #[sqlx::test]
    async fn concurrent_deployment_variable_edit_is_not_overwritten(pool: PgPool) {
        let project = project(&pool).await;
        let provider = BlockedProvider::new(1);
        let legacy = provider.inner.legacy_ciphertext_for_test("first");
        let replacement = provider.inner.legacy_ciphertext_for_test("edited");
        let environment = crate::db::environments::test_environment_id(&pool, project.id).await;
        let deployment: Uuid = sqlx::query_scalar("INSERT INTO deployments (deployment_id, project_id, created_by_id, status, environment_id) SELECT 'overlap', $1, id, 'Healthy', $2 FROM users LIMIT 1 RETURNING deployments.id")
            .bind(project.id).bind(environment).fetch_one(&pool).await.unwrap();
        sqlx::query("INSERT INTO deployment_env_vars (deployment_id, key, value, is_secret) VALUES ($1, 'TOKEN', $2, true)").bind(deployment).bind(legacy).execute(&pool).await.unwrap();
        let worker = migrate_blocked(&pool, &provider);
        provider.wait().await;
        sqlx::query(
            "UPDATE deployment_env_vars SET key = 'RENAMED', value = $2 WHERE deployment_id = $1",
        )
        .bind(deployment)
        .bind(&replacement)
        .execute(&pool)
        .await
        .unwrap();
        assert_conflict_and_retry(&pool, &provider, worker, "source changed").await;
        let current = env_vars::list_deployment_env_vars(&pool, deployment)
            .await
            .unwrap()
            .remove(0);
        assert_eq!(current.key, "RENAMED");
        assert_eq!(
            provider
                .inner
                .decrypt(
                    &current.value,
                    &EncryptionContext::deployment_env(deployment, "RENAMED")
                )
                .await
                .unwrap(),
            "edited"
        );
    }

    #[sqlx::test]
    async fn concurrent_extension_status_edit_is_not_overwritten(pool: PgPool) {
        let project = project(&pool).await;
        let provider = BlockedProvider::new(1);
        let legacy = provider.inner.legacy_ciphertext_for_test("secret");
        extensions::create(
            &pool,
            project.id,
            "database",
            "test",
            &json!({"secret_encrypted": legacy}),
        )
        .await
        .unwrap();
        let worker = migrate_blocked(&pool, &provider);
        provider.wait().await;
        extensions::update_status(
            &pool,
            project.id,
            "database",
            &json!({"ordinary": "edited"}),
        )
        .await
        .unwrap();
        assert_conflict_and_retry(&pool, &provider, worker, "source changed").await;
        let current = extensions::find_by_project_and_name(&pool, project.id, "database")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current.status, json!({"ordinary": "edited"}));
    }

    async fn change_behind_cursor(pool: &PgPool, insert: bool) {
        let project = project(pool).await;
        let provider = BlockedProvider::new(2);
        let legacy = provider.inner.legacy_ciphertext_for_test("secret");
        sqlx::query("INSERT INTO project_env_vars (id, project_id, key, value, is_secret) SELECT lpad(to_hex(i), 32, '0')::uuid, $1, 'VAR_' || i, $2, true FROM generate_series(1, 2) i")
            .bind(project.id).bind(&legacy).execute(pool).await.unwrap();
        let worker = migrate_blocked(pool, &provider);
        provider.wait().await;
        if insert {
            sqlx::query("INSERT INTO project_env_vars (id, project_id, key, value, is_secret) VALUES ($1, $2, 'BEHIND', $3, true)")
                .bind(Uuid::nil()).bind(project.id).bind(&legacy).execute(pool).await.unwrap();
        } else {
            sqlx::query("UPDATE project_env_vars SET value = $2 WHERE id = $1")
                .bind(Uuid::from_u128(1))
                .bind(&legacy)
                .execute(pool)
                .await
                .unwrap();
        }
        assert_conflict_and_retry(pool, &provider, worker, "coverage changed").await;
        for current in env_vars::list_project_env_vars(pool, project.id, None)
            .await
            .unwrap()
        {
            assert_eq!(
                provider
                    .inner
                    .decrypt(
                        &current.value,
                        &EncryptionContext::project_env(project.id, None, &current.key)
                    )
                    .await
                    .unwrap(),
                "secret"
            );
        }
    }

    #[sqlx::test]
    async fn insert_behind_page_cursor_prevents_completion_and_can_retry(pool: PgPool) {
        change_behind_cursor(&pool, true).await;
    }

    #[sqlx::test]
    async fn edit_behind_page_cursor_prevents_completion_and_can_retry(pool: PgPool) {
        change_behind_cursor(&pool, false).await;
    }

    struct SlowProvider {
        inner: LocalEncryptionProvider,
        decrypt_calls: std::sync::atomic::AtomicUsize,
        stalled: tokio::sync::Notify,
    }

    impl SlowProvider {
        async fn before_decrypt(&self) {
            use std::sync::atomic::Ordering;
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            if self.decrypt_calls.fetch_add(1, Ordering::SeqCst) == 1 {
                self.stalled.notify_one();
                std::future::pending::<()>().await;
            }
        }
    }

    #[async_trait::async_trait]
    impl EncryptionProvider for SlowProvider {
        async fn encrypt(&self, plaintext: &str, context: &EncryptionContext) -> Result<String> {
            self.inner.encrypt(plaintext, context).await
        }
        async fn decrypt(&self, ciphertext: &str, context: &EncryptionContext) -> Result<String> {
            self.before_decrypt().await;
            self.inner.decrypt(ciphertext, context).await
        }
        async fn decrypt_legacy_for_migration(&self, ciphertext: &str) -> Result<String> {
            self.before_decrypt().await;
            self.inner.decrypt_legacy_for_migration(ciphertext).await
        }
    }

    #[sqlx::test]
    async fn interrupted_slow_migration_skips_committed_authentication_on_resume(pool: PgPool) {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let project = project(&pool).await;
        let provider = Arc::new(SlowProvider {
            inner: provider(),
            decrypt_calls: AtomicUsize::new(0),
            stalled: tokio::sync::Notify::new(),
        });
        let first = provider
            .inner
            .encrypt(
                "first",
                &EncryptionContext::project_env(project.id, None, "FIRST"),
            )
            .await
            .unwrap();
        let second = provider.inner.legacy_ciphertext_for_test("second");
        let a =
            env_vars::upsert_project_env_var(&pool, project.id, "FIRST", &first, true, true, None)
                .await
                .unwrap();
        let b = env_vars::upsert_project_env_var(
            &pool, project.id, "SECOND", &second, true, true, None,
        )
        .await
        .unwrap();
        for (id, deterministic) in [(a.id, Uuid::from_u128(1)), (b.id, Uuid::from_u128(2))] {
            sqlx::query("UPDATE project_env_vars SET id = $2 WHERE id = $1")
                .bind(id)
                .bind(deterministic)
                .execute(&pool)
                .await
                .unwrap();
        }
        let worker = tokio::spawn({
            let pool = pool.clone();
            let provider = provider.clone();
            async move { migrate(&pool, Some(provider.as_ref()), &settings()).await }
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            provider.stalled.notified(),
        )
        .await
        .unwrap();
        worker.abort();
        assert!(worker.await.unwrap_err().is_cancelled());
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM credential_context_migration_progress"
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            1
        );
        let wrong_key =
            LocalEncryptionProvider::new("AgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgI=").unwrap();
        assert!(migrate(&pool, Some(&wrong_key), &settings()).await.is_err());
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM credential_context_migrations WHERE completed_at IS NOT NULL"
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            0
        );
        migrate(&pool, Some(provider.as_ref()), &settings())
            .await
            .unwrap();
        assert_eq!(
            provider.decrypt_calls.load(Ordering::SeqCst),
            5,
            "only unfinished rows plus the two constant-cost canary checks may decrypt on resume"
        );
        assert_eq!(
            env_vars::get_project_env_var(&pool, project.id, "FIRST", None)
                .await
                .unwrap()
                .unwrap()
                .value,
            first
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM credential_context_migration_progress"
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            0
        );
    }
    #[sqlx::test]
    async fn explicit_source_key_imports_weak_key_baseline_without_runtime_fallback(pool: PgPool) {
        const ZERO_KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
        const TARGET_KEY: &str = "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=";
        let source = LocalEncryptionProvider::new(ZERO_KEY).unwrap();
        let target =
            LocalEncryptionProvider::new_with_migration_source_key(TARGET_KEY, Some(ZERO_KEY))
                .unwrap();
        let project = project(&pool).await;
        let legacy = source.legacy_ciphertext_for_test("provisioned-secret");
        env_vars::upsert_project_env_var(&pool, project.id, "TOKEN", &legacy, true, true, None)
            .await
            .unwrap();
        extensions::create(
            &pool,
            project.id,
            "database",
            "aws-rds-provisioner",
            &json!({}),
        )
        .await
        .unwrap();
        extensions::update_status(
            &pool,
            project.id,
            "database",
            &json!({"master_password_encrypted":legacy}),
        )
        .await
        .unwrap();
        migrate(&pool, Some(&target), &settings()).await.unwrap();
        let var = env_vars::get_project_env_var(&pool, project.id, "TOKEN", None)
            .await
            .unwrap()
            .unwrap();
        let context = EncryptionContext::project_env(project.id, None, "TOKEN");
        let current_only = LocalEncryptionProvider::new(TARGET_KEY).unwrap();
        assert_eq!(
            current_only.decrypt(&var.value, &context).await.unwrap(),
            "provisioned-secret"
        );
        assert!(source.decrypt(&var.value, &context).await.is_err());
        let source_bound = source.encrypt("not-an-import", &context).await.unwrap();
        assert!(
            target.decrypt(&source_bound, &context).await.is_err(),
            "source key is never an ordinary v2 decryption fallback"
        );
        assert!(target.decrypt(&legacy, &context).await.is_err());
        let ext = extensions::find_by_project_and_name(&pool, project.id, "database")
            .await
            .unwrap()
            .unwrap();
        let status_ciphertext = ext.status["master_password_encrypted"].as_str().unwrap();
        let status_context = EncryptionContext::extension(
            project.id,
            "aws-rds-provisioner",
            "database",
            "/status/master_password_encrypted",
        );
        assert_eq!(
            current_only
                .decrypt(status_ciphertext, &status_context)
                .await
                .unwrap(),
            "provisioned-secret"
        );
        assert!(source
            .decrypt(status_ciphertext, &status_context)
            .await
            .is_err());
        // The source key is removable immediately after successful cutover.
        migrate(&pool, Some(&current_only), &settings())
            .await
            .unwrap();
    }
}
