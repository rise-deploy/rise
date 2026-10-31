use super::relay::MOUNT_PREFIX;
use super::test_flow;
use crate::server::state::AppState;
use axum::{routing::get, Router};

/// The OAuth relay at the root: `/oidc/{project}/{extension}/…` serves `authorize`,
/// `callback`, `token`, `jwks` and `.well-known/openid-configuration` for every
/// active OAuth extension.
///
/// Mounted with `route_service`, which hands the relay the unstripped path: the
/// relay interprets the raw path below `/oidc/` itself.
pub fn relay_routes(state: &AppState) -> Router<AppState> {
    Router::new().route_service(
        &format!("{MOUNT_PREFIX}/{{*path}}"),
        state.oauth_relay.service(state.oauth_rate_limiter.clone()),
    )
}

/// The Rise UI's OAuth test flow, nested under `/api/v1` without the auth
/// middleware: both handlers authenticate the Rise UI session cookie themselves.
pub fn test_flow_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/projects/{project}/extensions/{extension}/oauth/test",
            get(test_flow::start),
        )
        .route(
            "/projects/{project}/extensions/{extension}/oauth/test/callback",
            get(test_flow::callback),
        )
}
