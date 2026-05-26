use super::controller::DeploymentUrls;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Default)]
pub enum DeploymentStatus {
    // Build/Deploy states
    #[default]
    Pending,
    Building,
    Pushing,
    Pushed, // Handoff point between CLI and controller
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
    pub project: String,          // Project ID
    pub created_by: String,       // User ID
    pub created_by_email: String, // User email for display
    #[serde(default)]
    pub status: DeploymentStatus,
    #[serde(default = "default_group")]
    pub deployment_group: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment_color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>, // RFC3339 timestamp
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
    pub is_active: bool,
    #[serde(default)]
    pub can_rollback: bool,
    #[serde(default = "default_replicas")]
    pub replicas: u32,
    #[serde(default = "default_cpu")]
    pub cpu: String,
    #[serde(default = "default_memory")]
    pub memory: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_url: Option<String>, // URL to the CI pipeline/job that created this deployment
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pull_request_url: Option<String>, // URL to the PR/MR associated with this deployment
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_repository_url: Option<String>, // HTTPS URL of the Git repository this deployment was created from
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

/// The default deployment group name
/// This group drives the overall project status and is used for primary deployments
pub const DEFAULT_DEPLOYMENT_GROUP: &str = "default";

/// Normalize a deployment group name for use in URLs and resource names.
///
/// Replaces sequences of characters that are not alphanumeric, `-`, `_`, or `.`
/// with `--` (e.g., `mr/123` → `mr--123`). The result is also trimmed so it
/// starts and ends with an alphanumeric character, satisfying the Kubernetes
/// label value regex: `(([A-Za-z0-9][-A-Za-z0-9_.]*)?[A-Za-z0-9])?`
///
/// **Collision safety**: This function is injective (collision-free) only when
/// input group names do not contain `--`. The deployment group validation in
/// `is_valid_group_name` enforces this constraint.
///
/// This matches the normalization used in the `{deployment_group}` placeholder
/// of `staging_ingress_url_template`.
pub fn normalize_deployment_group(deployment_group: &str) -> String {
    let mut result = String::new();
    let mut last_was_invalid = false;

    for ch in deployment_group.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
            result.push(ch);
            last_was_invalid = false;
        } else if !last_was_invalid {
            result.push_str("--");
            last_was_invalid = true;
        }
    }

    result
        .trim_matches(|c: char| !c.is_ascii_alphanumeric())
        .to_string()
}

/// Generate the Rise system environment variables for a deployment.
///
/// Returns `(key, value)` pairs for:
/// - `RISE_ISSUER` — Rise server URL (base URL for all Rise endpoints and JWT issuer)
/// - `RISE_APP_URL` — Canonical URL where the app is accessible
/// - `RISE_APP_URLS` — JSON array of all URLs where the app can be accessed
/// - `RISE_DEPLOYMENT_GROUP` — The deployment group name (e.g. "default", "mr/123")
/// - `RISE_DEPLOYMENT_GROUP_NORMALIZED` — The group name normalized for URLs (e.g. "mr--123")
/// - `RISE_ENVIRONMENT` — The environment name (e.g. "production", "staging"), if set
pub fn rise_system_env_vars(
    public_url: &str,
    deployment_group: &str,
    deployment_urls: &DeploymentUrls,
    environment_name: Option<&str>,
) -> Vec<(String, String)> {
    let urls_for_env: Vec<String> = if deployment_urls.all_urls.is_empty() {
        let mut combined = vec![deployment_urls.default_url.clone()];
        combined.extend(deployment_urls.custom_domain_urls.clone());
        combined
    } else {
        deployment_urls.all_urls.clone()
    };
    let app_urls_json = serde_json::to_string(&urls_for_env).unwrap_or_else(|_| "[]".to_string());

    let mut vars = vec![
        ("RISE_ISSUER".to_string(), public_url.to_string()),
        (
            "RISE_APP_URL".to_string(),
            deployment_urls.primary_url.clone(),
        ),
        ("RISE_APP_URLS".to_string(), app_urls_json),
        (
            "RISE_DEPLOYMENT_GROUP".to_string(),
            deployment_group.to_string(),
        ),
        (
            "RISE_DEPLOYMENT_GROUP_NORMALIZED".to_string(),
            normalize_deployment_group(deployment_group),
        ),
    ];

    if let Some(env_name) = environment_name {
        vars.push(("RISE_ENVIRONMENT".to_string(), env_name.to_string()));
    }

    vars
}

/// A runtime environment variable override included in a deployment request
#[derive(Debug, Deserialize, Serialize)]
pub struct EnvOverride {
    pub key: String,
    pub value: String,
    #[serde(default)]
    pub is_secret: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_protected: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Target environment name. When set, this override is only applied if the
    /// resolved deployment environment matches. `None` means the override is global.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub for_environment: Option<String>,
}

// Request to create a deployment
#[derive(Debug, Deserialize)]
pub struct CreateDeploymentRequest {
    pub project: String, // Project name
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>, // Optional pre-built image reference
    /// Deployment group. Defaults to the primary group of the target environment.
    #[serde(default)]
    pub group: Option<String>,
    /// Target environment name. If omitted, resolved from the deployment group.
    #[serde(default)]
    pub environment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<String>, // Expiration duration (e.g., '7d', '2h', '30m')
    /// HTTP port the application listens on.
    /// If not provided, uses the project's PORT env var or defaults to 8080.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_deployment: Option<String>, // Optional source deployment ID to create from
    #[serde(default)]
    pub use_source_env_vars: bool, // If true and from_deployment is set, copy env vars from source (default: false = use current project env vars)
    #[serde(default)]
    pub push_image: bool, // If true with image, CLI will pull and push image to Rise registry
    /// Runtime environment variable overrides applied after copying project/source env vars
    #[serde(default)]
    pub env_overrides: Vec<EnvOverride>,
    /// URL to the CI pipeline/job that created this deployment. Auto-detected from CI environment if not provided.
    #[serde(default)]
    pub job_url: Option<String>,
    /// URL to the pull request/merge request associated with this deployment. Auto-detected from CI environment if not provided.
    #[serde(default)]
    pub pull_request_url: Option<String>,
    /// HTTPS URL of the Git repository this deployment was created from. Auto-detected from CI environment or the local git remote if not provided.
    #[serde(default)]
    pub git_repository_url: Option<String>,
    /// Number of replicas (overrides rise.toml and platform defaults)
    #[serde(default)]
    pub replicas: Option<u32>,
    /// CPU allocation (e.g., "500m", "1") — overrides rise.toml and platform defaults
    #[serde(default)]
    pub cpu: Option<String>,
    /// Memory allocation (e.g., "256Mi", "1Gi") — overrides rise.toml and platform defaults
    #[serde(default)]
    pub memory: Option<String>,
    /// Workload-identity token audiences from `[identity]` in rise.toml.
    /// Map of { in-pod filename -> token audience }. On redeploy without an
    /// explicit value, inherited from the source deployment.
    #[serde(default)]
    pub identity_audiences: Option<std::collections::BTreeMap<String, String>>,
}

// Response from creating a deployment
#[derive(Debug, Serialize)]
pub struct CreateDeploymentResponse {
    pub deployment_id: String,
    pub image_tag: String, // Full tag: registry_url/namespace/project:deployment_id
    /// Deprecated: New clients should fetch credentials from the deployment-scoped
    /// endpoint `GET /projects/{name}/deployments/{id}/registry-credentials` instead.
    /// This field is kept for backward compatibility with older CLI versions.
    pub credentials: crate::server::registry::models::RegistryCredentials,
}

// Request to update deployment status
#[derive(Debug, Deserialize)]
pub struct UpdateDeploymentStatusRequest {
    pub status: DeploymentStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_normalize_deployment_group() {
        // Basic cases
        assert_eq!(normalize_deployment_group("default"), "default");
        assert_eq!(normalize_deployment_group("mr/123"), "mr--123");
        assert_eq!(normalize_deployment_group("mr-123"), "mr-123");

        // Leading/trailing invalid chars are trimmed to alphanumeric boundary
        assert_eq!(normalize_deployment_group("/leading"), "leading");
        assert_eq!(normalize_deployment_group("trailing/"), "trailing");
        assert_eq!(normalize_deployment_group("/both/"), "both");

        // Leading/trailing dots and underscores are also trimmed
        assert_eq!(normalize_deployment_group(".dotted."), "dotted");
        assert_eq!(normalize_deployment_group("_underscored_"), "underscored");
        assert_eq!(normalize_deployment_group("_.-mixed-._"), "mixed");

        // Consecutive invalid chars collapse to a single --
        assert_eq!(normalize_deployment_group("mr//123"), "mr--123");
        assert_eq!(normalize_deployment_group("a///b"), "a--b");

        // Empty and all-invalid inputs
        assert_eq!(normalize_deployment_group(""), "");
        assert_eq!(normalize_deployment_group("/"), "");
        assert_eq!(normalize_deployment_group("///"), "");

        // Dots and underscores in the middle are preserved
        assert_eq!(normalize_deployment_group("a.b_c"), "a.b_c");
    }

    #[test]
    fn test_rise_system_env_vars_default_group() {
        let urls = DeploymentUrls {
            default_url: "https://myapp.rise.dev".to_string(),
            primary_url: "https://myapp.rise.dev".to_string(),
            custom_domain_urls: vec![],
            all_urls: vec!["https://myapp.rise.dev".to_string()],
        };

        let vars = rise_system_env_vars("https://rise.dev", "default", &urls, None);

        let map: std::collections::HashMap<_, _> = vars.into_iter().collect();
        assert_eq!(map["RISE_ISSUER"], "https://rise.dev");
        assert_eq!(map["RISE_APP_URL"], "https://myapp.rise.dev");
        assert_eq!(map["RISE_APP_URLS"], r#"["https://myapp.rise.dev"]"#);
        assert_eq!(map["RISE_DEPLOYMENT_GROUP"], "default");
        assert_eq!(map["RISE_DEPLOYMENT_GROUP_NORMALIZED"], "default");
    }

    #[test]
    fn test_rise_system_env_vars_custom_group_with_domains() {
        let urls = DeploymentUrls {
            default_url: "https://myapp-mr--42.rise.dev".to_string(),
            primary_url: "https://custom.example.com".to_string(),
            custom_domain_urls: vec!["https://custom.example.com".to_string()],
            all_urls: vec![
                "https://myapp-mr--42.rise.dev".to_string(),
                "https://custom.example.com".to_string(),
            ],
        };

        let vars = rise_system_env_vars("https://rise.dev", "mr/42", &urls, None);

        let map: std::collections::HashMap<_, _> = vars.into_iter().collect();
        assert_eq!(map["RISE_APP_URL"], "https://custom.example.com");
        assert_eq!(
            map["RISE_APP_URLS"],
            r#"["https://myapp-mr--42.rise.dev","https://custom.example.com"]"#
        );
        assert_eq!(map["RISE_DEPLOYMENT_GROUP"], "mr/42");
        assert_eq!(map["RISE_DEPLOYMENT_GROUP_NORMALIZED"], "mr--42");
    }

    #[test]
    fn test_env_override_deserialization_defaults_is_protected_to_none() {
        let env_override: EnvOverride = serde_json::from_value(json!({
            "key": "API_KEY",
            "value": "secret",
            "is_secret": true
        }))
        .unwrap();

        assert!(env_override.is_secret);
        assert_eq!(env_override.is_protected, None);
    }

    #[test]
    fn test_env_override_deserialization_keeps_explicit_is_protected() {
        let env_override: EnvOverride = serde_json::from_value(json!({
            "key": "API_KEY",
            "value": "secret",
            "is_secret": true,
            "is_protected": false
        }))
        .unwrap();

        assert_eq!(env_override.is_protected, Some(false));
    }
}
