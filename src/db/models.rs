use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use std::borrow::Cow;
use uuid::Uuid;

/// User model - represents authenticated users from Dex
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct User {
    pub id: Uuid,
    pub email: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Project model - represents deployable applications
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Project {
    pub id: Uuid,
    pub name: String,
    pub status: ProjectStatus,
    pub access_class: String,
    pub owner_user_id: Option<Uuid>,
    pub owner_team_id: Option<Uuid>,
    /// Finalizers that must be removed before the project can be deleted.
    /// Each controller adds its own finalizer when it creates external resources.
    pub finalizers: Vec<String>,
    /// URL to where the project code lives (e.g. a GitHub/GitLab repository)
    pub source_url: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Project status enum
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "text")]
pub enum ProjectStatus {
    Stopped,
    Running,
    Failed,
    Deploying,
    Deleting,
    Terminated,
}

impl std::fmt::Display for ProjectStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProjectStatus::Stopped => write!(f, "Stopped"),
            ProjectStatus::Running => write!(f, "Running"),
            ProjectStatus::Failed => write!(f, "Failed"),
            ProjectStatus::Deploying => write!(f, "Deploying"),
            ProjectStatus::Deleting => write!(f, "Deleting"),
            ProjectStatus::Terminated => write!(f, "Terminated"),
        }
    }
}

/// Team model - represents groups of users
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Team {
    pub id: Uuid,
    pub name: String,
    /// Whether this team is managed by an Identity Provider
    /// When true, membership is controlled by IdP groups claim
    pub idp_managed: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Team member model - junction table for team membership
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct TeamMember {
    pub team_id: Uuid,
    pub user_id: Uuid,
    pub role: TeamRole,
    pub created_at: DateTime<Utc>,
}

/// Team role enum
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::Type, PartialEq)]
#[sqlx(type_name = "text")]
pub enum TeamRole {
    #[sqlx(rename = "owner")]
    Owner,
    #[sqlx(rename = "member")]
    Member,
}

impl std::fmt::Display for TeamRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TeamRole::Owner => write!(f, "owner"),
            TeamRole::Member => write!(f, "member"),
        }
    }
}

/// Service Account model - represents workload identity for CI/CD automation
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ServiceAccount {
    pub id: Uuid,
    pub project_id: Uuid,
    pub user_id: Uuid,
    pub issuer_url: String,
    pub claims: serde_json::Value, // JSONB stored as serde_json::Value
    pub sequence: i32,
    /// If set, restricts which environments this SA can deploy to; NULL means any environment
    pub allowed_environment_ids: Option<Vec<Uuid>>,
    pub deleted_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Environment model - represents a deployment environment within a project
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Environment {
    pub id: Uuid,
    pub project_id: Uuid,
    pub name: String,
    pub primary_deployment_group: Option<String>,
    pub is_production: bool,
    pub color: String,
    /// Per-environment deployment constraints (NULL = inherit platform defaults)
    pub min_replicas: Option<i32>,
    pub max_replicas: Option<i32>,
    pub min_cpu: Option<String>,
    pub max_cpu: Option<String>,
    pub min_memory: Option<String>,
    pub max_memory: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Deployment model - represents application deployments
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Deployment {
    pub id: Uuid,
    pub deployment_id: String,
    pub project_id: Uuid,
    pub created_by_id: Uuid,
    pub status: DeploymentStatus,
    pub deployment_group: String,
    pub environment_id: Option<Uuid>,
    pub expires_at: Option<DateTime<Utc>>,
    pub termination_reason: Option<TerminationReason>,
    pub completed_at: Option<DateTime<Utc>>,
    pub error_message: Option<String>,
    pub build_logs: Option<String>,
    pub controller_metadata: serde_json::Value,
    pub image: Option<String>,
    pub image_digest: Option<String>,
    pub rolled_back_from_deployment_id: Option<Uuid>,
    pub http_port: i32,
    pub needs_reconcile: bool,
    pub is_active: bool,
    pub deploying_started_at: Option<DateTime<Utc>>,
    pub first_healthy_at: Option<DateTime<Utc>>,
    /// URL to the CI pipeline/job that created this deployment
    pub job_url: Option<String>,
    /// URL to the pull request/merge request associated with this deployment
    pub pull_request_url: Option<String>,
    /// HTTPS URL of the Git repository this deployment was created from
    pub git_repository_url: Option<String>,
    /// Number of replicas for this deployment
    pub replicas: i32,
    /// CPU allocation (e.g., "500m", "1")
    pub cpu: String,
    /// Memory allocation (e.g., "256Mi", "1Gi")
    pub memory: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Deployment status enum - tracks lifecycle of deployment
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::Type, PartialEq)]
#[sqlx(type_name = "text")]
pub enum DeploymentStatus {
    // Build/Deploy states (pre-infrastructure)
    Pending,
    Building,
    Pushing,
    Pushed, // Handoff point between CLI and controller
    Deploying,

    // Running states (post-infrastructure)
    Healthy,   // Running and passing health checks
    Unhealthy, // Running but failing health checks

    // Cancellation states (pre-infrastructure)
    Cancelling, // Being cancelled before infrastructure provisioned
    Cancelled,  // Terminal: cancelled before infrastructure provisioned

    // Termination states (post-infrastructure)
    Terminating, // Being gracefully terminated
    Stopped,     // Terminal: user-initiated termination
    Superseded,  // Terminal: replaced by newer deployment

    // Terminal states
    Failed,  // Terminal: could not reach Healthy
    Expired, // Terminal: deployment expired after reaching Healthy
}

impl std::fmt::Display for DeploymentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeploymentStatus::Pending => write!(f, "Pending"),
            DeploymentStatus::Building => write!(f, "Building"),
            DeploymentStatus::Pushing => write!(f, "Pushing"),
            DeploymentStatus::Pushed => write!(f, "Pushed"),
            DeploymentStatus::Deploying => write!(f, "Deploying"),
            DeploymentStatus::Healthy => write!(f, "Healthy"),
            DeploymentStatus::Unhealthy => write!(f, "Unhealthy"),
            DeploymentStatus::Cancelling => write!(f, "Cancelling"),
            DeploymentStatus::Cancelled => write!(f, "Cancelled"),
            DeploymentStatus::Terminating => write!(f, "Terminating"),
            DeploymentStatus::Stopped => write!(f, "Stopped"),
            DeploymentStatus::Superseded => write!(f, "Superseded"),
            DeploymentStatus::Failed => write!(f, "Failed"),
            DeploymentStatus::Expired => write!(f, "Expired"),
        }
    }
}

/// Termination reason enum - tracks why deployment was terminated
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::Type, PartialEq)]
#[sqlx(type_name = "termination_reason")]
pub enum TerminationReason {
    UserStopped, // User explicitly stopped the deployment
    Superseded,  // Replaced by newer deployment
    Cancelled,   // Cancelled before infrastructure provisioned
    Failed,      // Deployment timed out or failed to become healthy
    Expired,     // Deployment expired after specified time limit
}

impl std::fmt::Display for TerminationReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TerminationReason::UserStopped => write!(f, "UserStopped"),
            TerminationReason::Superseded => write!(f, "Superseded"),
            TerminationReason::Cancelled => write!(f, "Cancelled"),
            TerminationReason::Failed => write!(f, "Failed"),
            TerminationReason::Expired => write!(f, "Expired"),
        }
    }
}

/// Project environment variable
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ProjectEnvVar {
    pub id: Uuid,
    pub project_id: Uuid,
    pub key: String,
    /// Encrypted value if is_secret = true
    pub value: String,
    pub is_secret: bool,
    pub is_protected: bool,
    /// If set, this env var is scoped to a specific environment; NULL means global
    pub environment_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Provenance of a deployment environment variable.
///
/// Stored as TEXT in the database. Parsed/validated via `EnvVarSource`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvVarSource {
    /// Rise system variables (PORT, RISE_ISSUER, RISE_APP_URL, etc.)
    System,
    /// Copied from project-level env vars with no environment scope
    Global,
    /// Copied from project-level env vars scoped to a specific environment
    Env(String),
    /// Injected by an extension (OAuth, RDS, Snowflake, etc.)
    Extension,
    /// From rise.toml configuration file
    Toml,
    /// From CLI flags (--env, --secret-env, --protected-env, --env-file)
    Cli,
}

impl EnvVarSource {
    /// Serialize to the string stored in the database.
    pub fn as_str(&self) -> Cow<'_, str> {
        match self {
            Self::System => Cow::Borrowed("system"),
            Self::Global => Cow::Borrowed("global"),
            Self::Env(name) => Cow::Owned(format!("env:{}", name)),
            Self::Extension => Cow::Borrowed("extension"),
            Self::Toml => Cow::Borrowed("toml"),
            Self::Cli => Cow::Borrowed("cli"),
        }
    }

    /// Parse from the string stored in the database.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "system" => Some(Self::System),
            "global" => Some(Self::Global),
            "extension" => Some(Self::Extension),
            "toml" => Some(Self::Toml),
            "cli" => Some(Self::Cli),
            _ => s
                .strip_prefix("env:")
                .map(|name| Self::Env(name.to_string())),
        }
    }

    /// Returns the set of source values that are allowed from client requests.
    /// Server-managed sources (system, global, env:*, extension) are not accepted from clients.
    pub fn is_client_allowed(&self) -> bool {
        matches!(self, Self::Toml | Self::Cli)
    }
}

impl std::fmt::Display for EnvVarSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Deployment environment variable
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct DeploymentEnvVar {
    pub id: Uuid,
    pub deployment_id: Uuid,
    pub key: String,
    /// Encrypted value if is_secret = true
    pub value: String,
    pub is_secret: bool,
    pub is_protected: bool,
    /// Provenance tracking for where this env var came from.
    /// Stored as TEXT, parsed via [`EnvVarSource`].
    pub source: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Custom domain for projects
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct CustomDomain {
    pub id: Uuid,
    pub project_id: Uuid,
    pub domain: String,
    pub is_primary: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Project extension - represents external resources provisioned for a project
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ProjectExtension {
    pub project_id: Uuid,
    pub extension: String,
    pub extension_type: String,
    pub spec: serde_json::Value,
    pub status: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_var_source_roundtrip() {
        let variants = [
            EnvVarSource::System,
            EnvVarSource::Global,
            EnvVarSource::Env("staging".to_string()),
            EnvVarSource::Extension,
            EnvVarSource::Toml,
            EnvVarSource::Cli,
        ];
        for variant in &variants {
            let s = variant.as_str();
            let parsed = EnvVarSource::parse(&s).unwrap_or_else(|| {
                panic!("parse failed for {:?} (serialized as {:?})", variant, s)
            });
            assert_eq!(&parsed, variant);
        }
    }

    #[test]
    fn env_var_source_parse_unknown_returns_none() {
        assert_eq!(EnvVarSource::parse("unknown"), None);
        assert_eq!(EnvVarSource::parse(""), None);
        assert_eq!(EnvVarSource::parse("env"), None);
    }

    #[test]
    fn env_var_source_is_client_allowed() {
        assert!(EnvVarSource::Toml.is_client_allowed());
        assert!(EnvVarSource::Cli.is_client_allowed());

        assert!(!EnvVarSource::System.is_client_allowed());
        assert!(!EnvVarSource::Global.is_client_allowed());
        assert!(!EnvVarSource::Env("prod".to_string()).is_client_allowed());
        assert!(!EnvVarSource::Extension.is_client_allowed());
    }
}
