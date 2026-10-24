use super::{device, handlers};
use crate::server::state::AppState;
use axum::{
    routing::{get, post},
    Router,
};

/// Public routes that don't require authentication
pub fn public_routes() -> Router<AppState> {
    Router::new()
        .route("/auth/authorize", post(handlers::authorize))
        .route("/auth/code/exchange", post(handlers::code_exchange))
        .route("/auth/device/exchange", post(device::exchange))
        .route("/auth/signin", get(handlers::signin_page))
        .route("/auth/signin/start", get(handlers::oauth_signin_start))
        .route("/auth/callback", get(handlers::oauth_callback))
        .route("/auth/ingress", get(handlers::ingress_auth))
        .route("/auth/logout", get(handlers::oauth_logout))
        .route("/auth/cli-success", get(handlers::cli_auth_success))
        .route("/auth/jwks", get(handlers::jwks))
}

/// Root-level well-known routes (must be at root per OIDC spec)
///
/// These routes must be mounted at the root level (not under `/api/v1`) to comply
/// with OpenID Connect Discovery 1.0 specification which requires the discovery
/// endpoint to be at `/.well-known/openid-configuration` relative to the issuer URL.
pub fn well_known_routes() -> Router<AppState> {
    Router::new().route(
        "/.well-known/openid-configuration",
        get(handlers::openid_configuration),
    )
}

/// Routes for `/.rise/auth/*` path (for custom domain support via Ingress routing)
///
/// These routes are mounted at the root level (not under `/api/v1`) to allow
/// custom domains to route auth requests through their Ingress to the Rise backend.
/// This enables cookie-based authentication for custom domains where cookie sharing
/// with the Rise backend domain is not possible due to browser security restrictions.
///
/// Flow:
/// 1. User visits custom domain → signin page at /.rise/auth/signin
/// 2. Signin start redirects to IdP with callback URL on main Rise domain
/// 3. After IdP callback on main domain, redirect to /.rise/auth/complete with one-time token
/// 4. Complete handler sets cookie on custom domain and shows success page
pub fn rise_auth_routes() -> Router<AppState> {
    Router::new()
        .route("/.rise/auth/signin", get(handlers::signin_page))
        .route(
            "/.rise/auth/signin/start",
            get(handlers::oauth_signin_start),
        )
        .route("/.rise/auth/complete", get(handlers::oauth_complete))
}

/// Auth-only routes that require authentication but NOT platform access
/// These routes are accessible to all authenticated users regardless of platform access policy
pub fn auth_only_routes() -> Router<AppState> {
    Router::new().route("/users/me", get(handlers::me))
}

/// Protected routes that require authentication AND platform access
pub fn platform_routes() -> Router<AppState> {
    Router::new()
        .route("/users/lookup", post(handlers::users_lookup))
        .route("/auth/device", get(device::lookup))
        .route("/auth/device/approve", post(device::approve))
        .route("/auth/device/deny", post(device::deny))
}
