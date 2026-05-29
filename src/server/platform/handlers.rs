use super::models::PlatformCapabilities;
use crate::server::auth::context::AuthContext;
use crate::server::state::AppState;
use axum::{extract::State, Json};

/// Describe the deployment platform's capabilities.
///
/// Authenticated but not project-scoped: it intentionally accepts any
/// `AuthContext` (interactive users *and* CI service accounts) so the CLI can
/// resolve its build architecture the same way it reaches
/// `registry-credentials`. We deliberately do **not** call `auth.user()`, which
/// would reject service-account tokens.
pub async fn platform_capabilities(
    State(state): State<AppState>,
    _auth: AuthContext,
) -> Json<PlatformCapabilities> {
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
