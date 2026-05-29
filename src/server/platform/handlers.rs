use super::models::PlatformCapabilities;
use crate::server::state::AppState;
use axum::{extract::State, Json};

/// Describe the deployment platform's capabilities.
///
/// Served as a public (unauthenticated) endpoint: it must be callable by
/// project service accounts, but it has no project context to resolve a service
/// account against, and the response carries no sensitive information (just the
/// runtime architecture and whether pods may run as root). Keeping it public
/// avoids the awkward middle ground of accepting any JWKS-valid token without
/// the per-project claim validation that scoped endpoints perform.
pub async fn platform_capabilities(State(state): State<AppState>) -> Json<PlatformCapabilities> {
    // When no Kubernetes controller is configured (local dev / non-K8s
    // runtimes) there is no resource builder, so we advertise an unconstrained
    // platform.
    let runtime_arch = state
        .resource_builder
        .as_ref()
        .and_then(|rb| rb.node_selector.get("kubernetes.io/arch"))
        .cloned();
    let pod_security_enabled = state
        .resource_builder
        .as_ref()
        .map(|rb| rb.pod_security_enabled);

    Json(PlatformCapabilities::new(
        runtime_arch,
        pod_security_enabled,
    ))
}
