use crate::access::AccessRequirement;
use serde::{Deserialize, Serialize};

/// A runtime environment variable override included in a deployment request.
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
    /// Target environment name. When set, this override is only applied if the
    /// resolved deployment environment matches. `None` means the override is global.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub for_environment: Option<String>,
}

/// Per-container probe configuration. All fields are optional and fall back to
/// runtime defaults. `disabled = true` turns probes off entirely
/// (`health_check = false` in rise.toml).
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

/// One container in a multi-container deployment request or persisted side-data.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ContainerSpec {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Port the container listens on. Drives per-container service/routing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replicas: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,
    /// Container-scoped env vars.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_overrides: Vec<EnvOverride>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_check: Option<HealthCheckSpec>,
}

/// One ingress route mapping. A route maps a path to a target container; the
/// effective port is always the target container's `port`.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RouteSpec {
    pub path: String,
    pub container: String,
    /// Per-route ingress auth requirement override (see
    /// `project_config::RouteConfig::access`). Additive and `#[serde(default)]`,
    /// so existing `CONTAINER_SIDE_DATA_VERSION` side-data (which predates this
    /// field) decodes with `access = None` — no version bump needed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<AccessRequirement>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_spec_serializes_sparse_container_without_empty_fields() {
        let spec = ContainerSpec {
            name: "api".to_string(),
            image: None,
            port: Some(8080),
            replicas: None,
            cpu: None,
            memory: None,
            env_overrides: vec![],
            health_check: None,
        };

        assert_eq!(
            serde_json::to_value(&spec).unwrap(),
            json!({ "name": "api", "port": 8080 })
        );
    }

    #[test]
    fn request_spec_serializes_full_container_shape() {
        let spec = ContainerSpec {
            name: "api".to_string(),
            image: Some("repo/api:tag".to_string()),
            port: Some(8080),
            replicas: Some(2),
            cpu: Some("500m".to_string()),
            memory: Some("256Mi".to_string()),
            env_overrides: vec![EnvOverride {
                key: "LOG_LEVEL".to_string(),
                value: "debug".to_string(),
                is_secret: false,
                is_protected: None,
                source: Some("toml".to_string()),
                for_environment: None,
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
        };

        assert_eq!(
            serde_json::to_value(&spec).unwrap(),
            json!({
                "name": "api",
                "image": "repo/api:tag",
                "port": 8080,
                "replicas": 2,
                "cpu": "500m",
                "memory": "256Mi",
                "env_overrides": [ { "key": "LOG_LEVEL", "value": "debug", "is_secret": false, "source": "toml" } ],
                "health_check": {
                    "disabled": false,
                    "path": "/health",
                    "initial_delay_seconds": 5,
                    "period_seconds": 10,
                    "timeout_seconds": 2,
                    "failure_threshold": 3,
                    "liveness_enabled": true,
                    "readiness_enabled": false
                }
            })
        );
    }

    #[test]
    fn env_override_deserialization_defaults_match_wire_contract() {
        let env_override: EnvOverride = serde_json::from_value(json!({
            "key": "API_KEY",
            "value": "secret",
            "is_secret": true
        }))
        .unwrap();

        assert!(env_override.is_secret);
        assert_eq!(env_override.is_protected, None);
        assert_eq!(env_override.source, None);
        assert_eq!(env_override.for_environment, None);
    }
}
