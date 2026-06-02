use crate::db::{extensions as db_extensions, projects as db_projects};
use crate::server::encryption::EncryptionProvider;
use crate::server::extensions::{Extension, InjectedEnvVar};
use crate::server::settings::{PrivateKeySource, SnowflakeAuth};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use rise_runtime_sync::{with_leader_election, LeaderElection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use snowflake_connector_rs::{SnowflakeAuthMethod, SnowflakeClient, SnowflakeClientConfig};

/// User-facing extension spec - minimal configuration
/// Backend connection credentials are configured in config/{RISE_CONFIG_RUN_MODE}.yaml
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SnowflakeOAuthProvisionerSpec {
    /// Additional blocked roles (unioned with backend defaults)
    #[serde(default)]
    pub blocked_roles: Vec<String>,

    /// Additional OAuth scopes (unioned with backend defaults)
    #[serde(default)]
    pub scopes: Vec<String>,
}

/// Extension status tracking Snowflake integration state
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SnowflakeOAuthProvisionerStatus {
    /// Current state in the provisioning lifecycle
    pub state: SnowflakeOAuthState,

    /// Snowflake SECURITY INTEGRATION name
    #[serde(skip_serializing_if = "Option::is_none")]
    pub integration_name: Option<String>,

    /// Name of the created OAuth extension
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth_extension_name: Option<String>,

    /// OAuth client ID from Snowflake
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth_client_id: Option<String>,

    /// Encrypted OAuth client secret (stored in status like RDS master_password_encrypted)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth_client_secret_encrypted: Option<String>,

    /// Redirect URI configured in the integration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<String>,

    /// Last error message
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,

    /// Timestamp when the integration was created
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,

    /// Last time the Snowflake integration was successfully re-verified while
    /// `Available`. Used to throttle the drift check to
    /// `verify_interval_seconds` so the reconciler does not hammer Snowflake.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_verified_at: Option<DateTime<Utc>>,
}

/// State machine for Snowflake OAuth provisioning lifecycle
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "PascalCase")]
pub enum SnowflakeOAuthState {
    #[default]
    Pending,
    TestingConnection,
    CreatingIntegration,
    RetrievingCredentials,
    CreatingOAuthExtension,
    Available,
    Deleting,
    Deleted,
    Failed,
}

/// Effective configuration merging user spec with backend defaults
#[derive(Debug, Clone)]
struct EffectiveConfig {
    blocked_roles: Vec<String>,
    scopes: Vec<String>,
}

/// Configuration for SnowflakeOAuthProvisioner
pub struct SnowflakeOAuthProvisionerConfig {
    pub db_pool: PgPool,
    pub encryption_provider: Arc<dyn EncryptionProvider>,
    pub http_client: reqwest::Client,
    pub api_domain: String,
    pub oauth_provider: Option<Arc<dyn Extension>>,

    // Backend configuration (from config/{RISE_CONFIG_RUN_MODE}.yaml)
    pub account: String,
    pub user: String,
    pub role: Option<String>,
    pub warehouse: Option<String>,
    pub auth: SnowflakeAuth,
    pub integration_name_prefix: String,
    pub default_blocked_roles: Vec<String>,
    pub default_scopes: Vec<String>,
    pub refresh_token_validity_seconds: i64,
    pub verify_interval_seconds: u64,
}

/// Main Snowflake OAuth provisioner implementation
pub struct SnowflakeOAuthProvisioner {
    db_pool: PgPool,
    encryption_provider: Arc<dyn EncryptionProvider>,
    http_client: reqwest::Client,
    api_domain: String,
    oauth_provider: Option<Arc<dyn Extension>>,

    // Backend configuration
    account: String,
    user: String,
    role: Option<String>,
    warehouse: Option<String>,
    auth: SnowflakeAuth,
    integration_name_prefix: String,
    default_blocked_roles: Vec<String>,
    default_scopes: Vec<String>,
    refresh_token_validity_seconds: i64,
    verify_interval_seconds: u64,
}

impl Clone for SnowflakeOAuthProvisioner {
    fn clone(&self) -> Self {
        Self {
            db_pool: self.db_pool.clone(),
            encryption_provider: self.encryption_provider.clone(),
            http_client: self.http_client.clone(),
            api_domain: self.api_domain.clone(),
            oauth_provider: self.oauth_provider.clone(),
            account: self.account.clone(),
            user: self.user.clone(),
            role: self.role.clone(),
            warehouse: self.warehouse.clone(),
            auth: self.auth.clone(),
            integration_name_prefix: self.integration_name_prefix.clone(),
            default_blocked_roles: self.default_blocked_roles.clone(),
            default_scopes: self.default_scopes.clone(),
            refresh_token_validity_seconds: self.refresh_token_validity_seconds,
            verify_interval_seconds: self.verify_interval_seconds,
        }
    }
}

impl SnowflakeOAuthProvisioner {
    /// Escape a Snowflake identifier (table name, integration name, etc.)
    /// Validates allowed characters, normalizes hyphens to underscores, and wraps in double quotes
    fn escape_identifier(identifier: &str) -> Result<String> {
        // Snowflake identifiers: alphanumeric, underscore, dollar sign
        // We're more restrictive for security and normalize hyphens to underscores
        let normalized = identifier.replace('-', "_");
        if !normalized.chars().all(|c| c.is_alphanumeric() || c == '_') {
            return Err(anyhow!(
                "Invalid identifier '{}': only alphanumeric and underscore characters are allowed",
                identifier
            ));
        }

        // Escape internal double quotes and wrap in double quotes
        let escaped = normalized.replace('"', "\"\"");
        Ok(format!("\"{}\"", escaped))
    }

    /// Escape a string literal for use in Snowflake SQL
    /// Doubles single quotes to escape them
    fn escape_string_literal(value: &str) -> String {
        value.replace('\'', "''")
    }

    pub fn new(config: SnowflakeOAuthProvisionerConfig) -> Self {
        Self {
            db_pool: config.db_pool,
            encryption_provider: config.encryption_provider,
            http_client: config.http_client,
            api_domain: config.api_domain,
            oauth_provider: config.oauth_provider,
            account: config.account,
            user: config.user,
            role: config.role,
            warehouse: config.warehouse,
            auth: config.auth,
            integration_name_prefix: config.integration_name_prefix,
            default_blocked_roles: config.default_blocked_roles,
            default_scopes: config.default_scopes,
            refresh_token_validity_seconds: config.refresh_token_validity_seconds,
            verify_interval_seconds: config.verify_interval_seconds,
        }
    }

    /// Validate Snowflake credentials during startup
    /// Returns Ok(()) if credentials are valid, Err if connection fails
    pub async fn validate_credentials(&self) -> Result<()> {
        info!(
            "Validating Snowflake credentials for account: {} (user: {}, role: {:?}, warehouse: {:?})",
            self.account, self.user, self.role, self.warehouse
        );

        // Test the connection and get session info
        let test_query = "SELECT CURRENT_VERSION() as version, CURRENT_ACCOUNT() as account, \
                          CURRENT_USER() as user, CURRENT_ROLE() as role, \
                          CURRENT_SECONDARY_ROLES() as secondary_roles, \
                          CURRENT_WAREHOUSE() as warehouse";

        match self.execute_sql(test_query).await {
            Ok(rows) => {
                // Log connection info for debugging
                if let Some(row) = rows.first() {
                    if let Some(version) = row.get("version").or_else(|| row.get("VERSION")) {
                        info!("Snowflake version: {}", version);
                    }
                    if let Some(account) = row.get("account").or_else(|| row.get("ACCOUNT")) {
                        info!("Snowflake account: {}", account);
                    }
                    if let Some(user) = row.get("user").or_else(|| row.get("USER")) {
                        info!("Connected as user: {}", user);
                    }
                    if let Some(role) = row.get("role").or_else(|| row.get("ROLE")) {
                        info!("Active role: {}", role);
                    }
                    if let Some(secondary_roles) = row
                        .get("secondary_roles")
                        .or_else(|| row.get("SECONDARY_ROLES"))
                    {
                        info!("Secondary roles: {}", secondary_roles);
                    }
                    if let Some(warehouse) = row.get("warehouse").or_else(|| row.get("WAREHOUSE")) {
                        info!("Active warehouse: {}", warehouse);
                    }
                } else {
                    info!("Successfully connected to Snowflake");
                }

                Ok(())
            }
            Err(e) => {
                error!(
                    "Failed to validate Snowflake credentials for account {} (user: {}): {:?}",
                    self.account, self.user, e
                );
                Err(anyhow!(
                    "Snowflake credential validation failed for account '{}' (user: '{}'): {}. \
                     Please verify your Snowflake configuration in config/{{RISE_CONFIG_RUN_MODE}}.yaml. \
                     The user must have CREATE INTEGRATION privilege.",
                    self.account,
                    self.user,
                    e
                ))
            }
        }
    }

    /// Merge user spec with backend defaults (union, not override)
    fn get_effective_config(&self, spec: &SnowflakeOAuthProvisionerSpec) -> EffectiveConfig {
        // Union blocked_roles: backend defaults + user additions (deduplicated)
        let mut blocked_roles = self.default_blocked_roles.clone();
        for role in &spec.blocked_roles {
            if !blocked_roles.contains(role) {
                blocked_roles.push(role.clone());
            }
        }

        // Union scopes: backend defaults + user additions (deduplicated)
        let mut scopes = self.default_scopes.clone();
        for scope in &spec.scopes {
            if !scopes.contains(scope) {
                scopes.push(scope.clone());
            }
        }

        EffectiveConfig {
            blocked_roles,
            scopes,
        }
    }

    /// Get the finalizer name for this extension instance
    fn finalizer_name(&self, extension_name: &str) -> String {
        format!(
            "rise.dev/extension/{}/{}",
            self.extension_type(),
            extension_name
        )
    }

    /// Generate Snowflake integration name: {prefix}_{project_name}_{extension_name}
    fn generate_integration_name(&self, project_name: &str, extension_name: &str) -> String {
        let sanitized_project = project_name.replace('-', "_").to_uppercase();
        let sanitized_extension = extension_name.replace('-', "_").to_uppercase();
        format!(
            "{}_{}_{}",
            self.integration_name_prefix.to_uppercase(),
            sanitized_project,
            sanitized_extension
        )
    }

    /// Generate OAuth extension name: {extension_name}-oauth
    fn generate_oauth_extension_name(&self, extension_name: &str) -> String {
        format!("{}-oauth", extension_name)
    }

    /// Create Snowflake client using configured credentials
    fn create_snowflake_client(&self) -> Result<SnowflakeClient> {
        let auth_method = match &self.auth {
            SnowflakeAuth::Password { password } => SnowflakeAuthMethod::Password(password.clone()),
            SnowflakeAuth::PrivateKey {
                key_source,
                private_key_password,
            } => {
                let private_key_pem = match key_source {
                    PrivateKeySource::Path { private_key_path } => {
                        std::fs::read_to_string(private_key_path)
                            .context("Failed to read private key file")?
                    }
                    PrivateKeySource::Inline { private_key } => private_key.clone(),
                };

                // Detect if key is encrypted or unencrypted based on PEM header
                let is_encrypted = private_key_pem.contains("BEGIN ENCRYPTED PRIVATE KEY");
                let is_unencrypted_pkcs8 = private_key_pem.contains("BEGIN PRIVATE KEY");
                let is_rsa_key = private_key_pem.contains("BEGIN RSA PRIVATE KEY");

                // For unencrypted keys, we need to convert to encrypted PKCS#8 format
                // because the Snowflake connector library only supports encrypted keys
                let password_bytes = if is_encrypted {
                    // Key is already encrypted, use provided password
                    private_key_password
                        .as_ref()
                        .map(|p| p.as_bytes().to_vec())
                        .unwrap_or_default()
                } else if is_unencrypted_pkcs8 || is_rsa_key {
                    // Key is unencrypted - the library doesn't support this
                    // We need to return a clear error
                    return Err(anyhow!(
                        "Unencrypted private keys are not supported by the Snowflake connector. \n\
                         \n\
                         Please encrypt your private key using:\n\
                         openssl pkcs8 -topk8 -v2 aes256 -in unencrypted_key.pem -out encrypted_key.p8\n\
                         \n\
                         Then update your config/{{RISE_CONFIG_RUN_MODE}}.yaml:\n\
                         auth_type: private_key\n\
                         private_key: \"$${{SNOWFLAKE_PRIVATE_KEY}}\"  # encrypted key\n\
                         private_key_password: \"$${{SNOWFLAKE_PRIVATE_KEY_PASSWORD}}\"\n\
                         \n\
                         Alternatively, use password authentication instead of private key."
                    ));
                } else {
                    // Unknown key format
                    return Err(anyhow!(
                        "Unsupported private key format. Expected PEM format with one of:\n\
                         - BEGIN ENCRYPTED PRIVATE KEY (PKCS#8 encrypted)\n\
                         - BEGIN PRIVATE KEY (PKCS#8 unencrypted - not supported, must be encrypted)\n\
                         - BEGIN RSA PRIVATE KEY (PKCS#1 - not supported, must be PKCS#8 encrypted)"
                    ));
                };

                SnowflakeAuthMethod::KeyPair {
                    encrypted_pem: private_key_pem,
                    password: password_bytes,
                }
            }
        };

        // Parse account to extract account locator and cloud region
        // Account format: "account_locator.region" or just "account_locator"
        let account_parts: Vec<&str> = self.account.split('.').collect();
        let account_identifier = account_parts
            .first()
            .ok_or_else(|| anyhow!("Invalid account format"))?
            .to_string();

        let mut config = SnowflakeClientConfig {
            account: account_identifier,
            ..Default::default()
        };

        // Set role if configured
        if let Some(ref role) = self.role {
            config.role = Some(role.clone());
        }

        // Set warehouse if configured
        if let Some(ref warehouse) = self.warehouse {
            config.warehouse = Some(warehouse.clone());
        }

        let client = SnowflakeClient::new(&self.user, auth_method, config).map_err(|e| {
            // Provide helpful error messages for common issues
            let error_str = format!("{:?}", e);
            if error_str.contains("ENCRYPTED PRIVATE KEY") {
                anyhow!(
                    "Failed to create Snowflake client: {}. \n\
                     \n\
                     The snowflake-connector-rs library expects private keys in PKCS#8 encrypted format.\n\
                     \n\
                     If you have an unencrypted private key, you can encrypt it with:\n\
                     openssl pkcs8 -topk8 -v2 aes256 -in rsa_key.p8 -out rsa_key_encrypted.p8\n\
                     \n\
                     Or generate a new encrypted key pair:\n\
                     openssl genrsa 2048 | openssl pkcs8 -topk8 -v2 aes256 -out rsa_key.p8\n\
                     \n\
                     Then configure the encrypted key and password in config/{{RISE_CONFIG_RUN_MODE}}.yaml:\n\
                     auth_type: private_key\n\
                     private_key: \"$${{SNOWFLAKE_PRIVATE_KEY}}\"\n\
                     private_key_password: \"$${{SNOWFLAKE_PRIVATE_KEY_PASSWORD}}\"\n\
                     \n\
                     Alternatively, use password authentication instead.",
                    e
                )
            } else {
                anyhow!("Failed to create Snowflake client: {}", e)
            }
        })?;

        Ok(client)
    }

    /// Execute SQL statement on Snowflake using the configured warehouse.
    async fn execute_sql(&self, sql: &str) -> Result<Vec<Value>> {
        self.execute_sql_inner(sql, true).await
    }

    /// Execute a metadata-only SQL statement on Snowflake without activating a
    /// warehouse. Use for `SHOW`/`DESCRIBE`-style queries that the Snowflake
    /// query engine can answer from metadata alone — avoids waking the
    /// warehouse on the steady-state drift check.
    async fn execute_metadata_sql(&self, sql: &str) -> Result<Vec<Value>> {
        self.execute_sql_inner(sql, false).await
    }

    async fn execute_sql_inner(&self, sql: &str, use_warehouse: bool) -> Result<Vec<Value>> {
        let client = self.create_snowflake_client()?;
        let session = client
            .create_session()
            .await
            .context("Failed to create Snowflake session")?;

        // Only attach a warehouse when the caller's query actually needs one.
        // Metadata-only queries skip this to let the warehouse auto-suspend.
        if use_warehouse {
            if let Some(ref warehouse) = self.warehouse {
                debug!("Setting warehouse for session: {}", warehouse);
                let use_warehouse_sql =
                    format!("USE WAREHOUSE {}", Self::escape_identifier(warehouse)?);
                debug!("Executing: {}", use_warehouse_sql);
                session
                    .query(use_warehouse_sql.as_str())
                    .await
                    .context("Failed to set warehouse for session")?;
                debug!("Warehouse set successfully");
            } else {
                debug!("No warehouse configured - session will have no active warehouse");
            }
        }

        let rows = session
            .query(sql)
            .await
            .context("Failed to execute SQL on Snowflake")?;

        // Convert SnowflakeRow to serde_json::Value
        // Build JSON objects from row data by extracting column values
        let json_rows: Vec<Value> = rows
            .iter()
            .map(|row| {
                let mut obj = serde_json::Map::new();

                // SnowflakeRow has a get() method that returns Option<String>
                // We need to iterate through all columns and build the JSON object
                // The debug format shows: column_indices: {"COLUMN_NAME": index}

                // For now, try to extract values by common column names
                // A better approach would be to iterate column_indices, but we don't have direct access

                // Try common column names (case-insensitive by trying both cases)
                let common_columns = vec![
                    "version",
                    "VERSION",
                    "account",
                    "ACCOUNT",
                    "user",
                    "USER",
                    "role",
                    "ROLE",
                    "secondary_roles",
                    "SECONDARY_ROLES",
                    "warehouse",
                    "WAREHOUSE",
                    "credentials",
                    "CREDENTIALS",
                    "client_id",
                    "CLIENT_ID",
                    "client_secret",
                    "CLIENT_SECRET",
                ];

                for col_name in common_columns {
                    if let Ok(value) = row.get(col_name) {
                        // Convert to lowercase for consistent key naming
                        obj.insert(col_name.to_lowercase(), Value::String(value));
                    }
                }

                if obj.is_empty() {
                    // Debug log if we couldn't extract any values
                    let row_debug = format!("{:?}", row);
                    debug!("Could not extract columns from SnowflakeRow: {}", row_debug);
                }

                Value::Object(obj)
            })
            .collect();

        Ok(json_rows)
    }

    /// Handle Pending state: generate names and add finalizer
    async fn handle_pending(
        &self,
        _spec: &SnowflakeOAuthProvisionerSpec,
        status: &mut SnowflakeOAuthProvisionerStatus,
        project_name: &str,
        project_id: Uuid,
        extension_name: &str,
    ) -> Result<()> {
        info!(
            "Initializing Snowflake OAuth provisioner for project {} (extension: {})",
            project_name, extension_name
        );

        // Generate integration name if not already set
        let integration_name = if let Some(ref existing_name) = status.integration_name {
            existing_name.clone()
        } else {
            self.generate_integration_name(project_name, extension_name)
        };

        // Generate OAuth extension name
        let oauth_extension_name = self.generate_oauth_extension_name(extension_name);

        // Generate redirect URI - must match the format used by Generic OAuth extension
        // Format: {RISE_PUBLIC_URL}/oidc/{project_name}/{oauth_extension_name}/callback
        let redirect_uri = format!(
            "{}/oidc/{}/{}/callback",
            self.api_domain.trim_end_matches('/'),
            project_name,
            oauth_extension_name
        );

        // Update status
        status.integration_name = Some(integration_name.clone());
        status.oauth_extension_name = Some(oauth_extension_name);
        status.redirect_uri = Some(redirect_uri);
        status.created_at = Some(Utc::now());
        status.state = SnowflakeOAuthState::TestingConnection;

        // Add finalizer to prevent project deletion during provisioning
        let finalizer = self.finalizer_name(extension_name);
        if let Err(e) = db_projects::add_finalizer(&self.db_pool, project_id, &finalizer).await {
            error!(
                "Failed to add finalizer '{}' for project {}: {}",
                finalizer, project_name, e
            );
        } else {
            info!(
                "Added finalizer '{}' to project {}",
                finalizer, project_name
            );
        }

        Ok(())
    }

    /// Handle TestingConnection state: verify Snowflake credentials
    async fn handle_testing_connection(
        &self,
        status: &mut SnowflakeOAuthProvisionerStatus,
        project_name: &str,
    ) -> Result<()> {
        info!(
            "Testing Snowflake connection for project {} (account: {})",
            project_name, self.account
        );

        // Test the connection with a simple query
        let test_query = "SELECT CURRENT_VERSION() as version, CURRENT_ACCOUNT() as account";

        match self.execute_sql(test_query).await {
            Ok(rows) => {
                info!(
                    "Successfully connected to Snowflake for project {}",
                    project_name
                );

                // Log Snowflake version info if available (for debugging)
                if let Some(row) = rows.first() {
                    if let Some(version) = row.get("version").or_else(|| row.get("VERSION")) {
                        debug!("Snowflake version: {}", version);
                    }
                }

                status.state = SnowflakeOAuthState::CreatingIntegration;
                status.error = None;
            }
            Err(e) => {
                error!(
                    "Failed to connect to Snowflake for project {}: {:?}",
                    project_name, e
                );
                status.state = SnowflakeOAuthState::Failed;
                status.error = Some(format!(
                    "Connection test failed: {:?}. Please verify Snowflake credentials in backend configuration.",
                    e
                ));
            }
        }

        Ok(())
    }

    /// Handle CreatingIntegration state: create SECURITY INTEGRATION in Snowflake
    async fn handle_creating_integration(
        &self,
        spec: &SnowflakeOAuthProvisionerSpec,
        status: &mut SnowflakeOAuthProvisionerStatus,
        project_name: &str,
    ) -> Result<()> {
        let integration_name = status
            .integration_name
            .as_ref()
            .ok_or_else(|| anyhow!("Integration name not set"))?;
        let redirect_uri = status
            .redirect_uri
            .as_ref()
            .ok_or_else(|| anyhow!("Redirect URI not set"))?;

        info!(
            "Creating Snowflake SECURITY INTEGRATION {} for project {}",
            integration_name, project_name
        );

        // Get effective config (union of backend defaults + user overrides)
        let effective_config = self.get_effective_config(spec);

        // Format blocked roles list for SQL with proper escaping
        let blocked_roles_sql = effective_config
            .blocked_roles
            .iter()
            .map(|r| format!("'{}'", Self::escape_string_literal(r)))
            .collect::<Vec<_>>()
            .join(", ");

        // Create SECURITY INTEGRATION SQL with proper escaping
        let integration_name_escaped = Self::escape_identifier(integration_name)?;
        let redirect_uri_escaped = Self::escape_string_literal(redirect_uri);

        let sql = format!(
            r#"CREATE SECURITY INTEGRATION {integration_name}
  TYPE = OAUTH
  ENABLED = TRUE
  OAUTH_CLIENT = CUSTOM
  OAUTH_CLIENT_TYPE = 'CONFIDENTIAL'
  OAUTH_REDIRECT_URI = '{redirect_uri}'
  OAUTH_ALLOW_NON_TLS_REDIRECT_URI = TRUE
  OAUTH_ISSUE_REFRESH_TOKENS = TRUE
  OAUTH_REFRESH_TOKEN_VALIDITY = {refresh_token_validity}
  OAUTH_ENFORCE_PKCE = TRUE
  OAUTH_USE_SECONDARY_ROLES = IMPLICIT
  BLOCKED_ROLES_LIST = ({blocked_roles})"#,
            integration_name = integration_name_escaped,
            redirect_uri = redirect_uri_escaped,
            refresh_token_validity = self.refresh_token_validity_seconds,
            blocked_roles = blocked_roles_sql
        );

        match self.execute_sql(&sql).await {
            Ok(_) => {
                info!(
                    "Successfully created SECURITY INTEGRATION {}",
                    integration_name
                );
                status.state = SnowflakeOAuthState::RetrievingCredentials;
                status.error = None;
            }
            Err(e) => {
                error!(
                    "Failed to create SECURITY INTEGRATION {}: {:?}",
                    integration_name, e
                );
                status.state = SnowflakeOAuthState::Failed;
                status.error = Some(format!("Failed to create integration: {:?}", e));
            }
        }

        Ok(())
    }

    /// Handle RetrievingCredentials state: get OAuth client credentials from Snowflake
    async fn handle_retrieving_credentials(
        &self,
        status: &mut SnowflakeOAuthProvisionerStatus,
        project_name: &str,
    ) -> Result<()> {
        let integration_name = status
            .integration_name
            .as_ref()
            .ok_or_else(|| anyhow!("Integration name not set"))?;

        info!(
            "Retrieving OAuth credentials for integration {} (project: {})",
            integration_name, project_name
        );

        // Query for OAuth credentials with proper escaping
        // Extract specific fields directly in SQL using JSON_EXTRACT_PATH_TEXT
        let integration_name_escaped = Self::escape_string_literal(integration_name);
        let sql = format!(
            r#"SELECT
                JSON_EXTRACT_PATH_TEXT(SYSTEM$SHOW_OAUTH_CLIENT_SECRETS('{}'), 'OAUTH_CLIENT_ID') as client_id,
                JSON_EXTRACT_PATH_TEXT(SYSTEM$SHOW_OAUTH_CLIENT_SECRETS('{}'), 'OAUTH_CLIENT_SECRET') as client_secret"#,
            integration_name_escaped, integration_name_escaped
        );

        match self.execute_sql(&sql).await {
            Ok(rows) => {
                if let Some(row) = rows.first() {
                    // Extract client_id and client_secret directly from the row
                    // Our execute_sql() returns JSON Values with lowercase column names
                    let client_id = row
                        .get("client_id")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| anyhow!("client_id field not found in response"))?
                        .to_string();

                    let client_secret = row
                        .get("client_secret")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| anyhow!("client_secret field not found in response"))?
                        .to_string();

                    // Encrypt client secret
                    let client_secret_encrypted = self
                        .encryption_provider
                        .encrypt(&client_secret)
                        .await
                        .context("Failed to encrypt client secret")?;

                    info!(
                        "Successfully retrieved OAuth credentials for integration {}",
                        integration_name
                    );

                    // Update status
                    status.oauth_client_id = Some(client_id);
                    status.oauth_client_secret_encrypted = Some(client_secret_encrypted);
                    status.state = SnowflakeOAuthState::CreatingOAuthExtension;
                    status.error = None;

                    info!(
                        "Successfully retrieved credentials for integration {}",
                        integration_name
                    );
                } else {
                    let error_msg = "No credentials returned from Snowflake";
                    error!("{}", error_msg);
                    status.state = SnowflakeOAuthState::Failed;
                    status.error = Some(error_msg.to_string());
                }
            }
            Err(e) => {
                error!(
                    "Failed to retrieve credentials for integration {}: {:?}",
                    integration_name, e
                );
                status.state = SnowflakeOAuthState::Failed;
                status.error = Some(format!("Failed to retrieve credentials: {:?}", e));
            }
        }

        Ok(())
    }

    /// Handle CreatingOAuthExtension state: create Generic OAuth extension
    async fn handle_creating_oauth_extension(
        &self,
        spec: &SnowflakeOAuthProvisionerSpec,
        status: &mut SnowflakeOAuthProvisionerStatus,
        project_id: Uuid,
        project_name: &str,
        _extension_name: &str,
    ) -> Result<()> {
        let oauth_extension_name = status
            .oauth_extension_name
            .as_ref()
            .ok_or_else(|| anyhow!("OAuth extension name not set"))?;

        info!(
            "Creating OAuth extension {} for project {}",
            oauth_extension_name, project_name
        );

        // Check if OAuth extension already exists (and is not marked for deletion)
        if let Ok(Some(existing_ext)) =
            db_extensions::find_by_project_and_name(&self.db_pool, project_id, oauth_extension_name)
                .await
        {
            if existing_ext.deleted_at.is_none() {
                info!(
                    "OAuth extension {} already exists, skipping creation",
                    oauth_extension_name
                );
                status.state = SnowflakeOAuthState::Available;
                status.last_verified_at = Some(Utc::now());
                return Ok(());
            } else {
                // Extension exists but is marked for deletion
                // Wait for it to be fully deleted before recreating
                info!(
                    "OAuth extension {} is marked for deletion, waiting for cleanup",
                    oauth_extension_name
                );
                // Stay in CreatingOAuthExtension state and try again next reconciliation
                return Ok(());
            }
        }

        // Get the encrypted client secret from status to store directly in OAuth spec
        let client_secret_encrypted = status
            .oauth_client_secret_encrypted
            .as_ref()
            .ok_or_else(|| anyhow!("Client secret not set"))?
            .clone();

        // Get effective config for scopes
        let effective_config = self.get_effective_config(spec);

        // Create OAuth extension spec
        let oauth_spec = crate::server::extensions::providers::oauth::models::OAuthExtensionSpec {
            provider_name: format!("Snowflake ({})", project_name),
            description: format!("Auto-provisioned Snowflake OAuth for {}", project_name),
            client_id: status
                .oauth_client_id
                .as_ref()
                .ok_or_else(|| anyhow!("Client ID not set"))?
                .clone(),
            client_secret_ref: None,
            client_secret_encrypted: Some(client_secret_encrypted),
            // Snowflake doesn't support OIDC discovery, so we set the issuer_url to the Snowflake base
            // and explicitly provide the authorization and token endpoints
            issuer_url: format!("https://{}.snowflakecomputing.com", self.account),
            authorization_endpoint: Some(format!(
                "https://{}.snowflakecomputing.com/oauth/authorize",
                self.account
            )),
            token_endpoint: Some(format!(
                "https://{}.snowflakecomputing.com/oauth/token-request",
                self.account
            )),
            scopes: effective_config.scopes,
        };

        // Create OAuth extension
        db_extensions::create(
            &self.db_pool,
            project_id,
            oauth_extension_name,
            "oauth",
            &serde_json::to_value(&oauth_spec).context("Failed to serialize OAuth spec")?,
        )
        .await
        .context("Failed to create OAuth extension")?;

        info!(
            "Created OAuth extension {} for project {}",
            oauth_extension_name, project_name
        );

        // Initialize OAuth provider if available
        if let Some(oauth_provider) = &self.oauth_provider {
            let oauth_spec_value =
                serde_json::to_value(&oauth_spec).context("Failed to serialize OAuth spec")?;
            oauth_provider
                .on_spec_updated(
                    &serde_json::json!({}),
                    &oauth_spec_value,
                    project_id,
                    oauth_extension_name,
                    &self.db_pool,
                )
                .await
                .context("Failed to initialize OAuth provider")?;

            info!(
                "Initialized OAuth provider for extension {}",
                oauth_extension_name
            );
        }

        status.state = SnowflakeOAuthState::Available;
        status.error = None;
        status.last_verified_at = Some(Utc::now());

        Ok(())
    }

    /// Verify integration is still available and update if configuration changed
    async fn verify_integration_available(
        &self,
        status: &mut SnowflakeOAuthProvisionerStatus,
        project_id: Uuid,
        project_name: &str,
        _extension_name: &str,
        _spec: &SnowflakeOAuthProvisionerSpec,
    ) -> Result<()> {
        let integration_name = status
            .integration_name
            .as_ref()
            .ok_or_else(|| anyhow!("Integration name not set"))?;

        // Check if integration still exists with proper escaping.
        // SHOW INTEGRATIONS is metadata-only, so we use the no-warehouse path
        // — keeps the warehouse suspended during steady-state drift checks.
        let integration_name_escaped = Self::escape_string_literal(integration_name);
        let sql = format!("SHOW INTEGRATIONS LIKE '{}'", integration_name_escaped);

        match self.execute_metadata_sql(&sql).await {
            Ok(rows) => {
                if rows.is_empty() {
                    warn!(
                        "Integration {} not found for project {}, marking as failed",
                        integration_name, project_name
                    );
                    status.state = SnowflakeOAuthState::Failed;
                    status.error = Some("Integration no longer exists in Snowflake".to_string());
                    return Ok(());
                }
            }
            Err(e) => {
                warn!(
                    "Failed to verify integration {} for project {}: {:?}",
                    integration_name, project_name, e
                );
                // Don't mark as failed on verification errors, just log
                return Ok(());
            }
        }

        // Check if OAuth extension still exists (it might have been deleted by user)
        let oauth_extension_name = status
            .oauth_extension_name
            .as_ref()
            .ok_or_else(|| anyhow!("OAuth extension name not set"))?;

        let oauth_ext = db_extensions::find_by_project_and_name(
            &self.db_pool,
            project_id,
            oauth_extension_name,
        )
        .await?;

        // If OAuth extension was deleted, recreate it
        if oauth_ext.is_none() || oauth_ext.as_ref().unwrap().deleted_at.is_some() {
            warn!(
                "OAuth extension {} was deleted for project {}, recreating it",
                oauth_extension_name, project_name
            );

            // Transition back to CreatingOAuthExtension state to recreate it
            status.state = SnowflakeOAuthState::CreatingOAuthExtension;
            status.error = None;
            return Ok(());
        }

        let expected_redirect_uri = format!(
            "{}/oidc/{}/{}/callback",
            self.api_domain.trim_end_matches('/'),
            project_name,
            oauth_extension_name
        );

        let current_redirect_uri = status.redirect_uri.as_deref().unwrap_or("");

        // Check if redirect URI changed (e.g., api_domain config changed or bug fix)
        if expected_redirect_uri != current_redirect_uri {
            info!(
                "Redirect URI changed for project {}: {} -> {}. Updating SECURITY INTEGRATION.",
                project_name, current_redirect_uri, expected_redirect_uri
            );

            // Update the SECURITY INTEGRATION with new redirect URI
            let integration_name_escaped = Self::escape_identifier(integration_name)?;
            let redirect_uri_escaped = Self::escape_string_literal(&expected_redirect_uri);

            let sql = format!(
                "ALTER SECURITY INTEGRATION {} SET OAUTH_REDIRECT_URI = '{}'",
                integration_name_escaped, redirect_uri_escaped
            );

            self.execute_sql(&sql)
                .await
                .context("Failed to update SECURITY INTEGRATION redirect URI")?;

            // Update status with new redirect URI
            status.redirect_uri = Some(expected_redirect_uri.clone());

            info!(
                "Updated redirect URI for integration {} in project {}",
                integration_name, project_name
            );

            // Also notify the Generic OAuth extension to update its status
            // This ensures the OAuth extension's status.redirect_uri is updated
            if let Some(oauth_provider) = &self.oauth_provider {
                // Read current OAuth extension spec to pass to on_spec_updated
                let oauth_ext = db_extensions::find_by_project_and_name(
                    &self.db_pool,
                    project_id,
                    oauth_extension_name,
                )
                .await?
                .ok_or_else(|| anyhow!("OAuth extension not found"))?;

                oauth_provider
                    .on_spec_updated(
                        &oauth_ext.spec,
                        &oauth_ext.spec,
                        project_id,
                        oauth_extension_name,
                        &self.db_pool,
                    )
                    .await
                    .context("Failed to update OAuth extension after redirect URI change")?;
            }
        }

        Ok(())
    }

    /// Handle deletion: cleanup all resources
    async fn handle_deletion(
        &self,
        status: &mut SnowflakeOAuthProvisionerStatus,
        project_id: Uuid,
        project_name: &str,
        extension_name: &str,
    ) -> Result<()> {
        info!(
            "Deleting Snowflake OAuth resources for project {} (extension: {})",
            project_name, extension_name
        );

        // 1. Drop Snowflake integration (best effort)
        if let Some(integration_name) = &status.integration_name {
            let integration_name_escaped = Self::escape_identifier(integration_name)?;
            let sql = format!("DROP INTEGRATION IF EXISTS {}", integration_name_escaped);
            match self.execute_sql(&sql).await {
                Ok(_) => {
                    info!("Dropped Snowflake integration {}", integration_name);
                }
                Err(e) => {
                    warn!("Failed to drop integration {}: {:?}", integration_name, e);
                }
            }
        }

        // 2. Delete OAuth extension (marks for deletion, OAuth provider handles cleanup)
        if let Some(oauth_ext_name) = &status.oauth_extension_name {
            if let Err(e) =
                db_extensions::mark_deleted(&self.db_pool, project_id, oauth_ext_name).await
            {
                warn!(
                    "Failed to mark OAuth extension {} for deletion: {:?}",
                    oauth_ext_name, e
                );
            } else {
                info!("Marked OAuth extension {} for deletion", oauth_ext_name);
            }
        }

        // 3. Remove finalizer
        let finalizer = self.finalizer_name(extension_name);
        if let Err(e) = db_projects::remove_finalizer(&self.db_pool, project_id, &finalizer).await {
            error!(
                "Failed to remove finalizer '{}' from project {}: {}",
                finalizer, project_name, e
            );
        } else {
            info!(
                "Removed finalizer '{}' from project {}",
                finalizer, project_name
            );
        }

        status.state = SnowflakeOAuthState::Deleted;
        Ok(())
    }

    /// Reconcile a single Snowflake OAuth extension
    async fn reconcile_single(
        &self,
        project_extension: crate::db::models::ProjectExtension,
        election: &LeaderElection,
    ) -> Result<bool> {
        debug!(
            "Reconciling Snowflake OAuth extension: {:?}",
            project_extension
        );

        let project = db_projects::find_by_id(&self.db_pool, project_extension.project_id)
            .await?
            .ok_or_else(|| anyhow!("Project not found"))?;

        // Parse spec
        let spec: SnowflakeOAuthProvisionerSpec =
            serde_json::from_value(project_extension.spec.clone())
                .context("Failed to parse Snowflake OAuth provisioner spec")?;

        // Parse current status or create default
        let mut status: SnowflakeOAuthProvisionerStatus =
            serde_json::from_value(project_extension.status.clone()).unwrap_or_default();

        // TODO: Implement spec change detection to automatically retry failed extensions
        // For now, user must delete and recreate the extension to retry after fixing issues

        // Check if marked for deletion
        if project_extension.deleted_at.is_some() {
            if status.state != SnowflakeOAuthState::Deleted {
                self.handle_deletion(
                    &mut status,
                    project_extension.project_id,
                    &project.name,
                    &project_extension.extension,
                )
                .await?;

                // Update status
                election.assert_leader().await?;
                db_extensions::update_status(
                    &self.db_pool,
                    project_extension.project_id,
                    &project_extension.extension,
                    &serde_json::to_value(&status)?,
                )
                .await?;

                // Hard delete if deletion is complete
                if status.state == SnowflakeOAuthState::Deleted {
                    election.assert_leader().await?;
                    db_extensions::delete_permanently(
                        &self.db_pool,
                        project_extension.project_id,
                        &project_extension.extension,
                    )
                    .await?;
                    info!(
                        "Permanently deleted extension record for project {}",
                        project.name
                    );
                }
            }
            return Ok(false); // Deletion complete
        }

        // Track initial state for change detection
        let initial_state = status.state.clone();

        // Handle normal lifecycle
        match status.state {
            SnowflakeOAuthState::Pending => {
                self.handle_pending(
                    &spec,
                    &mut status,
                    &project.name,
                    project.id,
                    &project_extension.extension,
                )
                .await?;
            }
            SnowflakeOAuthState::TestingConnection => {
                self.handle_testing_connection(&mut status, &project.name)
                    .await?;
            }
            SnowflakeOAuthState::CreatingIntegration => {
                self.handle_creating_integration(&spec, &mut status, &project.name)
                    .await?;
            }
            SnowflakeOAuthState::RetrievingCredentials => {
                self.handle_retrieving_credentials(&mut status, &project.name)
                    .await?;
            }
            SnowflakeOAuthState::CreatingOAuthExtension => {
                self.handle_creating_oauth_extension(
                    &spec,
                    &mut status,
                    project.id,
                    &project.name,
                    &project_extension.extension,
                )
                .await?;
            }
            SnowflakeOAuthState::Available => {
                // Throttle the drift check: once Available, we only re-verify
                // against Snowflake every `verify_interval_seconds`. Otherwise
                // the inner 5s tick would issue `SHOW INTEGRATIONS` constantly
                // and (via the warehouse session) prevent auto-suspend.
                //
                // Compare elapsed in seconds (u64) to avoid any cast/overflow
                // when building a `chrono::Duration` from the configured value.
                let due = status.last_verified_at.is_none_or(|last| {
                    let elapsed_secs = (Utc::now() - last).num_seconds();
                    elapsed_secs >= 0 && (elapsed_secs as u64) >= self.verify_interval_seconds
                });
                if !due {
                    return Ok(false);
                }

                self.verify_integration_available(
                    &mut status,
                    project.id,
                    &project.name,
                    &project_extension.extension,
                    &spec,
                )
                .await?;

                // Stamp regardless of the inner result: transient Snowflake
                // errors are intentionally swallowed inside the helper, and we
                // don't want a flaky `SHOW INTEGRATIONS` to defeat the throttle.
                status.last_verified_at = Some(Utc::now());
            }
            SnowflakeOAuthState::Failed => {
                // Stay in Failed state - don't retry automatically
                // User must fix the configuration and update the extension to retry
                // Error was already logged when transitioning to Failed state
                // No state change - keep in Failed state
            }
            _ => {}
        }

        // Update status in database
        election.assert_leader().await?;
        db_extensions::update_status(
            &self.db_pool,
            project_extension.project_id,
            &project_extension.extension,
            &serde_json::to_value(&status)?,
        )
        .await?;

        // Determine if more work can be done immediately
        let state_changed = status.state != initial_state;

        Ok(state_changed)
    }
}

#[async_trait]
impl Extension for SnowflakeOAuthProvisioner {
    fn extension_type(&self) -> &str {
        "snowflake-oauth-provisioner"
    }

    fn display_name(&self) -> &str {
        "Snowflake OAuth"
    }

    fn description(&self) -> &str {
        "Provisions Snowflake SECURITY INTEGRATIONs and configures Generic OAuth extensions for Snowflake authentication"
    }

    fn documentation(&self) -> &str {
        r#"# Snowflake OAuth Provisioner

Automatically provisions Snowflake SECURITY INTEGRATIONs and creates Generic OAuth extensions for end-user authentication.

## Configuration

Backend credentials are configured in `config/{RISE_CONFIG_RUN_MODE}.yaml`:

```yaml
extensions:
  providers:
  - type: snowflake-oauth-provisioner
    account: "myorg.us-east-1"
    user: "admin_user"
    auth_type: password
    password: "${SNOWFLAKE_PASSWORD}"
    integration_name_prefix: "rise"
    default_blocked_roles: ["ACCOUNTADMIN", "ORGADMIN", "SECURITYADMIN"]
    default_scopes: ["refresh_token"]
    refresh_token_validity_seconds: 7776000  # 90 days
```

## User Spec

Users can optionally configure additional blocked roles and scopes:

```yaml
blocked_roles:
  - SYSADMIN
scopes:
  - session:role:ANALYST
```

## Lifecycle

1. Pending → TestingConnection (verify Snowflake credentials)
2. TestingConnection → CreatingIntegration (CREATE SECURITY INTEGRATION)
3. CreatingIntegration → RetrievingCredentials (fetch OAuth credentials)
4. RetrievingCredentials → CreatingOAuthExtension (create OAuth extension)
5. CreatingOAuthExtension → Available (ready for use)

The provisioner tests the Snowflake connection before creating the integration to catch
credential issues early. If the test fails, the extension will transition to Failed state
with an error message.

## Secondary Roles

The integration is created with `OAUTH_USE_SECONDARY_ROLES = IMPLICIT`, which enables
secondary roles for OAuth sessions. This allows users to use multiple roles in their session.

Deletion removes all resources: Snowflake integration and OAuth extension.
"#
    }

    fn spec_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "blocked_roles": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Additional blocked roles (unioned with backend defaults)",
                    "default": []
                },
                "scopes": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Additional OAuth scopes (unioned with backend defaults)",
                    "default": []
                }
            },
            "additionalProperties": false
        })
    }

    async fn validate_spec(&self, spec: &Value) -> Result<()> {
        let _parsed: SnowflakeOAuthProvisionerSpec = serde_json::from_value(spec.clone())
            .context("Invalid Snowflake OAuth provisioner spec")?;
        Ok(())
    }

    fn format_status(&self, status: &Value) -> String {
        match serde_json::from_value::<SnowflakeOAuthProvisionerStatus>(status.clone()) {
            Ok(status) => {
                let state = format!("{:?}", status.state);
                if let Some(error) = &status.error {
                    format!("{} (error: {})", state, error)
                } else if let Some(integration_name) = &status.integration_name {
                    format!("{} (integration: {})", state, integration_name)
                } else {
                    state
                }
            }
            Err(_) => "Unknown".to_string(),
        }
    }

    async fn before_deployment(
        &self,
        _project_id: Uuid,
        _deployment_group: &str,
    ) -> Result<Vec<InjectedEnvVar>> {
        // No-op: this extension doesn't inject deployment-specific resources
        Ok(vec![])
    }

    fn start(&self, shutdown: CancellationToken) {
        let provisioner = self.clone();

        tokio::spawn(async move {
            info!("Starting Snowflake OAuth provisioner reconciliation loop");

            let pool = provisioner.db_pool.clone();
            let result = with_leader_election(
                pool,
                "rise-ext-snowflake",
                Uuid::new_v4(),
                std::time::Duration::from_secs(60),
                move |election| async move {
                    let mut error_state: HashMap<Uuid, (usize, DateTime<Utc>)> = HashMap::new();

                    loop {
                        if !election.is_leader() {
                            tokio::select! {
                                _ = shutdown.cancelled() => break,
                                _ = sleep(std::time::Duration::from_secs(5)) => {}
                            }
                            continue;
                        }

                        match db_extensions::list_by_extension_type(
                            &provisioner.db_pool,
                            "snowflake-oauth-provisioner",
                        )
                        .await
                        {
                            Ok(extensions) => {
                                for ext in extensions {
                                    // Apply exponential backoff for errors
                                    if let Some((error_count, last_error)) =
                                        error_state.get(&ext.project_id)
                                    {
                                        let backoff_seconds =
                                            2_i64.pow(*error_count as u32).min(300);
                                        let backoff_until =
                                            *last_error + Duration::seconds(backoff_seconds);

                                        if Utc::now() < backoff_until {
                                            continue;
                                        }
                                    }

                                    if !election.is_leader() {
                                        warn!("Lost leadership before Snowflake reconcile");
                                        break;
                                    }

                                    match provisioner.reconcile_single(ext.clone(), &election).await
                                    {
                                        Ok(_) => {
                                            error_state.remove(&ext.project_id);
                                        }
                                        Err(e) => {
                                            error!("Reconciliation failed: {:?}", e);
                                            let entry = error_state
                                                .entry(ext.project_id)
                                                .or_insert((0, Utc::now()));
                                            entry.0 += 1;
                                            entry.1 = Utc::now();
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                error!("Failed to list Snowflake OAuth extensions: {:?}", e);
                            }
                        }

                        tokio::select! {
                            _ = shutdown.cancelled() => break,
                            _ = sleep(std::time::Duration::from_secs(5)) => {}
                        }
                    }
                    Ok(())
                },
            )
            .await;
            if let Err(e) = result {
                error!(
                    "Snowflake OAuth extension reconciliation loop error: {:?}",
                    e
                );
            }
        });
    }
}
