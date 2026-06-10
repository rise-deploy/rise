use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

/// Deployment-relevant models live in `rise-backend-core` (the deployment-backend
/// contract seam) and are re-exported here so the rest of the backend keeps using
/// the familiar `crate::db::models::*` paths.
pub use rise_backend_core::models::{
    CustomDomain, Deployment, DeploymentEnvVar, DeploymentStatus, EnvVarSource, Environment,
    Project, ProjectEnvVar, ProjectStatus, TerminationReason,
};

/// User model - represents authenticated users from Dex
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct User {
    pub id: Uuid,
    pub email: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
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
