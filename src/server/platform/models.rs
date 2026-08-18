use serde::Serialize;

/// Normalize architecture names from runtime APIs to OCI platform names.
///
/// Docker normally reports Go's `GOARCH` values (already `amd64`, `arm64`,
/// etc.), but compatible/remote daemons may expose kernel-style aliases.  Keep
/// the capability stable so the CLI can safely prepend `linux/`.
pub(crate) fn normalize_runtime_arch(raw: &str) -> Option<String> {
    let arch = raw.trim().to_ascii_lowercase();
    let normalized = match arch.as_str() {
        "" => return None,
        "x86_64" | "x86-64" => "amd64",
        "aarch64" => "arm64",
        "i386" | "i486" | "i586" | "i686" | "x86" => "386",
        "armv7" | "armv7l" => "arm/v7",
        "armv6" | "armv6l" => "arm/v6",
        other => other,
    };
    Some(normalized.to_string())
}

/// Read-only description of the deployment platform's capabilities, surfaced to
/// clients via `GET /api/v1/platform/capabilities`.
///
/// The response body *is* the capability set — fields sit flat at the top
/// level. The intent is to name the real runtime capabilities (architecture,
/// whether pods may run as root) and let clients derive *consequences*
/// themselves (e.g. "binding a privileged port `< 1024` will fail without
/// `CAP_NET_BIND_SERVICE` or root"), rather than baking those consequences into
/// per-resource config. New capabilities are added as additional fields.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PlatformCapabilities {
    /// Container architecture the target runtime accepts (e.g. `amd64`,
    /// `arm64`). Kubernetes takes it from the controller's
    /// `node_selector["kubernetes.io/arch"]`; Docker reads it from the
    /// connected daemon. Absent when the runtime is unconstrained — clients
    /// then fall back to the host architecture.
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
    fn normalizes_runtime_architectures_to_oci_names() {
        assert_eq!(normalize_runtime_arch("amd64").as_deref(), Some("amd64"));
        assert_eq!(normalize_runtime_arch("x86_64").as_deref(), Some("amd64"));
        assert_eq!(normalize_runtime_arch("aarch64").as_deref(), Some("arm64"));
        assert_eq!(normalize_runtime_arch("ARM64").as_deref(), Some("arm64"));
        assert_eq!(normalize_runtime_arch("armv7l").as_deref(), Some("arm/v7"));
        assert_eq!(normalize_runtime_arch("s390x").as_deref(), Some("s390x"));
        assert_eq!(normalize_runtime_arch("  "), None);
    }

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
