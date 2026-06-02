use crate::db::{
    self, deployments as db_deployments, extensions as db_extensions, postgres_admin,
    projects as db_projects,
};
use crate::server::encryption::EncryptionProvider;
use crate::server::extensions::{Extension, InjectedEnvVar, InjectedEnvVarValue};
use anyhow::{Context, Result};
use async_trait::async_trait;
use aws_sdk_rds::Client as RdsClient;
use chrono::{DateTime, Duration, Utc};
use rise_runtime_sync::{with_leader_election, LeaderElection};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

// Constants
const RDS_ENGINE_POSTGRES: &str = "postgres";
const RDS_MASTER_USERNAME: &str = "riseadmin";
const RDS_ADMIN_DATABASE: &str = "postgres";

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AwsRdsSpec {
    /// Database engine (currently only "postgres" is supported)
    #[serde(default = "default_engine")]
    pub engine: String,
    /// Engine version (e.g., "16.2")
    #[serde(default)]
    pub engine_version: Option<String>,
    /// Database isolation mode for deployment groups
    #[serde(default = "default_database_isolation")]
    pub database_isolation: DatabaseIsolation,
    /// Environment variable name for the database URL (e.g., "DATABASE_URL", "POSTGRES_URL")
    /// If set to None or empty string, no DATABASE_URL-style variable will be injected
    #[serde(default = "default_database_url_env_var")]
    pub database_url_env_var: Option<String>,
    /// Whether to inject PG* environment variables (PGHOST, PGPORT, etc.)
    #[serde(default = "default_true")]
    pub inject_pg_vars: bool,
}

/// Database isolation mode for deployment groups
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum DatabaseIsolation {
    /// All deployment groups share the same database
    Shared,
    /// Each deployment group gets its own empty database
    Isolated,
}

fn default_engine() -> String {
    RDS_ENGINE_POSTGRES.to_string()
}

fn default_database_isolation() -> DatabaseIsolation {
    DatabaseIsolation::Shared
}

fn default_true() -> bool {
    true
}

fn default_database_url_env_var() -> Option<String> {
    Some("DATABASE_URL".to_string())
}

/// Status and credentials for a specific database
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DatabaseStatus {
    /// Username for this database
    pub user: String,
    /// Encrypted password for this user
    pub password_encrypted: String,
    /// Current provisioning status
    pub status: DatabaseState,
    /// Timestamp when cleanup was scheduled (for inactive deployment groups)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cleanup_scheduled_at: Option<DateTime<Utc>>,
}

/// Provisioning state for an individual database
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub enum DatabaseState {
    Pending,
    CreatingDatabase,
    CreatingUser,
    Available,
    Terminating,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AwsRdsStatus {
    /// Current state of the RDS instance
    pub state: RdsState,
    /// RDS instance identifier
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    /// RDS instance size (e.g., "db.t4g.micro")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_size: Option<String>,
    /// Database endpoint (host:port)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Master username
    #[serde(skip_serializing_if = "Option::is_none")]
    pub master_username: Option<String>,
    /// Encrypted master password
    #[serde(skip_serializing_if = "Option::is_none")]
    pub master_password_encrypted: Option<String>,
    /// Map of database names to their status and credentials
    /// Key is the database name (e.g., project name for default, or "{project}_{deployment_group}" for non-default)
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub databases: HashMap<String, DatabaseStatus>,
    /// Last error message
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum RdsState {
    Pending,
    Creating,
    Available,
    Deleting,
    Deleted,
    Failed,
}

pub struct AwsRdsProvisionerConfig {
    pub rds_client: RdsClient,
    pub db_pool: sqlx::PgPool,
    pub encryption_provider: Arc<dyn EncryptionProvider>,
    pub region: String,
    pub instance_size: String,
    pub disk_size: i32,
    pub instance_id_template: String,
    pub instance_id_prefix: String,
    pub default_engine_version: String,
    pub vpc_security_group_ids: Option<Vec<String>>,
    pub db_subnet_group_name: Option<String>,
    pub backup_retention_days: i32,
    pub backup_window: Option<String>,
    pub maintenance_window: Option<String>,
}

pub struct AwsRdsProvisioner {
    rds_client: RdsClient,
    db_pool: sqlx::PgPool,
    encryption_provider: Arc<dyn EncryptionProvider>,
    region: String,
    instance_size: String,
    disk_size: i32,
    instance_id_template: String,
    instance_id_prefix: String,
    default_engine_version: String,
    vpc_security_group_ids: Option<Vec<String>>,
    db_subnet_group_name: Option<String>,
    backup_retention_days: i32,
    backup_window: Option<String>,
    maintenance_window: Option<String>,
}

impl AwsRdsProvisioner {
    pub async fn new(config: AwsRdsProvisionerConfig) -> Result<Self> {
        Ok(Self {
            rds_client: config.rds_client,
            db_pool: config.db_pool,
            encryption_provider: config.encryption_provider,
            region: config.region,
            instance_size: config.instance_size,
            disk_size: config.disk_size,
            instance_id_template: config.instance_id_template,
            instance_id_prefix: config.instance_id_prefix,
            default_engine_version: config.default_engine_version,
            vpc_security_group_ids: config.vpc_security_group_ids,
            db_subnet_group_name: config.db_subnet_group_name,
            backup_retention_days: config.backup_retention_days,
            backup_window: config.backup_window,
            maintenance_window: config.maintenance_window,
        })
    }

    fn instance_id_for_project(&self, project_name: &str, extension_name: &str) -> String {
        self.instance_id_template
            .replace("{prefix}", &self.instance_id_prefix)
            .replace("{project_name}", project_name)
            .replace("{extension_name}", extension_name)
    }

    /// Get the finalizer name for this extension instance (new format)
    fn finalizer_name(&self, extension_name: &str) -> String {
        format!(
            "rise.dev/extension/{}/{}",
            self.extension_type(),
            extension_name
        )
    }

    /// Get the old finalizer name format (for migration)
    /// TODO: Remove this in a future version after migration period
    fn old_finalizer_name(&self, extension_name: &str) -> String {
        extension_name.to_string()
    }

    /// Reconcile a single RDS extension
    ///
    /// Returns `Ok(true)` if more work can be done immediately (should not wait),
    /// `Ok(false)` if reconciliation is complete or waiting for external state change,
    /// `Err(...)` on error.
    async fn reconcile_single(
        &self,
        project_extension: db::models::ProjectExtension,
        election: &LeaderElection,
    ) -> Result<bool> {
        debug!("Reconciling AWS RDS extension: {:?}", project_extension);
        let project = db_projects::find_by_id(&self.db_pool, project_extension.project_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Project not found"))?;

        // Parse spec
        let spec: AwsRdsSpec = serde_json::from_value(project_extension.spec.clone())
            .context("Failed to parse AWS RDS spec")?;

        // Parse current status or create default
        let mut status: AwsRdsStatus = serde_json::from_value(project_extension.status.clone())
            .unwrap_or(AwsRdsStatus {
                state: RdsState::Pending,
                instance_id: None,
                instance_size: None,
                endpoint: None,
                master_username: None,
                master_password_encrypted: None,
                databases: HashMap::new(),
                error: None,
            });

        // Migrate old finalizer format to new format
        // TODO: Remove this migration logic in a future version
        let old_finalizer = self.old_finalizer_name(&project_extension.extension);
        let new_finalizer = self.finalizer_name(&project_extension.extension);

        // Check if project has the old-style finalizer
        if project.finalizers.contains(&old_finalizer)
            && !project.finalizers.contains(&new_finalizer)
        {
            info!(
                "Migrating finalizer for extension '{}' from old format '{}' to new format '{}'",
                project_extension.extension, old_finalizer, new_finalizer
            );

            // Remove old finalizer
            if let Err(e) =
                db_projects::remove_finalizer(&self.db_pool, project.id, &old_finalizer).await
            {
                error!(
                    "Failed to remove old finalizer '{}' from project {}: {}",
                    old_finalizer, project.name, e
                );
            }

            // Add new finalizer
            if let Err(e) =
                db_projects::add_finalizer(&self.db_pool, project.id, &new_finalizer).await
            {
                error!(
                    "Failed to add new finalizer '{}' to project {}: {}",
                    new_finalizer, project.name, e
                );
            } else {
                info!(
                    "Successfully migrated finalizer for extension '{}' in project {}",
                    project_extension.extension, project.name
                );
            }
        }

        // Check if marked for deletion
        if project_extension.deleted_at.is_some() {
            // Handle deletion
            if status.state != RdsState::Deleted {
                self.handle_deletion(&mut status, &project.name).await?;
                // Update status
                election.assert_leader().await?;
                db_extensions::update_status(
                    &self.db_pool,
                    project_extension.project_id,
                    &project_extension.extension,
                    &serde_json::to_value(&status)?,
                )
                .await?;

                // If deletion is complete, hard delete the record and remove finalizer
                if status.state == RdsState::Deleted {
                    let finalizer = self.finalizer_name(&project_extension.extension);

                    // Remove finalizer so project can be deleted
                    election.assert_leader().await?;
                    if let Err(e) = db_projects::remove_finalizer(
                        &self.db_pool,
                        project_extension.project_id,
                        &finalizer,
                    )
                    .await
                    {
                        error!(
                            "Failed to remove finalizer '{}' from project {}: {}",
                            finalizer, project.name, e
                        );
                    } else {
                        info!(
                            "Removed finalizer '{}' from project {}",
                            finalizer, project.name
                        );
                    }

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
            return Ok(false); // Deletion complete, no more work
        }

        // Track if state changed during this reconciliation
        let initial_state = status.state.clone();
        let initial_db_states: Vec<_> = status
            .databases
            .values()
            .map(|db| db.status.clone())
            .collect();

        // Handle normal lifecycle
        match status.state {
            RdsState::Pending => {
                self.handle_pending(
                    &spec,
                    &mut status,
                    &project.name,
                    project.id,
                    &project_extension.extension,
                )
                .await?;
            }
            RdsState::Creating => {
                self.handle_creating(&mut status, &project.name, project.id)
                    .await?;
            }
            RdsState::Available => {
                // Check if instance still exists and perform cleanup
                self.verify_instance_available(&mut status, &project.name, project.id, &spec)
                    .await?;
            }
            RdsState::Failed => {
                // Retry creation immediately
                info!(
                    "RDS instance for project {} is in failed state, retrying immediately",
                    project.name
                );
                status.state = RdsState::Pending;
                status.error = None;
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
        let db_states_changed = {
            let current_db_states: Vec<_> = status
                .databases
                .values()
                .map(|db| db.status.clone())
                .collect();
            current_db_states != initial_db_states
        };

        // More work needed if:
        // - State transitioned (e.g., Pending → Creating, Failed → Pending)
        // - Database states changed (e.g., Pending → CreatingDatabase)
        // - In Creating state (might have made progress on database provisioning)
        let needs_more_work =
            state_changed || db_states_changed || status.state == RdsState::Creating;

        Ok(needs_more_work)
    }

    async fn handle_pending(
        &self,
        spec: &AwsRdsSpec,
        status: &mut AwsRdsStatus,
        project_name: &str,
        project_id: Uuid,
        extension_name: &str,
    ) -> Result<()> {
        // Use stored instance_id if already set, otherwise generate a new unique one
        let instance_id = if let Some(ref existing_id) = status.instance_id {
            existing_id.clone()
        } else {
            self.instance_id_for_project(project_name, extension_name)
        };

        info!(
            "Creating RDS instance {} for project {} (extension: {})",
            instance_id, project_name, extension_name
        );

        // Generate master credentials
        let master_username = RDS_MASTER_USERNAME.to_string();
        let master_password = self.generate_password();

        // Encrypt password
        let encrypted_password = self
            .encryption_provider
            .encrypt(&master_password)
            .await
            .context("Failed to encrypt master password")?;

        // Validate VPC configuration
        // If VPC security groups are specified, a subnet group is required to place the instance in the VPC
        if self.vpc_security_group_ids.is_some() && self.db_subnet_group_name.is_none() {
            let error_msg = "vpc_security_group_ids requires db_subnet_group_name to be set";
            error!("{}", error_msg);
            status.state = RdsState::Failed;
            status.error = Some(error_msg.to_string());
            return Ok(());
        }

        // Create RDS instance
        // Use spec engine_version if provided, otherwise use the provisioner's default
        let engine_version = spec
            .engine_version
            .clone()
            .unwrap_or_else(|| self.default_engine_version.clone());

        // Build tags for the RDS instance
        let managed_tag = aws_sdk_rds::types::Tag::builder()
            .key("rise:managed")
            .value("true")
            .build();
        let project_tag = aws_sdk_rds::types::Tag::builder()
            .key("rise:project")
            .value(project_name)
            .build();

        let mut create_request = self
            .rds_client
            .create_db_instance()
            .db_instance_identifier(&instance_id)
            .db_instance_class(&self.instance_size)
            .engine(RDS_ENGINE_POSTGRES)
            .engine_version(&engine_version)
            .master_username(&master_username)
            .master_user_password(&master_password)
            .allocated_storage(self.disk_size)
            .publicly_accessible(false)
            .storage_encrypted(true)
            .backup_retention_period(self.backup_retention_days)
            .tags(managed_tag)
            .tags(project_tag);

        // Add VPC security groups if configured
        if let Some(ref security_groups) = self.vpc_security_group_ids {
            for sg in security_groups {
                create_request = create_request.vpc_security_group_ids(sg);
            }
        }

        // Add DB subnet group if configured
        if let Some(ref subnet_group) = self.db_subnet_group_name {
            create_request = create_request.db_subnet_group_name(subnet_group);
        }

        // Add backup window if configured
        if let Some(ref backup_window) = self.backup_window {
            create_request = create_request.preferred_backup_window(backup_window);
        }

        // Add maintenance window if configured
        if let Some(ref maintenance_window) = self.maintenance_window {
            create_request = create_request.preferred_maintenance_window(maintenance_window);
        }

        match create_request.send().await {
            Ok(_) => {
                info!("RDS create request sent for instance {}", instance_id);
                status.state = RdsState::Creating;
                status.instance_id = Some(instance_id);
                status.instance_size = Some(self.instance_size.clone());
                status.master_username = Some(master_username);
                status.master_password_encrypted = Some(encrypted_password);
                status.error = None;

                // Add finalizer immediately to ensure cleanup if project is deleted during provisioning
                let finalizer = self.finalizer_name(extension_name);
                if let Err(e) =
                    db_projects::add_finalizer(&self.db_pool, project_id, &finalizer).await
                {
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
            }
            Err(e) => {
                error!("Failed to create RDS instance {}: {:?}", instance_id, e);
                status.state = RdsState::Failed;
                status.error = Some(format!("Failed to create instance: {:?}", e));
            }
        }

        Ok(())
    }

    async fn handle_creating(
        &self,
        status: &mut AwsRdsStatus,
        project_name: &str,
        _project_id: Uuid,
    ) -> Result<()> {
        let instance_id = status
            .instance_id
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Instance ID not set in creating state"))?;

        // Check instance status
        match self
            .rds_client
            .describe_db_instances()
            .db_instance_identifier(instance_id)
            .send()
            .await
        {
            Ok(resp) => {
                let instances = resp.db_instances();
                if let Some(instance) = instances.first() {
                    if let Some(instance_status) = instance.db_instance_status() {
                        match instance_status {
                            "available" => {
                                info!("RDS instance {} is now available", instance_id);
                                status.state = RdsState::Available;

                                // Extract endpoint
                                if let Some(endpoint) = instance.endpoint() {
                                    if let (Some(address), Some(port)) =
                                        (endpoint.address(), endpoint.port())
                                    {
                                        status.endpoint = Some(format!("{}:{}", address, port));

                                        // Provision default database with state tracking
                                        let default_db_name =
                                            format!("{}_db_default", project_name);

                                        // Check if we need to create a new database entry
                                        if !status.databases.contains_key(&default_db_name) {
                                            // Generate credentials for new database
                                            let username =
                                                format!("{}_db_default_user", project_name);
                                            let password = self.generate_password();
                                            let encrypted = self
                                                .encryption_provider
                                                .encrypt(&password)
                                                .await
                                                .unwrap_or(password.clone()); // Fallback if encryption fails

                                            status.databases.insert(
                                                default_db_name.clone(),
                                                DatabaseStatus {
                                                    user: username,
                                                    password_encrypted: encrypted,
                                                    status: DatabaseState::Pending,
                                                    cleanup_scheduled_at: None,
                                                },
                                            );
                                        }

                                        // Process databases (will handle Pending -> CreatingDatabase -> CreatingUser -> Available)
                                        self.process_databases(status, address, port).await?;
                                    }
                                }

                                // Only mark as Available if all databases are Available
                                let all_databases_ready = status
                                    .databases
                                    .values()
                                    .all(|db| db.status == DatabaseState::Available);

                                if all_databases_ready && !status.databases.is_empty() {
                                    status.error = None;
                                } else {
                                    // Keep state as Creating if databases aren't ready yet
                                    status.state = RdsState::Creating;
                                }
                            }
                            "creating"
                            | "configuring-enhanced-monitoring"
                            | "backing-up"
                            | "modifying" => {
                                debug!(
                                    "RDS instance {} is still creating (status: {})",
                                    instance_id, instance_status
                                );
                            }
                            "failed" => {
                                error!("RDS instance {} failed to create", instance_id);
                                status.state = RdsState::Failed;
                                status.error = Some("Instance creation failed".to_string());
                            }
                            _ => {
                                warn!(
                                    "RDS instance {} has unexpected status: {}",
                                    instance_id, instance_status
                                );
                            }
                        }
                    }
                }
            }
            Err(e) => {
                error!("Failed to describe RDS instance {}: {:?}", instance_id, e);
                // Don't fail immediately, will retry on next reconcile
            }
        }

        Ok(())
    }

    async fn verify_instance_available(
        &self,
        status: &mut AwsRdsStatus,
        project_name: &str,
        project_id: Uuid,
        spec: &AwsRdsSpec,
    ) -> Result<()> {
        let instance_id = status
            .instance_id
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Instance ID not set in available state"))?;

        // Check if instance still exists
        match self
            .rds_client
            .describe_db_instances()
            .db_instance_identifier(instance_id)
            .send()
            .await
        {
            Ok(resp) => {
                let instances = resp.db_instances();
                if let Some(instance) = instances.first() {
                    if let Some(instance_status) = instance.db_instance_status() {
                        if instance_status != "available" {
                            warn!(
                                "RDS instance {} status changed from available to {}",
                                instance_id, instance_status
                            );
                            status.state = RdsState::Creating; // Will check again on next reconcile
                        } else {
                            // Instance is available, process any pending databases
                            if let Some(endpoint) = instance.endpoint() {
                                if let (Some(address), Some(port)) =
                                    (endpoint.address(), endpoint.port())
                                {
                                    self.process_databases(status, address, port).await?;

                                    // Cleanup: Mark orphaned databases for deletion (isolated mode only)
                                    if spec.database_isolation == DatabaseIsolation::Isolated {
                                        self.cleanup_orphaned_databases(
                                            status,
                                            project_id,
                                            project_name,
                                            spec,
                                            &format!("{}:{}", address, port),
                                        )
                                        .await?;
                                    }
                                }
                            }
                        }
                    }
                } else {
                    error!("RDS instance {} no longer exists", instance_id);
                    status.state = RdsState::Failed;
                    status.error = Some("Instance no longer exists".to_string());
                }
            }
            Err(e) => {
                warn!("Failed to verify RDS instance {}: {:?}", instance_id, e);
                // Don't fail immediately
            }
        }

        Ok(())
    }

    /// Process all databases in transitional states (Pending, CreatingDatabase, CreatingUser)
    /// Returns early (Ok(())) if a state transition happened and needs another reconciliation
    async fn process_databases(
        &self,
        status: &mut AwsRdsStatus,
        address: &str,
        port: i32,
    ) -> Result<()> {
        // Process each database that's not in Available or Terminating state
        for (db_name, db_status) in status.databases.iter_mut() {
            match db_status.status {
                DatabaseState::Pending => {
                    info!("Starting provisioning for database '{}'", db_name);
                    db_status.status = DatabaseState::CreatingDatabase;
                    // Will continue in next reconciliation
                    return Ok(());
                }
                DatabaseState::CreatingDatabase => {
                    // Decrypt master password
                    let encrypted = match status
                        .master_password_encrypted
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("Master password not set"))
                    {
                        Ok(encrypted) => encrypted,
                        Err(e) => {
                            error!("Failed to decrypt master password: {}", e);
                            status.state = RdsState::Failed;
                            status.error = Some("Failed to decrypt master password".to_string());
                            return Ok(());
                        }
                    };

                    let master_password = match self.encryption_provider.decrypt(encrypted).await {
                        Ok(pwd) => pwd,
                        Err(e) => {
                            error!("Failed to decrypt master password: {}", e);
                            status.state = RdsState::Failed;
                            status.error = Some("Failed to decrypt master password".to_string());
                            return Ok(());
                        }
                    };

                    let master_username = status.master_username.as_ref().unwrap();
                    let admin_db_url = format!(
                        "postgres://{}:{}@{}:{}/{}",
                        master_username, master_password, address, port, RDS_ADMIN_DATABASE
                    );

                    // Create database
                    match self
                        .create_default_database(&admin_db_url, db_name, master_username)
                        .await
                    {
                        Ok(_) => {
                            info!("Created database '{}'", db_name);
                            db_status.status = DatabaseState::CreatingUser;
                            // Will continue in next reconciliation
                            return Ok(());
                        }
                        Err(e) => {
                            error!("Failed to create database '{}': {:?}", db_name, e);
                            status.state = RdsState::Creating;
                            status.error = Some(format!("Failed to create database: {}", e));
                            return Ok(());
                        }
                    }
                }
                DatabaseState::CreatingUser => {
                    // Decrypt passwords
                    let encrypted = match status
                        .master_password_encrypted
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("Master password not set"))
                    {
                        Ok(encrypted) => encrypted,
                        Err(e) => {
                            error!("Failed to decrypt master password: {}", e);
                            status.state = RdsState::Failed;
                            return Ok(());
                        }
                    };

                    let master_password = match self.encryption_provider.decrypt(encrypted).await {
                        Ok(pwd) => pwd,
                        Err(e) => {
                            error!("Failed to decrypt master password: {}", e);
                            status.state = RdsState::Failed;
                            return Ok(());
                        }
                    };

                    let user_password = match self
                        .encryption_provider
                        .decrypt(&db_status.password_encrypted)
                        .await
                    {
                        Ok(pwd) => pwd,
                        Err(e) => {
                            error!("Failed to decrypt user password: {}", e);
                            db_status.status = DatabaseState::Pending;
                            return Ok(());
                        }
                    };

                    // Create user and grant privileges
                    let admin_db_url = format!(
                        "postgres://{}:{}@{}:{}/{}",
                        status.master_username.as_ref().unwrap(),
                        master_password,
                        address,
                        port,
                        RDS_ADMIN_DATABASE
                    );

                    match PgPool::connect(&admin_db_url).await {
                        Ok(pool) => {
                            // Check if user already exists
                            let user_exists =
                                match postgres_admin::user_exists(&pool, &db_status.user).await {
                                    Ok(exists) => exists,
                                    Err(e) => {
                                        error!("Failed to check if user exists: {:?}", e);
                                        return Ok(());
                                    }
                                };

                            if !user_exists {
                                // Create user if doesn't exist
                                match postgres_admin::create_user(
                                    &pool,
                                    &db_status.user,
                                    &user_password,
                                )
                                .await
                                {
                                    Ok(_) => info!("Created user '{}'", db_status.user),
                                    Err(e) => {
                                        error!("Failed to create user: {:?}", e);
                                        return Ok(());
                                    }
                                }
                            } else {
                                info!(
                                    "User '{}' already exists, skipping creation",
                                    db_status.user
                                );
                            }

                            // Change database owner to give full privileges
                            match postgres_admin::change_database_owner(
                                &pool,
                                db_name,
                                &db_status.user,
                            )
                            .await
                            {
                                Ok(_) => {
                                    info!("Changed owner of '{}' to '{}'", db_name, db_status.user);
                                    db_status.status = DatabaseState::Available;
                                }
                                Err(e) => {
                                    error!("Failed to change database owner: {:?}", e);
                                    return Ok(());
                                }
                            }
                        }
                        Err(e) => {
                            error!("Failed to connect to RDS instance: {:?}", e);
                            status.error = Some(format!("Failed to connect: {}", e));
                            return Ok(());
                        }
                    }
                }
                DatabaseState::Available | DatabaseState::Terminating => {
                    // Nothing to do for these states
                }
            }
        }

        Ok(())
    }

    /// Cleanup databases for inactive deployment groups (isolated mode only)
    async fn cleanup_orphaned_databases(
        &self,
        status: &mut AwsRdsStatus,
        project_id: Uuid,
        project_name: &str,
        spec: &AwsRdsSpec,
        endpoint: &str,
    ) -> Result<()> {
        use std::collections::HashSet;

        // Get list of active deployment groups for this project
        let active_groups = db_deployments::get_active_deployment_groups(&self.db_pool, project_id)
            .await
            .context("Failed to get active deployment groups")?;

        let now = Utc::now();
        let grace_period = Duration::hours(1);

        // Build set of expected database names from active deployment groups
        let expected_databases: HashSet<String> = active_groups
            .iter()
            .map(|group| Self::compute_database_name(project_name, group, &spec.database_isolation))
            .collect();

        let mut databases_to_remove = Vec::new();

        for (db_name, db_status) in status.databases.iter_mut() {
            // Skip shared database (always keep)
            if db_name.ends_with("_db_default") {
                continue;
            }

            // Check if this database corresponds to an active deployment group
            let is_active = expected_databases.contains(db_name);

            if !is_active {
                // Database doesn't match any active deployment group
                if let Some(scheduled_at) = db_status.cleanup_scheduled_at {
                    // Already scheduled, check if grace period expired
                    if now >= scheduled_at + grace_period {
                        info!(
                            "Grace period expired for database '{}', cleaning up",
                            db_name
                        );
                        databases_to_remove.push(db_name.clone());
                    } else {
                        debug!(
                            "Database '{}' scheduled for cleanup at {} (grace period not expired)",
                            db_name,
                            scheduled_at + grace_period
                        );
                    }
                } else {
                    // First time inactive, schedule for cleanup
                    db_status.cleanup_scheduled_at = Some(now);
                    info!(
                        "Database '{}' has no active deployment group, scheduling for cleanup in 1 hour",
                        db_name
                    );
                }
            } else {
                // Database has active deployment group, cancel cleanup if scheduled
                if db_status.cleanup_scheduled_at.is_some() {
                    info!(
                        "Database '{}' has active deployments again, cancelling cleanup",
                        db_name
                    );
                    db_status.cleanup_scheduled_at = None;
                }
            }
        }

        // Execute cleanup for databases past grace period
        if !databases_to_remove.is_empty() {
            let master_username = status
                .master_username
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Master username not set"))?;

            let master_password = self
                .encryption_provider
                .decrypt(
                    status
                        .master_password_encrypted
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("Master password not set"))?,
                )
                .await
                .context("Failed to decrypt master password")?;

            let admin_db_url = format!(
                "postgres://{}:{}@{}/{}",
                master_username, master_password, endpoint, RDS_ADMIN_DATABASE
            );

            let pool = PgPool::connect(&admin_db_url)
                .await
                .context("Failed to connect to RDS for cleanup")?;

            for db_name in databases_to_remove {
                if let Some(db_status) = status.databases.get(&db_name) {
                    let username = db_status.user.clone(); // Clone to avoid borrow issue

                    // Change database owner to master user before dropping
                    // (required because only the owner can drop a database)
                    match postgres_admin::change_database_owner(&pool, &db_name, master_username)
                        .await
                    {
                        Ok(_) => {
                            debug!("Changed owner of database '{}' to master user", db_name);
                        }
                        Err(e) => {
                            warn!("Failed to change owner of database '{}': {:?}", db_name, e);
                            continue; // Skip this database if we can't change owner
                        }
                    }

                    // Drop database
                    match postgres_admin::drop_database(&pool, &db_name).await {
                        Ok(_) => {
                            info!("Dropped database '{}'", db_name);
                        }
                        Err(e) => {
                            warn!("Failed to drop database '{}': {:?}", db_name, e);
                            continue; // Don't drop user if database drop failed
                        }
                    }

                    // Drop user
                    match postgres_admin::drop_user(&pool, &username).await {
                        Ok(_) => {
                            info!("Dropped user '{}'", username);
                        }
                        Err(e) => {
                            warn!("Failed to drop user '{}': {:?}", username, e);
                            // Continue anyway - database is already dropped
                        }
                    }

                    // Remove from status
                    status.databases.remove(&db_name);
                    info!(
                        "Cleaned up database '{}' and user '{}' for inactive deployment group",
                        db_name, username
                    );
                }
            }

            pool.close().await;
        }

        Ok(())
    }

    async fn handle_deletion(&self, status: &mut AwsRdsStatus, project_name: &str) -> Result<()> {
        let instance_id = match &status.instance_id {
            Some(id) => id.clone(),
            None => {
                // No instance ID, mark as deleted
                status.state = RdsState::Deleted;
                return Ok(());
            }
        };

        match status.state {
            RdsState::Pending | RdsState::Creating | RdsState::Available | RdsState::Failed => {
                // Initiate deletion (works even if instance is still creating)
                info!(
                    "Initiating deletion of RDS instance {} for project {} (current state: {:?})",
                    instance_id, project_name, status.state
                );

                match self
                    .rds_client
                    .delete_db_instance()
                    .db_instance_identifier(&instance_id)
                    .skip_final_snapshot(true)
                    .send()
                    .await
                {
                    Ok(_) => {
                        info!("RDS delete request sent for instance {}", instance_id);
                        status.state = RdsState::Deleting;
                    }
                    Err(e) => {
                        let err = e.into_service_error();
                        if err.is_db_instance_not_found_fault() {
                            info!(
                                "RDS instance {} not found (may not have been created yet)",
                                instance_id
                            );
                            status.state = RdsState::Deleted;
                        } else if err.is_invalid_db_instance_state_fault() {
                            warn!(
                                "RDS instance {} not in a deletable state yet, will retry",
                                instance_id
                            );
                        } else {
                            error!("Failed to delete RDS instance {}: {:?}", instance_id, err);
                            status.error = Some(format!("Failed to delete instance: {:?}", err));
                        }
                    }
                }
            }
            RdsState::Deleting => {
                // Check deletion progress
                match self
                    .rds_client
                    .describe_db_instances()
                    .db_instance_identifier(&instance_id)
                    .send()
                    .await
                {
                    Ok(resp) => {
                        let instances = resp.db_instances();
                        if instances.is_empty() {
                            info!("RDS instance {} successfully deleted", instance_id);
                            status.state = RdsState::Deleted;
                        } else if let Some(instance) = instances.first() {
                            if let Some(instance_status) = instance.db_instance_status() {
                                debug!(
                                    "RDS instance {} deletion in progress (status: {})",
                                    instance_id, instance_status
                                );
                            }
                        }
                    }
                    Err(e) => {
                        let err = e.into_service_error();
                        if err.is_db_instance_not_found_fault() {
                            info!("RDS instance {} successfully deleted", instance_id);
                            status.state = RdsState::Deleted;
                        } else {
                            error!("Error checking RDS instance deletion: {:?}", err);
                        }
                    }
                }
            }
            _ => {
                // Already deleted or in another state
            }
        }

        Ok(())
    }

    fn generate_password(&self) -> String {
        use rand::RngExt;
        const CHARSET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
        let mut rng = rand::rng();
        (0..32)
            .map(|_| {
                let idx = rng.random_range(0..CHARSET.len());
                CHARSET[idx] as char
            })
            .collect()
    }

    /// Build InjectedEnvVar list from database credentials.
    /// Shared by both `before_deployment` and `preview_env_vars`.
    async fn build_injected_env_vars(
        &self,
        spec: &AwsRdsSpec,
        endpoint: &str,
        database_name: &str,
        db_username: &str,
        db_password: &str,
    ) -> Result<Vec<InjectedEnvVar>> {
        // Parse endpoint to extract host and port
        let (host, port) = if let Some(colon_pos) = endpoint.rfind(':') {
            let h = &endpoint[..colon_pos];
            let p = &endpoint[colon_pos + 1..];
            (h.to_string(), p.to_string())
        } else {
            (endpoint.to_string(), "5432".to_string())
        };

        let mut result = Vec::new();

        // Inject database URL environment variable if requested
        if let Some(ref env_var_name) = spec.database_url_env_var {
            if !env_var_name.is_empty() {
                let database_url = format!(
                    "postgres://{}:{}@{}/{}",
                    db_username, db_password, endpoint, database_name
                );

                let encrypted_database_url = self
                    .encryption_provider
                    .encrypt(&database_url)
                    .await
                    .context(format!("Failed to encrypt {}", env_var_name))?;

                result.push(InjectedEnvVar {
                    key: env_var_name.clone(),
                    value: InjectedEnvVarValue::Protected {
                        decrypted: database_url,
                        encrypted: encrypted_database_url,
                    },
                });
            }
        }

        // Inject PG* environment variables if requested
        if spec.inject_pg_vars {
            // Non-secret connection parameters use Plain variant
            for (key, value) in [
                ("PGHOST", host.as_str()),
                ("PGPORT", port.as_str()),
                ("PGDATABASE", database_name),
                ("PGUSER", db_username),
            ] {
                result.push(InjectedEnvVar {
                    key: key.to_string(),
                    value: InjectedEnvVarValue::Plain(value.to_string()),
                });
            }

            // Only the password is a protected secret
            let encrypted_password = self
                .encryption_provider
                .encrypt(db_password)
                .await
                .context("Failed to encrypt password")?;

            result.push(InjectedEnvVar {
                key: "PGPASSWORD".to_string(),
                value: InjectedEnvVarValue::Protected {
                    decrypted: db_password.to_string(),
                    encrypted: encrypted_password,
                },
            });
        }

        Ok(result)
    }

    /// Compute expected database name for a deployment group
    ///
    /// This uses the same logic as before_deployment() to determine database names,
    /// ensuring consistency across the codebase.
    fn compute_database_name(
        project_name: &str,
        deployment_group: &str,
        isolation_mode: &DatabaseIsolation,
    ) -> String {
        let safe_deployment_group = deployment_group.replace(['/', '-'], "_");

        match isolation_mode {
            DatabaseIsolation::Shared => {
                format!("{}_db_default", project_name)
            }
            DatabaseIsolation::Isolated => {
                format!("{}_db_{}", project_name, safe_deployment_group)
            }
        }
    }

    /// Create the default database for a project
    async fn create_default_database(
        &self,
        admin_db_url: &str,
        database_name: &str,
        owner: &str,
    ) -> Result<()> {
        // Connect to the postgres database to run administrative commands
        let pool = PgPool::connect(admin_db_url)
            .await
            .context("Failed to connect to RDS instance")?;

        // Check if the database already exists
        let exists = postgres_admin::database_exists(&pool, database_name).await?;

        if exists {
            info!(
                "Database '{}' already exists, skipping creation",
                database_name
            );
            pool.close().await;
            return Ok(());
        }

        // Create the database
        postgres_admin::create_database(&pool, database_name, owner).await?;

        info!("Successfully created default database '{}'", database_name);

        pool.close().await;
        Ok(())
    }
}

#[async_trait]
impl Extension for AwsRdsProvisioner {
    fn extension_type(&self) -> &str {
        "aws-rds-provisioner"
    }

    fn display_name(&self) -> &str {
        "AWS RDS Database"
    }

    async fn validate_spec(&self, spec: &Value) -> Result<()> {
        let parsed: AwsRdsSpec =
            serde_json::from_value(spec.clone()).context("Failed to parse AWS RDS spec")?;

        if parsed.engine != RDS_ENGINE_POSTGRES {
            anyhow::bail!(
                "Only '{}' engine is currently supported",
                RDS_ENGINE_POSTGRES
            );
        }

        Ok(())
    }

    fn start(&self, shutdown: CancellationToken) {
        let provisioner = Self {
            rds_client: self.rds_client.clone(),
            db_pool: self.db_pool.clone(),
            encryption_provider: self.encryption_provider.clone(),
            region: self.region.clone(),
            instance_size: self.instance_size.clone(),
            disk_size: self.disk_size,
            instance_id_template: self.instance_id_template.clone(),
            instance_id_prefix: self.instance_id_prefix.clone(),
            default_engine_version: self.default_engine_version.clone(),
            vpc_security_group_ids: self.vpc_security_group_ids.clone(),
            db_subnet_group_name: self.db_subnet_group_name.clone(),
            backup_retention_days: self.backup_retention_days,
            backup_window: self.backup_window.clone(),
            maintenance_window: self.maintenance_window.clone(),
        };

        tokio::spawn(async move {
            info!(
                "Starting AWS RDS extension reconciliation loop for type '{}'",
                provisioner.extension_type()
            );

            let pool = provisioner.db_pool.clone();
            let result = with_leader_election(
                pool,
                "rise-ext-rds",
                Uuid::new_v4(),
                std::time::Duration::from_secs(60),
                move |election| async move {
            // Track error counts and last error times for exponential backoff
            let mut error_state: HashMap<Uuid, (usize, DateTime<Utc>)> = HashMap::new();

            loop {
                if !election.is_leader() {
                    tokio::select! {
                        _ = shutdown.cancelled() => break,
                        _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
                    }
                    continue;
                }

                // List ALL project extensions of this type (across all projects)
                match db_extensions::list_by_extension_type(
                    &provisioner.db_pool,
                    provisioner.extension_type(),
                )
                .await
                {
                    Ok(extensions) => {
                        if extensions.is_empty() {
                            debug!("No RDS extensions found, waiting for work");
                        }

                        for ext in extensions {
                            // Check if we should skip this extension due to backoff
                            if let Some((error_count, last_error)) =
                                error_state.get(&ext.project_id)
                            {
                                // Exponential backoff: 2^error_count seconds (capped at 5 minutes)
                                let backoff_seconds = 2_i64.pow(*error_count as u32).min(300);
                                let backoff_until =
                                    *last_error + Duration::seconds(backoff_seconds);

                                if Utc::now() < backoff_until {
                                    debug!(
                                        "Skipping extension for project {} due to backoff ({}s remaining)",
                                        ext.project_id,
                                        (backoff_until - Utc::now()).num_seconds()
                                    );
                                    continue;
                                }
                            }

                            if !election.is_leader() {
                                warn!("Lost leadership before RDS reconcile");
                                break;
                            }

                            match provisioner.reconcile_single(ext.clone(), &election).await {
                                Ok(_) => {
                                    // Success - reset error state
                                    error_state.remove(&ext.project_id);
                                }
                                Err(e) => {
                                    error!("Failed to reconcile AWS RDS extension: {:?}", e);
                                    // Increment error count and update last error time
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
                        error!("Failed to list extensions: {:?}", e);
                    }
                }

                // Check if any extension is in a transitional state
                let needs_active_polling = match db_extensions::list_by_extension_type(
                    &provisioner.db_pool,
                    provisioner.extension_type(),
                )
                .await
                {
                    Ok(extensions) => extensions.iter().any(|ext| {
                        if let Ok(status) =
                            serde_json::from_value::<AwsRdsStatus>(ext.status.clone())
                        {
                            matches!(
                                status.state,
                                RdsState::Pending
                                    | RdsState::Creating
                                    | RdsState::Deleting
                                    | RdsState::Failed
                            ) || ext.deleted_at.is_some()
                        } else {
                            false
                        }
                    }),
                    Err(_) => false,
                };

                let wait_time = if needs_active_polling { 2 } else { 5 };
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = sleep(std::time::Duration::from_secs(wait_time)) => {}
                }
            }
            Ok(())
                },
            )
            .await;
            if let Err(e) = result {
                error!("AWS RDS extension reconciliation loop error: {:?}", e);
            }
        });
    }

    async fn before_deployment(
        &self,
        project_id: Uuid,
        deployment_group: &str,
    ) -> Result<Vec<InjectedEnvVar>> {
        // Find all extensions of this type for this project
        let extensions =
            db_extensions::list_by_extension_type(&self.db_pool, self.extension_type())
                .await?
                .into_iter()
                .filter(|e| e.project_id == project_id && e.deleted_at.is_none())
                .collect::<Vec<_>>();

        if extensions.is_empty() {
            // Extension not enabled for this project - skip hook
            debug!(
                "Extension type '{}' not enabled for project {}, skipping before_deployment hook",
                self.extension_type(),
                project_id
            );
            return Ok(vec![]);
        }

        // For RDS, we expect at most one instance per project
        // If there are multiple, use the first one and log a warning
        let ext = &extensions[0];
        if extensions.len() > 1 {
            warn!(
                "Multiple RDS extensions found for project {}, using first instance: {}",
                project_id, ext.extension
            );
        }

        // Parse spec to get injection preferences
        let spec: AwsRdsSpec =
            serde_json::from_value(ext.spec.clone()).context("Failed to parse AWS RDS spec")?;

        // Get project info for database naming
        let project = db_projects::find_by_id(&self.db_pool, project_id)
            .await
            .context("Failed to find project")?
            .ok_or_else(|| anyhow::anyhow!("Project not found"))?;

        // Parse status (mutable so we can update database_users)
        let mut status: AwsRdsStatus =
            serde_json::from_value(ext.status.clone()).context("Failed to parse AWS RDS status")?;

        // Check if instance is available
        if status.state != RdsState::Available {
            anyhow::bail!(
                "RDS extension '{}' is not available (current state: {:?})",
                ext.extension,
                status.state
            );
        }

        let endpoint = status
            .endpoint
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("RDS endpoint not set"))?;

        let master_username = status
            .master_username
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Master username not set"))?;

        let encrypted_password = status
            .master_password_encrypted
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Master password not set"))?;

        // Decrypt password
        let master_password = self
            .encryption_provider
            .decrypt(encrypted_password)
            .await
            .context("Failed to decrypt master password")?;

        // Sanitize deployment_group for use in database/user names (replace slashes and special chars)
        let safe_deployment_group = deployment_group.replace(['/', '-'], "_");

        // Determine database name based on isolation mode and deployment group
        let database_name = match spec.database_isolation {
            DatabaseIsolation::Shared => {
                // All deployment groups share the same database
                format!("{}_db_default", project.name)
            }
            DatabaseIsolation::Isolated => {
                // Each deployment group gets its own database
                format!("{}_db_{}", project.name, safe_deployment_group)
            }
        };

        // Connect to the RDS instance to manage databases and users
        let admin_db_url = format!(
            "postgres://{}:{}@{}/{}",
            master_username, master_password, endpoint, RDS_ADMIN_DATABASE
        );

        // For isolated mode with non-default deployment groups, create isolated database
        if spec.database_isolation == DatabaseIsolation::Isolated && deployment_group != "default" {
            let pool = PgPool::connect(&admin_db_url)
                .await
                .context("Failed to connect to RDS instance")?;

            // Check if database already exists
            let db_exists = postgres_admin::database_exists(&pool, &database_name)
                .await
                .context("Failed to check if database exists")?;

            if !db_exists {
                info!(
                    "Creating isolated database '{}' for deployment group '{}'",
                    database_name, deployment_group
                );

                // Create empty database (owned by master user initially)
                postgres_admin::create_database(&pool, &database_name, master_username)
                    .await
                    .context("Failed to create isolated database")?;

                info!("Successfully created isolated database '{}'", database_name);
            }

            pool.close().await;
        }

        // Check if we already have credentials for this database
        let (db_username, db_password) = if let Some(db_status) =
            status.databases.get(&database_name)
        {
            // Ensure database is Available before using it
            if db_status.status != DatabaseState::Available {
                anyhow::bail!(
                    "RDS extension '{}': Database '{}' is not available (current state: {:?})",
                    ext.extension,
                    database_name,
                    db_status.status
                );
            }

            // Verify the database actually exists in PostgreSQL
            let pool = PgPool::connect(&admin_db_url)
                .await
                .context("Failed to connect to RDS instance to verify database")?;

            let db_exists = postgres_admin::database_exists(&pool, &database_name)
                .await
                .context("Failed to check if database exists")?;

            pool.close().await;

            if !db_exists {
                warn!(
                    "Database '{}' is marked as Available in status but does not exist in PostgreSQL, marking for recreation",
                    database_name
                );

                // Reset the database state to Pending so the reconciliation loop will recreate it
                status.databases.get_mut(&database_name).unwrap().status = DatabaseState::Pending;

                // Update extension status
                db_extensions::update_status(
                    &self.db_pool,
                    project_id,
                    &ext.extension,
                    &serde_json::to_value(&status)?,
                )
                .await
                .context(
                    "Failed to update extension status after marking database for recreation",
                )?;

                anyhow::bail!(
                    "RDS extension '{}': Database '{}' does not exist and has been marked for recreation, retry deployment",
                    ext.extension,
                    database_name
                );
            }

            info!(
                "Reusing existing database user '{}' for database '{}'",
                db_status.user, database_name
            );

            let password = self
                .encryption_provider
                .decrypt(&db_status.password_encrypted)
                .await
                .context("Failed to decrypt database user password")?;

            (db_status.user.clone(), password)
        } else {
            // Create new database user credentials (matches database name pattern)
            let username = format!("{}_db_{}_user", project.name, safe_deployment_group);
            let password = self.generate_password();

            info!(
                "Creating new database user '{}' for deployment group '{}'",
                username, deployment_group
            );

            let pool = PgPool::connect(&admin_db_url)
                .await
                .context("Failed to connect to RDS instance for user creation")?;

            // Check if user already exists (shouldn't happen, but handle it)
            let user_exists = postgres_admin::user_exists(&pool, &username)
                .await
                .context("Failed to check if user exists")?;

            if !user_exists {
                postgres_admin::create_user(&pool, &username, &password)
                    .await
                    .context("Failed to create database user")?;

                info!("Created database user '{}'", username);
            } else {
                warn!(
                    "Database user '{}' already exists in PostgreSQL but not in status, updating password",
                    username
                );

                // Update the password to match the new one we generated
                postgres_admin::update_user_password(&pool, &username, &password)
                    .await
                    .context("Failed to update existing user password")?;

                info!("Updated password for existing database user '{}'", username);
            }

            // Change database owner to the user (gives full privileges)
            postgres_admin::change_database_owner(&pool, &database_name, &username)
                .await
                .context("Failed to change database owner")?;

            info!(
                "Changed owner of database '{}' to user '{}'",
                database_name, username
            );

            pool.close().await;

            // Store credentials in status
            let encrypted_password = self
                .encryption_provider
                .encrypt(&password)
                .await
                .context("Failed to encrypt database user password")?;

            status.databases.insert(
                database_name.clone(),
                DatabaseStatus {
                    user: username.clone(),
                    password_encrypted: encrypted_password,
                    status: DatabaseState::Available,
                    cleanup_scheduled_at: None,
                },
            );

            // Update extension status in database
            db_extensions::update_status(
                &self.db_pool,
                project_id,
                &ext.extension,
                &serde_json::to_value(&status)?,
            )
            .await
            .context("Failed to update extension status with new database user")?;

            info!(
                "Stored credentials for database '{}' in extension status",
                database_name
            );

            (username, password)
        };

        // Build the InjectedEnvVar results
        let result = self
            .build_injected_env_vars(&spec, endpoint, &database_name, &db_username, &db_password)
            .await?;

        let var_keys: Vec<&str> = result.iter().map(|v| v.key.as_str()).collect();
        info!(
            "Prepared env vars for project {} (group: {}, database: {}): {:?}",
            project.name, deployment_group, database_name, var_keys
        );

        Ok(result)
    }

    async fn preview_env_vars(
        &self,
        _project_id: Uuid,
        _deployment_group: &str,
    ) -> Result<Vec<InjectedEnvVar>> {
        // RDS credentials are protected secrets that should not be used in local development.
        // They are provisioned automatically during deployment via before_deployment().
        Ok(vec![])
    }

    fn format_status(&self, status: &Value) -> String {
        // Try to parse as AwsRdsStatus
        let parsed: AwsRdsStatus = match serde_json::from_value(status.clone()) {
            Ok(s) => s,
            Err(_) => return "Unknown".to_string(),
        };

        // Format based on state
        match parsed.state {
            RdsState::Pending => "Pending".to_string(),
            RdsState::Creating => "Creating...".to_string(),
            RdsState::Available => {
                // Show instance size if available in a nice format
                format!("Available ({})", self.instance_size)
            }
            RdsState::Deleting => "Deleting...".to_string(),
            RdsState::Deleted => "Deleted".to_string(),
            RdsState::Failed => {
                // Include error message if available
                if let Some(error) = parsed.error {
                    format!("Failed: {}", error)
                } else {
                    "Failed".to_string()
                }
            }
        }
    }

    fn description(&self) -> &str {
        "Provides a PostgreSQL database on AWS RDS"
    }

    fn documentation(&self) -> &str {
        r#"# AWS RDS PostgreSQL Extension

This extension provisions a dedicated PostgreSQL database instance on AWS RDS for your project.

## Features

- Automatic RDS instance provisioning with configurable instance size and disk
- Configurable database isolation (shared or isolated per deployment group)
- Automatic credential injection as environment variables
- Database lifecycle management with automatic cleanup
- Encrypted password storage

## Configuration

The extension accepts an optional spec with the following fields:

- `engine` (optional, default: "postgres"): Database engine type
- `engine_version` (optional): Specific PostgreSQL version (e.g., "16.2"). If not specified, uses the configured default version.
- `database_isolation` (optional, default: "shared"): Controls how databases are provisioned:
  - `"shared"`: All deployment groups use the same database (simplest setup)
  - `"isolated"`: Each deployment group gets its own empty database (true data isolation)
- `database_url_env_var` (optional, default: "DATABASE_URL"): Name of the environment variable for the database URL (e.g., "DATABASE_URL", "POSTGRES_URL"). Set to null or empty string to disable injection.
- `inject_pg_vars` (optional, default: true): Whether to inject PostgreSQL environment variables (`PGHOST`, `PGPORT`, etc.)

## Example Spec

Minimal configuration (uses all defaults):
```json
{}
```

With custom engine version and isolated databases:
```json
{
  "engine": "postgres",
  "engine_version": "16.2",
  "database_isolation": "isolated"
}
```

Custom environment variable injection:
```json
{
  "database_url_env_var": "POSTGRES_URL",
  "inject_pg_vars": false
}
```

## Database Isolation Modes

### Shared Mode (default)

All deployment groups (default, staging, etc.) use the same database. This is simpler and suitable for most applications where deployment groups represent different environments rather than isolated tenants.

- Database name: `{project}_db_default`
- All deployments share the same data
- Simplest configuration

### Isolated Mode

Each deployment group gets its own empty database. This provides true data isolation and is useful for:
- Multi-tenant applications where each deployment group represents a tenant
- Testing with separate datasets
- Scenarios requiring complete data separation between deployment groups

- Database naming: `{project}_db_{deployment_group}`
- Each deployment group has its own isolated database
- Inactive databases are automatically cleaned up after 1 hour grace period

## Environment Variables

You can configure which environment variables to inject using the extension spec:

**Database URL Variable** (default: `DATABASE_URL`):
- Configurable via `database_url_env_var` (e.g., "DATABASE_URL", "POSTGRES_URL")
- Full PostgreSQL connection string (postgres://user:password@host:port/database)
- Set to null or empty string to disable injection
- This allows multiple RDS instances to inject different environment variables (e.g., one as `DATABASE_URL`, another as `SECONDARY_DB_URL`)

**PG* Variables** (enabled by default via `inject_pg_vars: true`):
- `PGHOST`: Database hostname
- `PGPORT`: Database port (5432)
- `PGDATABASE`: Database name for this deployment
- `PGUSER`: Database username for this deployment
- `PGPASSWORD`: Database password (encrypted at rest, injected at deployment time)

The PG* variables are recognized by `psql` and most PostgreSQL client libraries, allowing you to connect
with just `psql` without any connection string arguments. **Note:** Only one RDS extension should have
`inject_pg_vars: true` enabled per project, as multiple instances would override each other.

## Initial Provisioning

Creating a new RDS instance typically takes **5-15 minutes**. No new deployments can be created until the RDS instance is available. You can monitor the provisioning status in the Extensions tab.
"#
    }

    fn spec_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "engine": {
                    "type": "string",
                    "default": "postgres",
                    "description": "Database engine (currently only 'postgres' is supported)"
                },
                "engine_version": {
                    "type": "string",
                    "default": self.default_engine_version,
                    "description": format!("PostgreSQL version (e.g., '16.2'). If not specified, uses the configured default version: {}", self.default_engine_version)
                },
                "database_url_env_var": {
                    "type": "string",
                    "default": "DATABASE_URL",
                    "description": "Environment variable name for the database URL (e.g., 'DATABASE_URL', 'POSTGRES_URL'). Set to empty string to disable injection. This allows multiple RDS instances to use different environment variable names."
                },
                "inject_pg_vars": {
                    "type": "boolean",
                    "default": true,
                    "description": "Inject PG* environment variables (PGHOST, PGPORT, PGDATABASE, PGUSER, PGPASSWORD). Note: Only one RDS extension should have this enabled per project."
                }
            }
        })
    }
}
