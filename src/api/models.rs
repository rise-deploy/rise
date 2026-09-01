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

/// One event recorded in a deployment's append-only history.
#[derive(Debug, serde::Deserialize, Clone)]
pub struct DeploymentEvent {
    pub id: i64,
    pub occurred_at: String,
    pub kind: String,
    pub severity: String,
    pub source: String,
    pub subject: Option<String>,
    pub message: Option<String>,
    pub attributes: serde_json::Value,
}

#[derive(Debug, serde::Deserialize)]
pub struct DeploymentEventPage {
    pub events: Vec<DeploymentEvent>,
    pub next_cursor: Option<String>,
}

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
        /// Opaque controller bookkeeping, surfaced for introspection only.
        ///
        /// Each backend writes whatever it needs to track its own convergence — the
        /// ECS reconciler records the task-definition hash its service settled on.
        /// The shape is the controller's business and changes with it, so nothing
        /// outside the controller may depend on the keys inside: read it to see what
        /// a controller is thinking, never to drive behaviour.
        #[serde(default)]
        pub controller_metadata: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub primary_url: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub custom_domain_urls: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub all_urls: Vec<String>,
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
    // Shared with the backend request/runtime model so the CLI and server keep
    // byte-identical JSON field names and serde default/skip behavior.
    pub use rise_deployment_spec::request_spec::{
        ContainerSpec, EnvOverride, HealthCheckSpec, RouteSpec,
    };
}
