use crate::server::error::ServerError;
pub use rise_deployment_spec::request_spec::{
    ContainerSpec, EnvOverride, HealthCheckSpec, RouteSpec,
};
pub use rise_deployment_spec::side_data::{decode_side_data, encode_side_data};
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
    /// Full ordered list of every URL the deployment is reachable at — deployment-group
    /// URL, environment URL, production URL, and any custom domains, deduplicated. New
    /// frontend code prefers this over the narrower `primary_url`/`custom_domain_urls`
    /// pair, which double-counts when `primary_url` is itself a custom domain.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub all_urls: Vec<String>,
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
    /// Multi-container side-data. `None` for legacy single-container deployments
    /// (`replicas`/`cpu`/`memory` are the source of truth in that case).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub containers: Option<Vec<ContainerSpec>>,
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

// The default deployment group name, the group-name normalizer, and the Rise
// system env-var builder live in `rise-backend-core` (shared with the deployment
// backends); re-exported here so existing `crate::server::deployment::models::*`
// paths keep working.
pub use rise_backend_core::group::{normalize_deployment_group, DEFAULT_DEPLOYMENT_GROUP};
pub use rise_backend_core::system_env::rise_system_env_vars;

/// Validate the wire-level multi-container spec (containers + routes) from a
/// `CreateDeploymentRequest`. Returns `Ok(())` for legacy single-container
/// requests (`containers` is `None`).
pub fn validate_containers_and_routes(
    containers: Option<&[ContainerSpec]>,
    routes: &[RouteSpec],
) -> Result<(), ServerError> {
    rise_deployment_spec::validation::validate_containers_and_routes(containers, routes)
        .map_err(|e| ServerError::bad_request(e.message))
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
    /// Multi-container deployment spec. When present, the per-deployment
    /// `image`/`http_port`/`replicas`/`cpu`/`memory` fields above are ignored —
    /// every container declares its own. `None` keeps the legacy
    /// single-container behaviour (back-compat for older CLIs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub containers: Option<Vec<ContainerSpec>>,
    /// Ingress route map (`path` → `container`). Only meaningful when
    /// `containers` is set. Order doesn't matter — the reconciler sorts by
    /// path length descending for nginx longest-prefix-first semantics.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<RouteSpec>,
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
    /// Multi-container deployments only: map of container name → the
    /// fully-qualified client-facing image tag the CLI should build and push
    /// for that container. Server-derived tags share the project's repository
    /// and the returned `credentials` are minted to cover every entry in this
    /// map. Containers with a pre-built image (the request set `image`)
    /// appear with the user-supplied value so the CLI can skip the push.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_images: Option<std::collections::BTreeMap<String, String>>,
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
    use crate::server::deployment::controller::DeploymentUrls;
    use rise_deployment_spec::side_data::CONTAINER_SIDE_DATA_VERSION;
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

    fn cspec(name: &str, image: Option<&str>, port: Option<u16>) -> ContainerSpec {
        ContainerSpec {
            name: name.to_string(),
            image: image.map(|s| s.to_string()),
            port,
            replicas: None,
            cpu: None,
            memory: None,
            env_overrides: vec![],
            health_check: None,
        }
    }

    #[test]
    fn test_validate_legacy_request_passes() {
        assert!(validate_containers_and_routes(None, &[]).is_ok());
    }

    #[test]
    fn test_validate_legacy_request_with_routes_fails() {
        let routes = vec![RouteSpec {
            path: "/".to_string(),
            container: "app".to_string(),
        }];
        assert!(validate_containers_and_routes(None, &routes).is_err());
    }

    #[test]
    fn test_validate_empty_containers_list_fails() {
        let result = validate_containers_and_routes(Some(&[]), &[]);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_basic_multi_container_passes() {
        let containers = vec![
            cspec("api", Some("nginx:latest"), Some(8080)),
            cspec("worker", Some("busybox:latest"), None),
        ];
        let routes = vec![RouteSpec {
            path: "/".to_string(),
            container: "api".to_string(),
        }];
        validate_containers_and_routes(Some(&containers), &routes).unwrap();
    }

    #[test]
    fn test_validate_invalid_container_name_uppercase() {
        let containers = vec![cspec("API", Some("nginx"), Some(8080))];
        let err = validate_containers_and_routes(Some(&containers), &[]).unwrap_err();
        assert!(
            err.message.contains("Invalid container name"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn test_validate_invalid_container_name_too_long() {
        let containers = vec![cspec("aaaaaaaaaaaaaaaa", Some("nginx"), Some(8080))];
        assert!(validate_containers_and_routes(Some(&containers), &[]).is_err());
    }

    #[test]
    fn test_validate_invalid_container_name_trailing_dash() {
        let containers = vec![cspec("api-", Some("nginx"), Some(8080))];
        assert!(validate_containers_and_routes(Some(&containers), &[]).is_err());
    }

    #[test]
    fn test_validate_container_name_15_chars_ok() {
        let containers = vec![cspec("abcdefghijklmno", Some("nginx"), Some(8080))];
        validate_containers_and_routes(Some(&containers), &[]).unwrap();
    }

    #[test]
    fn test_validate_container_name_empty_fails() {
        let containers = vec![cspec("", Some("nginx"), Some(8080))];
        let err = validate_containers_and_routes(Some(&containers), &[]).unwrap_err();
        assert!(
            err.message.contains("Invalid container name"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn test_validate_container_name_single_char_ok() {
        let containers = vec![cspec("a", Some("nginx"), Some(8080))];
        validate_containers_and_routes(Some(&containers), &[]).unwrap();
    }

    #[test]
    fn test_validate_duplicate_container_names_fails() {
        let containers = vec![
            cspec("api", Some("nginx"), Some(8080)),
            cspec("api", Some("nginx"), Some(9090)),
        ];
        let err = validate_containers_and_routes(Some(&containers), &[]).unwrap_err();
        assert!(err.message.contains("Duplicate"), "got: {}", err.message);
    }

    #[test]
    fn test_validate_empty_image_string_fails() {
        let containers = vec![cspec("api", Some(""), Some(8080))];
        let err = validate_containers_and_routes(Some(&containers), &[]).unwrap_err();
        assert!(err.message.contains("empty image"), "got: {}", err.message);
    }

    #[test]
    fn test_validate_health_check_requires_http_port() {
        let mut spec = cspec("api", Some("nginx"), None);
        spec.health_check = Some(HealthCheckSpec::default());
        let err = validate_containers_and_routes(Some(&[spec]), &[]).unwrap_err();
        assert!(err.message.contains("health_check"), "got: {}", err.message);
    }

    #[test]
    fn test_validate_route_to_unknown_container_fails() {
        let containers = vec![cspec("api", Some("nginx"), Some(8080))];
        let routes = vec![RouteSpec {
            path: "/".to_string(),
            container: "ghost".to_string(),
        }];
        let err = validate_containers_and_routes(Some(&containers), &routes).unwrap_err();
        assert!(
            err.message.contains("unknown container"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn test_validate_route_to_worker_fails() {
        let containers = vec![cspec("worker", Some("busybox"), None)];
        let routes = vec![RouteSpec {
            path: "/".to_string(),
            container: "worker".to_string(),
        }];
        let err = validate_containers_and_routes(Some(&containers), &routes).unwrap_err();
        assert!(err.message.contains("no port"), "got: {}", err.message);
    }

    #[test]
    fn test_validate_route_path_missing_leading_slash_fails() {
        let containers = vec![cspec("api", Some("nginx"), Some(8080))];
        let routes = vec![RouteSpec {
            path: "api".to_string(),
            container: "api".to_string(),
        }];
        let err = validate_containers_and_routes(Some(&containers), &routes).unwrap_err();
        assert!(
            err.message.contains("must start with '/'"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn test_validate_route_path_reserved_rise_prefix_rejected() {
        let containers = vec![cspec("api", Some("nginx"), Some(8080))];
        for reserved in ["/.rise", "/.rise/auth"] {
            let routes = vec![RouteSpec {
                path: reserved.to_string(),
                container: "api".to_string(),
            }];
            let err = validate_containers_and_routes(Some(&containers), &routes).unwrap_err();
            assert!(
                err.message.contains("reserved") && err.message.contains("/.rise"),
                "got: {}",
                err.message
            );
        }
    }

    #[test]
    fn test_validate_route_path_normal_accepted() {
        let containers = vec![cspec("api", Some("nginx"), Some(8080))];
        let routes = vec![RouteSpec {
            path: "/api".to_string(),
            container: "api".to_string(),
        }];
        validate_containers_and_routes(Some(&containers), &routes).unwrap();
    }

    #[test]
    fn test_validate_route_path_invalid_charset_rejected() {
        let containers = vec![cspec("api", Some("nginx"), Some(8080))];
        for bad in ["/has space", "/has\"quote"] {
            let routes = vec![RouteSpec {
                path: bad.to_string(),
                container: "api".to_string(),
            }];
            let err = validate_containers_and_routes(Some(&containers), &routes).unwrap_err();
            assert!(
                err.message.contains("invalid characters"),
                "got: {}",
                err.message
            );
        }
    }

    #[test]
    fn test_validate_per_container_secret_override_rejected() {
        let mut spec = cspec("api", Some("nginx"), Some(8080));
        spec.env_overrides.push(EnvOverride {
            key: "API_KEY".to_string(),
            value: "secret".to_string(),
            is_secret: true,
            is_protected: None,
            source: None,
            for_environment: None,
        });
        let err = validate_containers_and_routes(Some(&[spec]), &[]).unwrap_err();
        assert!(
            err.message.contains("secret env overrides"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn create_deployment_request_deserializes_legacy_absent_and_null_containers() {
        let absent: CreateDeploymentRequest = serde_json::from_value(serde_json::json!({
            "project": "app"
        }))
        .unwrap();
        assert!(absent.containers.is_none());
        assert!(absent.routes.is_empty());

        let null_containers: CreateDeploymentRequest = serde_json::from_value(serde_json::json!({
            "project": "app",
            "containers": null
        }))
        .unwrap();
        assert!(null_containers.containers.is_none());
        assert!(null_containers.routes.is_empty());
    }

    #[test]
    fn create_deployment_request_deserializes_empty_containers_as_invalid_present_list() {
        let request: CreateDeploymentRequest = serde_json::from_value(serde_json::json!({
            "project": "app",
            "containers": [],
            "routes": []
        }))
        .unwrap();
        assert!(request.containers.as_ref().is_some_and(Vec::is_empty));
        let err = validate_containers_and_routes(request.containers.as_deref(), &request.routes)
            .unwrap_err();
        assert!(err.message.contains("must not be empty"));
    }

    // ── Versioned side-data envelope (encode/decode round-trip) ─────────────

    #[test]
    fn side_data_round_trips_through_versioned_envelope() {
        // Every persisted field populated, so a future field drop/rename shows up
        // here as a round-trip mismatch.
        let specs = vec![
            ContainerSpec {
                name: "api".to_string(),
                image: Some("img-api".to_string()),
                port: Some(8080),
                replicas: Some(3),
                cpu: Some("500m".to_string()),
                memory: Some("256Mi".to_string()),
                env_overrides: vec![EnvOverride {
                    key: "K".to_string(),
                    value: "V".to_string(),
                    is_secret: false,
                    is_protected: Some(true),
                    source: Some("toml".to_string()),
                    for_environment: Some("production".to_string()),
                }],
                health_check: Some(HealthCheckSpec {
                    disabled: false,
                    path: Some("/health".to_string()),
                    initial_delay_seconds: Some(5),
                    period_seconds: Some(10),
                    timeout_seconds: Some(2),
                    failure_threshold: Some(3),
                    liveness_enabled: Some(true),
                    readiness_enabled: Some(false),
                }),
            },
            ContainerSpec {
                name: "worker".to_string(),
                image: Some("img-worker".to_string()),
                port: None,
                replicas: Some(1),
                cpu: None,
                memory: None,
                env_overrides: vec![],
                health_check: None,
            },
        ];

        let encoded = encode_side_data(&specs).expect("encode");
        // The on-disk shape is a versioned envelope, never a bare array.
        assert_eq!(encoded["version"], CONTAINER_SIDE_DATA_VERSION);
        assert!(encoded["items"].is_array());

        let decoded: Vec<ContainerSpec> = decode_side_data(&encoded).expect("decode");
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].name, "api");
        assert_eq!(decoded[0].port, Some(8080));
        assert_eq!(decoded[0].env_overrides[0].is_protected, Some(true));
        let hc = decoded[0].health_check.as_ref().unwrap();
        assert_eq!(hc.path.as_deref(), Some("/health"));
        assert_eq!(hc.readiness_enabled, Some(false));
        assert_eq!(decoded[1].name, "worker");
        assert_eq!(decoded[1].port, None);

        // Routes round-trip through the same envelope.
        let routes = vec![RouteSpec {
            path: "/api".to_string(),
            container: "api".to_string(),
        }];
        let routes_decoded: Vec<RouteSpec> =
            decode_side_data(&encode_side_data(&routes).unwrap()).unwrap();
        assert_eq!(routes_decoded.len(), 1);
        assert_eq!(routes_decoded[0].path, "/api");
    }

    #[test]
    fn side_data_decode_tolerates_unknown_item_fields() {
        // Additive evolution: an item carrying a field this build doesn't know
        // must still decode (no deny_unknown_fields), so old readers survive a
        // new optional field without a version bump.
        let envelope = serde_json::json!({
            "version": CONTAINER_SIDE_DATA_VERSION,
            "items": [ { "name": "api", "port": 8080, "future_field": 123 } ],
        });
        let decoded: Vec<ContainerSpec> = decode_side_data(&envelope).expect("decode");
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].name, "api");
    }

    #[test]
    fn side_data_decode_rejects_unknown_version() {
        // A future/unknown version is a hard error (callers map it to Failed /
        // 500 / render-without) rather than silently decoding the wrong shape.
        let envelope = serde_json::json!({
            "version": CONTAINER_SIDE_DATA_VERSION + 1,
            "items": [ { "name": "api", "port": 8080 } ],
        });
        let err = decode_side_data::<ContainerSpec>(&envelope).unwrap_err();
        assert!(
            format!("{err:?}").contains("unsupported container side-data version"),
            "got: {err:?}"
        );
    }

    #[test]
    fn side_data_decode_rejects_bare_array() {
        // Hard cutover: a legacy bare array (no envelope) is NOT accepted — it
        // has no `version`/`items`, so the envelope deserialization fails.
        let bare = serde_json::json!([ { "name": "api", "port": 8080 } ]);
        assert!(decode_side_data::<ContainerSpec>(&bare).is_err());
    }
}
