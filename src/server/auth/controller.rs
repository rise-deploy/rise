//! Controller identity authentication context.
//!
//! Defines the request-extension token type and the Axum extractor used by the
//! generic-resource controller endpoints. The controller auth context is
//! intentionally separate from `AuthContext` (which covers user JWTs and
//! project-scoped service-account JWTs) so the type system rules out mixing
//! controller tokens with user/SA flows.
//!
//! The configuration type ([`ControllerIdentity`]) and the pure matchers
//! ([`match_controller_identity`], [`build_controller_indexes`]) live in the
//! pure `rise-backend-auth` crate and are re-exported here so call sites can
//! refer to them via this module path.
//!
//! The extractor is consumed by the generic resource API
//! (`src/server/resources/handlers.rs`).
use axum::{extract::FromRequestParts, http::request::Parts};

// Re-export so `crate::server::auth::controller::ControllerIdentity` (and the
// matcher/index helpers) keep resolving for existing call sites.
pub use rise_backend_auth::{
    build_controller_indexes, match_controller_identity, ControllerIdentity, ControllerMatch,
};

use crate::server::auth::context::VerifiedExternalToken;
use crate::server::error::ServerError;
use crate::server::state::AppState;

/// A JWKS-validated controller token, produced by `ControllerAuthContext` after
/// `auth_middleware` verifies the external JWT and the matching
/// `ControllerIdentity`'s claim constraints are satisfied.
///
/// `issuer` / `claims` are retained for audit and future use; only `identity_id`
/// is read by current consumers, hence `#[allow(dead_code)]`.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct VerifiedControllerToken {
    pub identity_id: String,
    pub issuer: String,
    pub claims: serde_json::Value,
}

/// Axum extractor — yields the verified controller token or 401.
#[derive(Clone, Debug)]
pub struct ControllerAuthContext(pub VerifiedControllerToken);

impl FromRequestParts<AppState> for ControllerAuthContext {
    type Rejection = ServerError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> std::result::Result<Self, Self::Rejection> {
        let token = parts
            .extensions
            .get::<VerifiedExternalToken>()
            .cloned()
            .ok_or_else(|| ServerError::unauthorized("Controller authentication required"))?;

        let candidates = state
            .controllers_by_issuer
            .get(&token.issuer)
            .ok_or_else(|| ServerError::unauthorized("Controller authentication required"))?;

        match match_controller_identity(&token.claims, candidates) {
            ControllerMatch::Single(ident) => Ok(ControllerAuthContext(VerifiedControllerToken {
                identity_id: ident.id.clone(),
                issuer: token.issuer,
                claims: token.claims,
            })),
            ControllerMatch::Unmatched(detail) => {
                tracing::warn!(
                    "Controller JWT for issuer '{}' did not match any configured identity: {}",
                    token.issuer,
                    detail
                );
                Err(ServerError::unauthorized(
                    "Token did not match any configured controller identity",
                ))
            }
            ControllerMatch::Multiple(matched) => {
                let ids: Vec<&str> = matched.iter().map(|i| i.id.as_str()).collect();
                tracing::error!(
                    "Multiple controller identities matched JWT from issuer '{}': {:?}",
                    token.issuer,
                    ids
                );
                Err(ServerError::conflict(
                    "Token matched multiple controller identities; configuration is ambiguous",
                ))
            }
        }
    }
}
