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
    let capabilities = state.deployment_backend.capabilities();

    Json(PlatformCapabilities::new(
        // `runtime_arch` is resolved at startup: some backends detect it from
        // the runtime, which the backend itself cannot do synchronously here.
        state.runtime_arch.clone(),
        capabilities.pod_security_enabled,
    ))
}
