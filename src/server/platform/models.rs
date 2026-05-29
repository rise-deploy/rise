use serde::Serialize;

/// Read-only description of the deployment platform's capabilities, surfaced to
/// clients via `GET /api/v1/platform/capabilities`.
///
/// The response body *is* the capability set — fields sit flat at the top
/// level. The intent is to name the real cluster capabilities (architecture,
/// whether pods may run as root) and let clients derive *consequences*
/// themselves (e.g. "binding a privileged port `< 1024` will fail without
/// `CAP_NET_BIND_SERVICE` or root"), rather than baking those consequences into
/// per-resource config. New capabilities are added as additional fields.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PlatformCapabilities {
    /// Container architecture the target cluster accepts (e.g. `amd64`,
    /// `arm64`), taken from the controller's
    /// `node_selector["kubernetes.io/arch"]`. Absent when the cluster is
    /// unconstrained — clients then fall back to the host architecture.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_arch: Option<String>,
    /// Whether deployed pods are allowed to run as the root user. Derived from
    /// the controller's `pod_security_enabled` setting: `false` when the
    /// platform enforces a restricted security context (which also means a
    /// privileged port `< 1024` can't be bound).
    pub runtime_allows_root: bool,
}

impl PlatformCapabilities {
    /// Build the capability set from the platform's raw properties.
    ///
    /// `pod_security_enabled` is `None` when there is no Kubernetes controller
    /// configured (e.g. local dev / non-K8s runtimes), in which case nothing
    /// restricts running as root, so `runtime_allows_root` defaults to `true`.
    pub fn new(runtime_arch: Option<String>, pod_security_enabled: Option<bool>) -> Self {
        let runtime_allows_root = pod_security_enabled.map(|enabled| !enabled).unwrap_or(true);
        Self {
            runtime_arch,
            runtime_allows_root,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforced_pod_security_disallows_root() {
        let caps = PlatformCapabilities::new(Some("amd64".to_string()), Some(true));
        assert_eq!(caps.runtime_arch.as_deref(), Some("amd64"));
        assert!(!caps.runtime_allows_root);
    }

    #[test]
    fn disabled_pod_security_allows_root() {
        let caps = PlatformCapabilities::new(None, Some(false));
        assert!(caps.runtime_arch.is_none());
        assert!(caps.runtime_allows_root);
    }

    #[test]
    fn no_controller_defaults_to_allowing_root() {
        let caps = PlatformCapabilities::new(None, None);
        assert!(caps.runtime_allows_root);
    }

    #[test]
    fn serializes_flat() {
        let caps = PlatformCapabilities::new(None, Some(true));
        let json = serde_json::to_value(&caps).unwrap();
        // `runtime_arch` is omitted when absent; the capability sits flat at the
        // top level (no redundant `capabilities` wrap).
        assert!(json.get("runtime_arch").is_none());
        assert!(json.get("capabilities").is_none());
        assert_eq!(json["runtime_allows_root"], serde_json::json!(false));
    }
}
