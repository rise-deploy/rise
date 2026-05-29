//! Shared API request/response types
//!
//! These types are used by both the CLI (client) and server for API communication.
//! They are always available regardless of feature flags.

// Re-export from server deployment models when server feature is enabled
#[cfg(feature = "backend")]
pub use crate::server::deployment::models::*;

// When server feature is NOT enabled, define the types here for CLI use
#[cfg(not(feature = "backend"))]
pub use self::client_models::*;

#[cfg(not(feature = "backend"))]
mod client_models {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Default)]
    pub enum DeploymentStatus {
        // Build/Deploy states
        #[default]
        Pending,
        Building,
        Pushing,
        Pushed,
        Deploying,

        // Running states
        Healthy,
        Unhealthy,

        // Cancellation states
        Cancelling,
        Cancelled,

        // Termination states
        Terminating,
        Stopped,
        Superseded,

        // Terminal states
        Failed,
        Expired,
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

    #[derive(Debug, Deserialize, Serialize, Clone, Default)]
    pub struct Deployment {
        #[serde(default)]
        pub id: String,
        pub deployment_id: String,
        pub project: String,
        pub created_by: String,
        pub created_by_email: String,
        #[serde(default)]
        pub status: DeploymentStatus,
        #[serde(default = "default_group")]
        pub deployment_group: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub environment: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub expires_at: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub error_message: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub completed_at: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub build_logs: Option<String>,
        #[serde(default)]
        pub controller_metadata: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub primary_url: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub custom_domain_urls: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub image: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub image_digest: Option<String>,
        #[serde(default)]
        pub http_port: u16,
        #[serde(default)]
        pub can_rollback: bool,
        #[serde(default = "default_replicas")]
        pub replicas: u32,
        #[serde(default = "default_cpu")]
        pub cpu: String,
        #[serde(default = "default_memory")]
        pub memory: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub job_url: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub pull_request_url: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub git_repository_url: Option<String>,
        #[serde(default)]
        pub created: String,
        #[serde(default)]
        pub updated: String,
    }

    fn default_group() -> String {
        DEFAULT_DEPLOYMENT_GROUP.to_string()
    }

    fn default_replicas() -> u32 {
        1
    }

    fn default_cpu() -> String {
        "500m".to_string()
    }

    fn default_memory() -> String {
        "256Mi".to_string()
    }

    pub const DEFAULT_DEPLOYMENT_GROUP: &str = "default";

    // ── Multi-container request wire types ──────────────────────────────────
    //
    // CLI-only mirrors of the server's request structs in
    // `server::deployment::models`. With `--features backend` the glob
    // re-export above supplies the real server types instead, so these must
    // stay byte-for-byte wire-identical (field names, types, and serde
    // skip/default rules) to what the server deserializes. The CLI only ever
    // serializes these, but they derive `Deserialize` too so they line up with
    // the server derives under a combined build.

    /// Request-side env var override. Mirrors `server::deployment::models::EnvOverride`.
    #[derive(Debug, Deserialize, Serialize, Clone)]
    pub struct EnvOverride {
        pub key: String,
        pub value: String,
        #[serde(default)]
        pub is_secret: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub is_protected: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub source: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub for_environment: Option<String>,
    }

    /// Per-container probe config. Mirrors `server::deployment::models::HealthCheckSpec`.
    /// A single flat struct keyed by `disabled` — NOT an untagged enum: the
    /// server reads `health_check = false` as `{ "disabled": true }`.
    #[derive(Debug, Deserialize, Serialize, Clone, Default)]
    pub struct HealthCheckSpec {
        #[serde(default)]
        pub disabled: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub initial_delay_seconds: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub period_seconds: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub timeout_seconds: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub failure_threshold: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub liveness_enabled: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub readiness_enabled: Option<bool>,
    }

    /// One container in a multi-container request.
    /// Mirrors `server::deployment::models::ContainerSpec`.
    #[derive(Debug, Deserialize, Serialize, Clone)]
    pub struct ContainerSpec {
        pub name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub image: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub port: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub replicas: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub cpu: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub memory: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub env_overrides: Vec<EnvOverride>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub health_check: Option<HealthCheckSpec>,
    }

    /// One ingress route mapping. Mirrors `server::deployment::models::RouteSpec`.
    #[derive(Debug, Deserialize, Serialize, Clone)]
    pub struct RouteSpec {
        pub path: String,
        pub container: String,
    }
}
