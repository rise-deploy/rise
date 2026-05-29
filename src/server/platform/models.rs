use serde::Serialize;
use std::collections::BTreeMap;

/// Capability key: whether deployed pods are allowed to run as the root user.
/// Derived from the controller's `pod_security_enabled` setting. When the
/// platform enforces a restricted security context this is `false`, and the
/// frontend can derive consequences from it (e.g. binding a privileged port
/// `< 1024` will fail without `CAP_NET_BIND_SERVICE` or root).
pub const CAP_DEPLOYMENTS_CAN_RUN_AS_ROOT: &str = "deployments:canRunAsRoot";

/// Read-only description of the deployment platform's properties, surfaced to
/// clients via `GET /api/v1/platform/capabilities`.
///
/// The intent is to name the real cluster *capabilities* (architecture, whether
/// pods may run as root) and let clients derive *consequences* themselves,
/// rather than baking those consequences into per-resource config.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PlatformCapabilities {
    /// Container architecture the target cluster accepts (e.g. `amd64`,
    /// `arm64`), taken from the controller's
    /// `node_selector["kubernetes.io/arch"]`. Absent when the cluster is
    /// unconstrained — clients then fall back to the host architecture.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_arch: Option<String>,
    /// Namespaced boolean capability flags (e.g. `deployments:canRunAsRoot`).
    /// Kept as an open map so new capabilities can be added without a wire
    /// break.
    pub capabilities: BTreeMap<String, bool>,
}

impl PlatformCapabilities {
    /// Build the capability set from the platform's raw properties.
    ///
    /// `pod_security_enabled` is `None` when there is no Kubernetes controller
    /// configured (e.g. local dev / non-K8s runtimes), in which case nothing
    /// restricts running as root, so `deployments:canRunAsRoot` defaults to
    /// `true`.
    pub fn new(runtime_arch: Option<String>, pod_security_enabled: Option<bool>) -> Self {
        let can_run_as_root = pod_security_enabled.map(|enabled| !enabled).unwrap_or(true);
        let mut capabilities = BTreeMap::new();
        capabilities.insert(CAP_DEPLOYMENTS_CAN_RUN_AS_ROOT.to_string(), can_run_as_root);
        Self {
            runtime_arch,
            capabilities,
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
        assert_eq!(
            caps.capabilities.get(CAP_DEPLOYMENTS_CAN_RUN_AS_ROOT),
            Some(&false)
        );
    }

    #[test]
    fn disabled_pod_security_allows_root() {
        let caps = PlatformCapabilities::new(None, Some(false));
        assert!(caps.runtime_arch.is_none());
        assert_eq!(
            caps.capabilities.get(CAP_DEPLOYMENTS_CAN_RUN_AS_ROOT),
            Some(&true)
        );
    }

    #[test]
    fn no_controller_defaults_to_allowing_root() {
        let caps = PlatformCapabilities::new(None, None);
        assert_eq!(
            caps.capabilities.get(CAP_DEPLOYMENTS_CAN_RUN_AS_ROOT),
            Some(&true)
        );
    }

    #[test]
    fn runtime_arch_omitted_from_json_when_absent() {
        let caps = PlatformCapabilities::new(None, Some(true));
        let json = serde_json::to_value(&caps).unwrap();
        assert!(json.get("runtime_arch").is_none());
        assert_eq!(
            json["capabilities"][CAP_DEPLOYMENTS_CAN_RUN_AS_ROOT],
            serde_json::json!(false)
        );
    }
}
