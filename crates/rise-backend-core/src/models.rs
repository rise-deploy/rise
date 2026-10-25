//! Deployment-relevant data models shared between `rise-deploy` and the
//! deployment backends.
//!
//! These are the SQLX row structs the backends operate on. They derive
//! `FromRow` / `sqlx::Type` so `rise-deploy`'s `db` helpers can map query
//! results directly, but this crate never executes a query itself.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use std::borrow::Cow;
use uuid::Uuid;

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
    /// ID of the quickstart template the project was created from, if any.
    pub template_id: Option<String>,
    /// Image:tag reference of the template at the time of the most recent
    /// template-driven deploy. Used to detect when the catalog has moved
    /// ahead of the deployed version.
    pub template_image: Option<String>,
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
    /// Caps `expires_at` of deployments created into a group other than
    /// `primary_deployment_group` (or into any group, when unset). Canonical
    /// `Nd|Nh|Nm`; `NULL` means no cap.
    pub max_deployment_expiration: Option<String>,
    /// The only environment deployments may be promoted from into this one
    /// (ADR-0007). `None` admits deployments from anywhere, fresh builds included.
    pub promotion_source_environment_id: Option<Uuid>,
    /// Whether the Rollback Guard replaces a primary-group deployment that
    /// stays `Unhealthy` for `rollback_guard_unhealthy_timeout_seconds`.
    pub rollback_guard_enabled: bool,
    pub rollback_guard_unhealthy_timeout_seconds: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// How a deployment came to be (ADR-0007).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum CreatedVia {
    /// A fresh build or a pre-built `--image`.
    Deploy,
    /// Created from another deployment in the same environment.
    Redeploy,
    /// Created from a deployment in another environment.
    Promote,
    /// An automatic Rollback Guard replacement of a sustained-`Unhealthy` deployment.
    RollbackGuard,
}

impl CreatedVia {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Deploy => "deploy",
            Self::Redeploy => "redeploy",
            Self::Promote => "promote",
            Self::RollbackGuard => "rollback_guard",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "deploy" => Some(Self::Deploy),
            "redeploy" => Some(Self::Redeploy),
            "promote" => Some(Self::Promote),
            "rollback_guard" => Some(Self::RollbackGuard),
            _ => None,
        }
    }
}

impl std::fmt::Display for CreatedVia {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
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
    pub environment_id: Uuid,
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
    /// Deprecated: nothing reads or writes this anymore — domain/config
    /// changes now trigger a `RiseProject` CRD resync (see
    /// `crd::trigger_resync`) instead of flipping a row-level flag. The
    /// column is still selected here because it's still on the table; drop
    /// both in a follow-up migration.
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
    /// SHA-256 hash of the per-deployment workload-identity bootstrap credential.
    /// NULL until the controller has reconciled the deployment.
    pub identity_credential_hash: Option<String>,
    /// Map of { in-pod filename -> token audience } for auto-minted workload JWTs.
    pub identity_audiences: serde_json::Value,
    /// Multi-container side-data (`Vec<ContainerSpec>` as JSONB) or `None` for
    /// legacy single-container deployments. Folded onto the row so callers don't
    /// need a separate query to read it.
    pub containers: Option<serde_json::Value>,
    /// Ingress route map (`Vec<RouteSpec>` as JSONB) or `None`. Always `None`
    /// when `containers` is `None`.
    pub routes: Option<serde_json::Value>,
    /// How this deployment was created.
    pub created_via: CreatedVia,
    /// The deployment this one was promoted from (the immediate source, set
    /// only when it crossed environments). The image origin stays
    /// `rolled_back_from_deployment_id`.
    pub promoted_from_deployment_id: Option<Uuid>,
    pub promoted_from_environment_id: Option<Uuid>,
    /// Migration compatibility version of the code (`[project].migration_version`),
    /// inherited by every deployment created from another one. `None` = undeclared.
    pub migration_version: Option<i64>,
    /// When the deployment last moved into `Unhealthy`; cleared when it recovers to `Healthy`.
    pub unhealthy_since: Option<DateTime<Utc>>,
    /// Set only on a Rollback Guard replacement: the unhealthy deployment it replaces.
    pub rollback_guard_for_deployment_id: Option<Uuid>,
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
    /// From rise.toml `[environments.<name>.env]`: scoped to that environment,
    /// so it is carried only into deployments of the same environment.
    TomlEnv(String),
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
            Self::TomlEnv(name) => Cow::Owned(format!("toml:{}", name)),
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
            _ => {
                if let Some(name) = s.strip_prefix("toml:") {
                    Some(Self::TomlEnv(name.to_string()))
                } else {
                    s.strip_prefix("env:")
                        .map(|name| Self::Env(name.to_string()))
                }
            }
        }
    }

    /// Returns the set of source values that are allowed from client requests.
    /// Server-managed sources (system, global, env:*, toml:*, extension) are not
    /// accepted from clients; the server derives `toml:<env>` from a `toml`
    /// override's target environment.
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
    pub environment_id: Uuid,
    pub domain: String,
    pub is_primary: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
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
            EnvVarSource::TomlEnv("production".to_string()),
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
        assert!(!EnvVarSource::TomlEnv("prod".to_string()).is_client_allowed());
    }

    #[test]
    fn env_var_source_toml_env_serializes_with_prefix() {
        assert_eq!(
            EnvVarSource::TomlEnv("staging".to_string()).as_str(),
            "toml:staging"
        );
        assert_eq!(
            EnvVarSource::parse("toml:staging"),
            Some(EnvVarSource::TomlEnv("staging".to_string()))
        );
    }

    #[test]
    fn created_via_roundtrip() {
        for v in [
            CreatedVia::Deploy,
            CreatedVia::Redeploy,
            CreatedVia::Promote,
            CreatedVia::RollbackGuard,
        ] {
            assert_eq!(CreatedVia::parse(v.as_str()), Some(v));
            let json = serde_json::to_value(v).unwrap();
            assert_eq!(json, serde_json::Value::String(v.as_str().to_string()));
        }
        assert_eq!(CreatedVia::parse("other"), None);
    }
}
