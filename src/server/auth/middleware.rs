use axum::{
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::Response,
};
use base64::{engine::general_purpose, Engine as _};
use jsonwebtoken::decode_header;
use serde::Deserialize;

use crate::db::{service_accounts, users, User};
use crate::server::auth::context::VerifiedExternalToken;
use crate::server::auth::cookie_helpers;
use crate::server::state::AppState;
use rise_backend_auth::{is_rise_issued_jwt, AccessClaims, PrincipalClaims, RiseToken};

/// Extract Bearer token from Authorization header
pub(crate) fn extract_bearer_token(headers: &HeaderMap) -> Option<String> {
    let auth_header = headers.get("Authorization")?.to_str().ok()?;

    if !auth_header.starts_with("Bearer ") {
        return None;
    }

    Some(auth_header[7..].to_string())
}

/// Extract Rise JWT from cookie
fn extract_rise_jwt_from_cookie(headers: &HeaderMap) -> Option<String> {
    cookie_helpers::extract_rise_jwt_cookie(headers)
}

/// Minimal JWT claims structure just to peek at the issuer (unvalidated), to
/// route between the Rise-issued and external verification paths.
#[derive(Debug, Deserialize)]
struct MinimalClaims {
    iss: String,
}

/// Authentication middleware that validates JWT and injects User or VerifiedExternalToken
/// into request extensions.
///
/// For Rise-issued JWTs: validates with `RiseTokenSigner` and injects `User`.
/// For external JWTs: validates signature + expiry via JWKS (phase 1) and injects
/// `VerifiedExternalToken`. Claim validation against project-scoped service accounts
/// happens in phase 2 (inside handlers via `AuthContext::resolve_for_project`).
pub async fn auth_middleware(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut req: Request,
    next: Next,
) -> Result<Response, (StatusCode, String)> {
    tracing::debug!(
        "Auth middleware: validating request to {}",
        req.uri().path()
    );

    // Try to extract token from cookie first (new primary method)
    let token = if let Some(cookie_token) = extract_rise_jwt_from_cookie(&headers) {
        tracing::debug!(
            "Auth middleware: found Rise JWT in cookie (length={})",
            cookie_token.len()
        );
        cookie_token
    } else if let Some(bearer_token) = extract_bearer_token(&headers) {
        tracing::debug!(
            "Auth middleware: found Bearer token in Authorization header (length={})",
            bearer_token.len()
        );
        bearer_token
    } else {
        tracing::warn!("Auth middleware: no authentication token found");
        return Err((
            StatusCode::UNAUTHORIZED,
            "Missing authentication token (cookie or Authorization header)".to_string(),
        ));
    };

    // Peek at the issuer to determine authentication method
    let issuer = {
        // Decode header to check if JWT is well-formed
        decode_header(&token).map_err(|e| {
            tracing::warn!("Failed to decode JWT header: {:#}", e);
            (
                StatusCode::UNAUTHORIZED,
                format!("Invalid token format: {}", e),
            )
        })?;

        // Decode payload without validation to peek at issuer
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() != 3 {
            return Err((StatusCode::UNAUTHORIZED, "Invalid JWT format".to_string()));
        }

        let payload = parts[1];
        let decoded = general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|e| {
                tracing::warn!("Failed to decode JWT payload: {:#}", e);
                (
                    StatusCode::UNAUTHORIZED,
                    "Invalid token encoding".to_string(),
                )
            })?;

        let claims: MinimalClaims = serde_json::from_slice(&decoded).map_err(|e| {
            tracing::warn!("Failed to parse JWT claims: {:#}", e);
            (StatusCode::UNAUTHORIZED, "Invalid token claims".to_string())
        })?;

        claims.iss
    };

    tracing::debug!(
        "Auth middleware: token issuer='{}', configured issuer='{}', rise public_url='{}'",
        issuer,
        state.auth_settings.issuer,
        state.public_url
    );

    if is_rise_issued_jwt(&issuer, &state.public_url) {
        // Rise-issued JWT — classify centrally (HS256 session / HS256 access /
        // RS256 ingress) and accept only the kinds valid on the API path.
        tracing::debug!("Auth middleware: authenticating with Rise-issued JWT");

        let rise_token = state.jwt_signer.verify_rise_jwt(&token).map_err(|e| {
            tracing::warn!("Auth middleware: Rise JWT validation failed: {:#}", e);
            (StatusCode::UNAUTHORIZED, format!("Invalid token: {}", e))
        })?;

        match rise_token {
            RiseToken::Session(claims) => {
                // User session token — `aud` must be the Rise public URL (the
                // verifier skips `aud`, so enforce it here as the API path does).
                if claims.aud != state.public_url {
                    tracing::warn!("Auth middleware: session token audience mismatch");
                    return Err((StatusCode::UNAUTHORIZED, "Invalid token".to_string()));
                }

                let email = &claims.email;
                tracing::debug!("Rise session JWT validated for user");

                // Extract groups from Rise JWT for platform access checks
                let groups = claims.groups.clone();
                req.extensions_mut().insert(groups);

                // CRITICAL: user creation and default-Organization membership must
                // happen in a single transaction so bootstrap's validation pass
                // never sees a user row without a membership.
                let user = users::find_or_create_with_default_organization(
                    &state.db_pool,
                    email,
                    state.default_organization_uid,
                )
                .await
                .map_err(|e| {
                    tracing::error!("Failed to find/create user: {:#}", e);
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Database error".to_string(),
                    )
                })?;

                tracing::debug!("User authenticated: {} ({})", user.email, user.id);
                req.extensions_mut().insert(user);
            }
            RiseToken::Access(claims) => {
                // Exchanged access token — `aud` must be the Rise public URL.
                if claims.aud != state.public_url {
                    tracing::warn!("Auth middleware: access token audience mismatch");
                    return Err((StatusCode::UNAUTHORIZED, "Invalid token".to_string()));
                }
                tracing::debug!(
                    "Auth middleware: Rise access token accepted (principal={:?})",
                    claims.principal
                );
                req.extensions_mut().insert(claims);
            }
            RiseToken::Ingress(_) => {
                // RS256 ingress tokens are for deployed-app ingress auth only.
                tracing::warn!("Auth middleware: ingress token rejected on the API path");
                return Err((StatusCode::UNAUTHORIZED, "Invalid token".to_string()));
            }
        }
    } else if !state.auth_settings.allow_raw_external_tokens {
        // Raw external tokens are disabled — callers must pre-exchange at
        // `POST /api/v1/auth/token` for a Rise access token.
        tracing::warn!(
            "Auth middleware: raw external token from issuer '{}' rejected (allow_raw_external_tokens=false)",
            issuer
        );
        return Err((
            StatusCode::UNAUTHORIZED,
            "Raw external tokens are not accepted; exchange your token at /api/v1/auth/token"
                .to_string(),
        ));
    } else {
        // External issuer — legacy path: JWKS validation only, resolved per
        // project later. The deprecation signal is emitted *after* validation
        // succeeds (see below) so the metric counts only accepted raw tokens
        // and is keyed on validated claims.
        tracing::debug!(
            "Auth middleware: external JWT from issuer '{}', performing JWKS validation",
            issuer
        );

        // Lightweight guard: skip JWKS fetch if neither a controller nor any SA
        // uses this issuer. Controller issuers are in static config (O(1));
        // SA issuers require a DB round-trip and are only checked when the
        // issuer isn't already a known controller issuer.
        if !state.controllers_by_issuer.contains_key(&issuer) {
            let sa_issuer_exists = service_accounts::issuer_exists(&state.db_pool, &issuer)
                .await
                .map_err(|e| {
                    tracing::error!("Failed to check issuer existence: {:#}", e);
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Database error".to_string(),
                    )
                })?;
            if !sa_issuer_exists {
                tracing::warn!(
                    "No service accounts or controller identities configured for issuer: {}",
                    issuer
                );
                return Err((
                    StatusCode::UNAUTHORIZED,
                    "No service accounts or controller identities configured for this issuer"
                        .to_string(),
                ));
            }
        }

        // Validate signature + expiry via JWKS (no custom claim validation)
        let claims = rise_backend_auth::verify_external_jwt(&token, &issuer, &*state.jwt_validator)
            .await
            .map_err(|e| {
                tracing::warn!(
                    "Auth middleware: external JWT JWKS validation failed for issuer '{}': {:#}",
                    issuer,
                    e
                );
                (StatusCode::UNAUTHORIZED, format!("Invalid token: {}", e))
            })?
            .claims()
            .clone();

        tracing::debug!(
            "Auth middleware: external JWT JWKS-validated for issuer '{}'",
            issuer
        );

        // Now that the raw token is accepted, emit one metric-shaped
        // deprecation event per request so operators can aggregate it in their
        // log pipeline (count, group by issuer/sub) to see which workload
        // identities still need to migrate to the token-exchange flow. Read
        // from the *validated* claims; only the `sub`/`aud` identifier claims
        // are logged, not the full payload.
        let sub = claims
            .get("sub")
            .and_then(|v| v.as_str())
            .unwrap_or("<none>");
        // `aud` may be a string or an array; render a bare string for the
        // common single-audience case and compact JSON otherwise, so the field
        // stays parseable.
        let aud = claims
            .get("aud")
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| v.to_string())
            })
            .unwrap_or_else(|| "<none>".to_string());
        tracing::warn!(
            target: "rise::deprecation",
            metric = "raw_external_token",
            issuer = %issuer,
            sub = %sub,
            aud = %aud,
            "deprecated: accepting raw external token (not pre-exchanged); migrate to \
             POST /api/v1/auth/token. auth.allow_raw_external_tokens defaults to false \
             starting in 0.25.0"
        );

        // Store the verified token for route-specific controller extraction or
        // project-scoped service-account claim validation.
        req.extensions_mut().insert(VerifiedExternalToken {
            issuer: issuer.clone(),
            claims,
        });
    }

    Ok(next.run(req).await)
}

/// Optional authentication middleware - allows unauthenticated requests but injects User if token is present
#[allow(dead_code)]
pub async fn optional_auth_middleware(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut req: Request,
    next: Next,
) -> Response {
    // Try to extract token from cookie first, then Authorization header
    let token = extract_rise_jwt_from_cookie(&headers).or_else(|| extract_bearer_token(&headers));

    if let Some(token) = token {
        // Peek at issuer
        if decode_header(&token).is_ok() {
            let parts: Vec<&str> = token.split('.').collect();
            if parts.len() == 3 {
                if let Ok(decoded) = general_purpose::URL_SAFE_NO_PAD.decode(parts[1]) {
                    if let Ok(claims) = serde_json::from_slice::<MinimalClaims>(&decoded) {
                        // Only accept Rise-issued JWTs for optional auth
                        if is_rise_issued_jwt(&claims.iss, &state.public_url) {
                            // Try to validate Rise user JWT (HS256 + correct audience)
                            if let Ok(rise_claims) =
                                state.jwt_signer.verify_user_jwt(&token, &state.public_url)
                            {
                                let email = &rise_claims.email;
                                // Mirror the strict auth path: never create a user
                                // row without the matching default-Org membership.
                                if let Ok(user) = users::find_or_create_with_default_organization(
                                    &state.db_pool,
                                    email,
                                    state.default_organization_uid,
                                )
                                .await
                                {
                                    req.extensions_mut().insert(user);
                                }
                            }
                        }
                        // Note: Service account tokens are not supported in optional auth
                    }
                }
            }
        }
    }

    // Continue regardless of authentication status
    next.run(req).await
}

/// Platform access middleware - blocks non-platform users from API endpoints
///
/// Applied AFTER auth_middleware (which validates JWT and injects User or VerifiedExternalToken).
/// Only applies to protected API routes - does NOT apply to ingress auth endpoint.
pub async fn platform_access_middleware(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, (StatusCode, String)> {
    use crate::server::auth::platform_access::{ConfigBasedAccessChecker, PlatformAccessChecker};

    // Service accounts (legacy external tokens) bypass platform access checks.
    // Their access is validated per-project in phase 2.
    if req.extensions().get::<VerifiedExternalToken>().is_some() {
        tracing::debug!("Skipping platform access check for external token (service account)");
        return Ok(next.run(req).await);
    }

    // Exchanged access tokens: service-account and controller principals carry
    // their own authorization (project binding / controller identity), so they
    // bypass the email/group allowlist exactly as a legacy external token does.
    // A (reserved) user access token is gated like any other user.
    if let Some(claims) = req.extensions().get::<AccessClaims>() {
        match &claims.principal {
            PrincipalClaims::ServiceAccount { .. } | PrincipalClaims::Controller { .. } => {
                tracing::debug!("Skipping platform access check for access token (SA/controller)");
                return Ok(next.run(req).await);
            }
            PrincipalClaims::User { .. } => {
                // Reserved — not minted in Phase 1. Fall through to user gating
                // would require a `User` extension, which an access token does
                // not provide, so reject explicitly rather than 500.
                tracing::warn!("User access token presented; not supported in this phase");
                return Err((
                    StatusCode::UNAUTHORIZED,
                    "User access tokens are not supported".to_string(),
                ));
            }
        }
    }

    // Extract user from extensions (injected by auth_middleware for Rise JWTs)
    let user = req.extensions().get::<User>().ok_or_else(|| {
        tracing::error!("platform_access_middleware called without user in extensions");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Authentication error".to_string(),
        )
    })?;

    // Extract groups from request extensions (stored by auth_middleware)
    let groups = req.extensions().get::<Option<Vec<String>>>();

    // Check platform access dynamically
    let checker = ConfigBasedAccessChecker {
        config: &state.auth_settings.platform_access,
        admin_users: &state.admin_users,
        operator_users: &state.operator_users,
    };

    // Pass groups from Rise JWT to platform access checker
    if !checker.has_platform_access(
        user,
        groups.as_ref().and_then(|g| g.as_ref().map(|v| v.as_ref())),
    ) {
        tracing::warn!(
            user_id = %user.id,
            user_email = %user.email,
            path = %req.uri().path(),
            "Platform access denied for non-platform user"
        );
        return Err((
            StatusCode::FORBIDDEN,
            "You do not have access to Rise platform features. \
             Your account is configured for application access only. \
             Please contact your administrator if you need platform access."
                .to_string(),
        ));
    }

    Ok(next.run(req).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn test_extract_bearer_token_valid() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "Authorization",
            HeaderValue::from_static("Bearer my-token-here"),
        );

        let token = extract_bearer_token(&headers);
        assert_eq!(token, Some("my-token-here".to_string()));
    }

    #[test]
    fn test_extract_bearer_token_missing_header() {
        let headers = HeaderMap::new();
        let result = extract_bearer_token(&headers);
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_bearer_token_invalid_format() {
        let mut headers = HeaderMap::new();
        headers.insert("Authorization", HeaderValue::from_static("Basic user:pass"));

        let result = extract_bearer_token(&headers);
        assert_eq!(result, None);
    }
}
