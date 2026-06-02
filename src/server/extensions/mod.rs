use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub mod registry;

#[cfg(feature = "backend")]
pub mod handlers;
#[cfg(feature = "backend")]
pub mod models;
#[cfg(feature = "backend")]
pub mod providers;
#[cfg(feature = "backend")]
pub mod routes;

/// Represents the value of an environment variable injected by an extension.
///
/// Extensions handle encryption internally (they have access to the encryption provider),
/// so callers receive pre-encrypted values where applicable.
pub enum InjectedEnvVarValue {
    /// Plaintext, non-secret value.
    Plain(String),
    /// Secret but not protected. Carries (decrypted, encrypted) so the caller can write
    /// the pre-encrypted value to DB without re-encrypting.
    Secret {
        decrypted: String,
        encrypted: String,
    },
    /// Protected secret. Carries (decrypted, encrypted). The API serialization layer
    /// MUST mask the decrypted value before returning to callers.
    Protected {
        #[allow(dead_code)]
        decrypted: String,
        encrypted: String,
    },
}

/// An environment variable to be injected into a deployment by an extension.
pub struct InjectedEnvVar {
    pub key: String,
    pub value: InjectedEnvVarValue,
}

/// Extension trait for project resource provisioning
#[async_trait]
pub trait Extension: Send + Sync {
    /// Extension type identifier (constant, used for UI registry lookup)
    /// Examples: "aws-rds-postgres", "aws-s3-bucket"
    /// This should be a constant string that doesn't change based on configuration
    fn extension_type(&self) -> &str;

    /// Human-readable display name for this extension type
    ///
    /// This should be a short, friendly name suitable for display in UI lists.
    /// Examples: "AWS RDS Database", "Google OAuth", "Snowflake OAuth"
    ///
    /// # Returns
    /// A short display name (e.g., "AWS RDS Database")
    fn display_name(&self) -> &str;

    /// Validate extension spec on create/update
    ///
    /// Should check that the spec is valid JSONB and contains required fields.
    async fn validate_spec(&self, spec: &Value) -> Result<()>;

    /// Callback invoked after spec update
    ///
    /// This hook is called after the spec has been successfully updated in the database.
    /// Extensions can use this to:
    /// - Reset verification states when critical fields change
    /// - Update cached configuration
    /// - Trigger reconciliation
    ///
    /// # Arguments
    /// * `old_spec` - The previous spec value
    /// * `new_spec` - The new spec value
    /// * `project_id` - Project UUID
    /// * `extension_name` - Extension instance name
    /// * `db_pool` - Database pool for status updates
    ///
    /// # Returns
    /// Ok(()) if successful, Err if the update should fail
    async fn on_spec_updated(
        &self,
        _old_spec: &Value,
        _new_spec: &Value,
        _project_id: Uuid,
        _extension_name: &str,
        _db_pool: &sqlx::PgPool,
    ) -> Result<()> {
        // Default implementation: no-op
        Ok(())
    }

    /// Start the extension's background reconciliation loop(s).
    ///
    /// This method should spawn a background task (via `tokio::spawn`) that runs
    /// its reconciliation loop under a leader election, exiting and releasing the
    /// lease when `shutdown` is cancelled. Prefer `rise_runtime_sync::with_leader_election`
    /// so the lease release is handled for you.
    ///
    /// Example implementation:
    /// ```ignore
    /// fn start(&self, shutdown: CancellationToken) {
    ///     let me = Arc::clone(self);
    ///     tokio::spawn(async move {
    ///         let _ = with_leader_election(me.db_pool.clone(), "rise-ext-foo", Uuid::new_v4(),
    ///             Duration::from_secs(60), |election| async move {
    ///             loop {
    ///                 tokio::select! {
    ///                     _ = shutdown.cancelled() => break,
    ///                     _ = tokio::time::sleep(Duration::from_secs(5)) => {}
    ///                 }
    ///                 if !election.is_leader() { continue; }
    ///                 me.reconcile().await;
    ///             }
    ///             Ok(())
    ///         }).await;
    ///     });
    /// }
    /// ```
    fn start(&self, shutdown: CancellationToken);

    /// Hook called before deployment creation
    ///
    /// Returns environment variables to inject into the deployment. May have side effects
    /// (e.g., provisioning databases). The caller writes the returned vars to the DB.
    ///
    /// # Arguments
    /// * `project_id` - Project UUID
    /// * `deployment_group` - Deployment group name (e.g., "default", "staging")
    ///
    /// # Returns
    /// Vec of environment variables to inject, or Err if the deployment should fail
    async fn before_deployment(
        &self,
        project_id: Uuid,
        deployment_group: &str,
    ) -> Result<Vec<InjectedEnvVar>>;

    /// Preview environment variables that would be injected for a deployment.
    ///
    /// Pure computation with no side effects. Used by the preview endpoint
    /// to show what env vars `rise run` would receive.
    ///
    /// Default implementation delegates to `before_deployment`.
    async fn preview_env_vars(
        &self,
        project_id: Uuid,
        deployment_group: &str,
    ) -> Result<Vec<InjectedEnvVar>> {
        self.before_deployment(project_id, deployment_group).await
    }

    /// Format the extension status for human-readable display
    ///
    /// This allows each extension provider to control how its status is
    /// displayed in the CLI without the CLI needing to know about provider-specific
    /// status structures.
    ///
    /// # Arguments
    /// * `status` - The status JSONB value from the database
    ///
    /// # Returns
    /// A human-readable string summarizing the extension status
    ///
    /// # Example
    /// For an RDS extension, this might return:
    /// - "Available (db.t4g.micro)"
    /// - "Creating..."
    /// - "Failed: Invalid subnet group"
    fn format_status(&self, status: &Value) -> String;

    /// Get a human-readable description of the extension
    ///
    /// This should be a concise one-line summary of what the extension does.
    ///
    /// # Returns
    /// A short description string (e.g., "Provisions a PostgreSQL database on AWS RDS")
    fn description(&self) -> &str;

    /// Get documentation for the extension
    ///
    /// This should provide comprehensive documentation about:
    /// - What the extension does
    /// - How to configure it
    /// - What environment variables it injects
    /// - Example configurations
    ///
    /// The documentation can be in markdown format.
    ///
    /// # Returns
    /// Documentation string (markdown supported)
    fn documentation(&self) -> &str;

    /// Get the JSON schema or example spec structure
    ///
    /// This should return a JSON Schema or example JSON structure that shows
    /// what fields are valid in the extension spec.
    ///
    /// # Returns
    /// JSON value representing the schema or example spec
    fn spec_schema(&self) -> Value;
}
