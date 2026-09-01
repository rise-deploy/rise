//! Configuration types read by the Kubernetes deployment backend.
//!
//! `rise-deploy` re-exports these from `server::settings`, so the shape of the
//! generated backend-config schema is unchanged by their living here.

use serde::{Deserialize, Serialize};

#[cfg(feature = "schema")]
use schemars::JsonSchema;

fn default_kubernetes_max_tail_lines() -> i64 {
    100_000
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct KubernetesLogBackendSettings {
    /// Upper bound on the number of lines the backend will ever request from
    /// the kubelet in a single call. The frontend pages backward by widening
    /// `tail_lines` through an opaque continuation; paging stops at this
    /// ceiling or when the kubelet's own ring buffer is exhausted. Default:
    /// 100000.
    #[serde(default = "default_kubernetes_max_tail_lines")]
    pub max_tail_lines: i64,
}

impl Default for KubernetesLogBackendSettings {
    fn default() -> Self {
        Self {
            max_tail_lines: default_kubernetes_max_tail_lines(),
        }
    }
}

/// Backend address for routing /.rise/* traffic to the Rise backend
#[derive(Debug, Clone)]
pub struct BackendAddress {
    pub host: String,
    pub port: u16,
}

impl BackendAddress {
    /// Parse backend address from a URL by extracting host and port
    /// Example: "http://172.17.0.1:3000" -> BackendAddress { host: "172.17.0.1", port: 3000 }
    pub fn from_url(url: &str) -> Result<Self, anyhow::Error> {
        let parsed = url::Url::parse(url)
            .map_err(|e| anyhow::anyhow!("Invalid URL for backend address: {}", e))?;

        let host = parsed
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("URL missing host"))?
            .to_string();

        let port = parsed
            .port()
            .or_else(|| {
                // Default ports based on scheme
                match parsed.scheme() {
                    "http" => Some(80),
                    "https" => Some(443),
                    _ => None,
                }
            })
            .ok_or_else(|| anyhow::anyhow!("URL missing port and no default for scheme"))?;

        Ok(Self { host, port })
    }

    /// Check if the host is an IP address (vs a DNS name)
    pub fn is_ip_address(&self) -> bool {
        self.host.parse::<std::net::IpAddr>().is_ok()
    }
}

/// TLS mode for custom domains
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "kebab-case")]
pub enum CustomDomainTlsMode {
    /// All hosts (primary + custom domains) share the same TLS secret
    Shared,
    /// Each custom domain gets its own tls-{domain} secret (cert-manager integration)
    PerDomain,
}

/// Health probe configuration
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct HealthProbeConfig {
    /// Enable liveness probes (default: true)
    #[serde(default = "default_true")]
    pub liveness_enabled: bool,

    /// Enable readiness probes (default: true)
    #[serde(default = "default_true")]
    pub readiness_enabled: bool,

    /// Path for HTTP probes (default: "/")
    #[serde(default = "default_probe_path")]
    pub path: String,

    /// Initial delay in seconds (default: 10)
    #[serde(default = "default_initial_delay")]
    pub initial_delay_seconds: i32,

    /// Period in seconds (default: 10)
    #[serde(default = "default_period_seconds")]
    pub period_seconds: i32,

    /// Timeout in seconds (default: 5)
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: i32,

    /// Failure threshold (default: 3)
    #[serde(default = "default_failure_threshold")]
    pub failure_threshold: i32,
}

/// NetworkPolicy configuration for deployed apps
///
/// Uses Kubernetes NetworkPolicy types directly. Egress semantics:
/// - null: policyTypes is ["Ingress"] only, Kubernetes does not restrict egress
/// - Empty list: policyTypes includes "Egress" with no rules = deny all egress
/// - Non-empty list: explicit egress rules enforced
#[derive(Debug, Clone, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct NetworkPolicyConfig {
    /// Ingress rules
    pub ingress: Vec<k8s_openapi::api::networking::v1::NetworkPolicyIngressRule>,
    /// Egress rules (null = unrestricted egress)
    pub egress: Option<Vec<k8s_openapi::api::networking::v1::NetworkPolicyEgressRule>>,
}

fn default_true() -> bool {
    true
}

fn default_probe_path() -> String {
    "/".to_string()
}

fn default_initial_delay() -> i32 {
    10
}

fn default_period_seconds() -> i32 {
    10
}

fn default_timeout_seconds() -> i32 {
    5
}

fn default_failure_threshold() -> i32 {
    3
}
