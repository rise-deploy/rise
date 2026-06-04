//! RFC 8693 token-exchange handler.
//!
//! Exchanges an external OIDC subject token (+ optional Rise project) for a
//! short-lived, Rise-signed **access token** that fully encodes the resolved
//! principal. This is the single place an external CI identity (`iss` + `sub`)
//! maps to a Rise principal, so every successful mint is audit-logged.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as BASE64URL, Engine as _};

use crate::db::{projects, service_accounts};
use crate::server::auth::sa_match::{match_service_account, SaMatchError};
use crate::server::rate_limit::extract_client_ip;
use crate::server::state::AppState;
use rise_backend_auth::{
    is_rise_issued_jwt, match_controller_identity, verify_external_jwt, AuthError, ControllerMatch,
    PrincipalClaims, Scope,
};

use super::models::{
    ExchangeError, ExchangeRequest, ExchangeResponse, GRANT_TYPE_TOKEN_EXCHANGE,
    MAX_SUBJECT_TOKEN_LEN, TOKEN_TYPE_JWT,
};

/// The fixed scope set granted to a service-account access token (matching what
/// a service account can do today — per-SA configurable scopes are deferred).
const SA_SCOPES: [Scope; 3] = [Scope::Deploy, Scope::RegistryPush, Scope::ReadProject];

/// Rate-limit bucket used before a token's issuer is known / guard-validated.
const PRE_VALIDATION_BUCKET: &str = "auth-token-exchange-invalid";

/// Peek the (unvalidated) `iss` claim of a JWT without verifying its signature.
fn peek_issuer(token: &str) -> Option<String> {
    let payload_b64 = token.split('.').nth(1)?;
    let payload = BASE64URL.decode(payload_b64).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    value
        .get("iss")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// `POST /api/v1/auth/token` — exchange an external OIDC token for a Rise access token.
pub async fn exchange(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ExchangeRequest>,
) -> Response {
    match exchange_inner(&state, &headers, req).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn exchange_inner(
    state: &AppState,
    headers: &HeaderMap,
    req: ExchangeRequest,
) -> Result<ExchangeResponse, ExchangeError> {
    let ip = extract_client_ip(headers);

    // 0. Input validation (cheap, pre-rate-limit).
    if req.grant_type != GRANT_TYPE_TOKEN_EXCHANGE {
        return Err(ExchangeError::invalid_request("unsupported grant_type"));
    }
    if req.subject_token_type != TOKEN_TYPE_JWT {
        return Err(ExchangeError::invalid_request(
            "unsupported subject_token_type",
        ));
    }
    let subject_token = req.subject_token.trim();
    if subject_token.is_empty() {
        return Err(ExchangeError::invalid_request(
            "subject_token must not be empty",
        ));
    }
    if subject_token.len() > MAX_SUBJECT_TOKEN_LEN {
        return Err(ExchangeError::invalid_request("subject_token too large"));
    }

    // Coarse pre-validation rate limit (keyed by IP / global) — blunts
    // unauthenticated fan-out before any JWKS work.
    state
        .oauth_rate_limiter
        .increment_and_check(&ip, None, PRE_VALIDATION_BUCKET)
        .await
        .map_err(ExchangeError::rate_limited)?;

    // 1. Peek issuer and reject Rise-issued tokens (they cannot be exchanged).
    let Some(issuer) = peek_issuer(subject_token) else {
        return Err(ExchangeError::invalid_request(
            "subject_token is not a well-formed JWT",
        ));
    };
    if is_rise_issued_jwt(&issuer, &state.public_url) {
        return Err(ExchangeError::invalid_grant(
            "subject_token must be an external token",
        ));
    }

    // 2. Issuer guard: only configured controllers or known SA issuers. Avoid
    //    leaking unknown-issuer vs no-match beyond the coarse invalid_grant.
    let issuer_known = state.controllers_by_issuer.contains_key(&issuer) || {
        match service_accounts::issuer_exists(&state.db_pool, &issuer).await {
            Ok(exists) => exists,
            Err(e) => {
                tracing::error!("Token exchange: failed to check issuer existence: {:?}", e);
                return Err(ExchangeError::temporarily_unavailable(
                    "issuer lookup failed",
                ));
            }
        }
    };
    if !issuer_known {
        return Err(ExchangeError::invalid_grant(
            "subject_token could not be validated",
        ));
    }

    // Per-issuer rate limit (the issuer is now a server-recognized value, never
    // a raw attacker-chosen one). Keyed before the JWKS fetch.
    state
        .oauth_rate_limiter
        .increment_and_check(&ip, None, &issuer)
        .await
        .map_err(ExchangeError::rate_limited)?;

    // 3. JWKS signature + expiry validation (typed errors → distinct OAuth codes).
    let verified = match verify_external_jwt(subject_token, &issuer, &*state.jwt_validator).await {
        Ok(claims) => claims,
        Err(AuthError::Jwks { issuer, detail }) => {
            tracing::warn!(
                "Token exchange: JWKS unavailable for '{}': {}",
                issuer,
                detail
            );
            return Err(ExchangeError::temporarily_unavailable(
                "could not fetch issuer keys",
            ));
        }
        Err(e) => {
            tracing::warn!("Token exchange: subject_token verification failed: {:?}", e);
            return Err(ExchangeError::invalid_grant(
                "subject_token could not be validated",
            ));
        }
    };
    let claims = verified.claims();

    // 4 / 5. Resolve the principal.
    let (sub, principal, audit) = if let Some(project_name) = req.rise_project.as_deref() {
        resolve_service_account(state, project_name, &issuer, claims).await?
    } else {
        resolve_controller(state, &issuer, claims)?
    };

    // 6. Mint the access token, TTL-clamped.
    let ttl = state.server_settings.auth_token_max_ttl_seconds;
    let (token, minted) = state
        .jwt_signer
        .sign_access_jwt(&sub, principal, &state.public_url, ttl)
        .map_err(|e| {
            tracing::error!("Token exchange: failed to sign access token: {:?}", e);
            ExchangeError::temporarily_unavailable("failed to mint access token")
        })?;

    // 7. Audit log — links the external CI identity to the Rise principal it
    //    authorized (the only place this mapping is recorded).
    let source_sub = claims.get("sub").and_then(|v| v.as_str()).unwrap_or("");
    match &audit {
        AuditSubject::ServiceAccount { sa_id, project } => tracing::info!(
            service_account_id = %sa_id,
            project = %project,
            source_iss = %issuer,
            source_sub = %source_sub,
            jti = %minted.jti,
            "Token exchange: minted service-account access token"
        ),
        AuditSubject::Controller { identity_id } => tracing::info!(
            controller_identity_id = %identity_id,
            source_iss = %issuer,
            source_sub = %source_sub,
            jti = %minted.jti,
            "Token exchange: minted controller access token"
        ),
    }

    Ok(ExchangeResponse {
        access_token: token,
        token_type: "Bearer".to_string(),
        issued_token_type: TOKEN_TYPE_JWT.to_string(),
        expires_in: ttl,
    })
}

/// What the audit log records about a successful mint.
enum AuditSubject {
    ServiceAccount { sa_id: uuid::Uuid, project: String },
    Controller { identity_id: String },
}

/// Resolve a project service-account exchange (the `rise_project` is present).
async fn resolve_service_account(
    state: &AppState,
    project_name: &str,
    issuer: &str,
    claims: &serde_json::Value,
) -> Result<(String, PrincipalClaims, AuditSubject), ExchangeError> {
    let project = match projects::find_by_name(&state.db_pool, project_name).await {
        Ok(Some(p)) => p,
        Ok(None) => return Err(ExchangeError::invalid_target("unknown rise_project")),
        Err(e) => {
            tracing::error!("Token exchange: failed to look up project: {:?}", e);
            return Err(ExchangeError::temporarily_unavailable(
                "project lookup failed",
            ));
        }
    };

    let service_accounts = match service_accounts::find_by_project_and_issuer(
        &state.db_pool,
        project.id,
        issuer,
    )
    .await
    {
        Ok(sas) => sas,
        Err(e) => {
            tracing::error!(
                "Token exchange: failed to look up service accounts: {:?}",
                e
            );
            return Err(ExchangeError::temporarily_unavailable(
                "service account lookup failed",
            ));
        }
    };

    let sa =
        match match_service_account(
            claims,
            issuer,
            &service_accounts,
            &state.controllers_by_issuer,
        ) {
            Ok(sa) => sa,
            Err(SaMatchError::ControllerToken) => {
                return Err(ExchangeError::invalid_grant(
                    "controller tokens cannot be exchanged as service accounts",
                ))
            }
            Err(SaMatchError::AmbiguousController) => return Err(ExchangeError::invalid_grant(
                "subject_token matched multiple controller identities (ambiguous configuration)",
            )),
            Err(SaMatchError::Ambiguous(_)) => {
                return Err(ExchangeError::invalid_grant(
                    "subject_token matched multiple service accounts (ambiguous configuration)",
                ))
            }
            Err(SaMatchError::MalformedClaims(sa_id)) => {
                tracing::error!(
                    "Token exchange: malformed claims on service account {}",
                    sa_id
                );
                return Err(ExchangeError::temporarily_unavailable(
                    "service account claims configuration is invalid",
                ));
            }
            Err(SaMatchError::NoMatch { .. }) => {
                return Err(ExchangeError::invalid_grant(
                    "subject_token could not be validated",
                ))
            }
        };

    let principal = PrincipalClaims::ServiceAccount {
        service_account_id: sa.id,
        synthetic_user_id: sa.user_id,
        project_id: project.id,
        project_name: project.name.clone(),
        allowed_environment_ids: sa.allowed_environment_ids.clone(),
        scopes: SA_SCOPES.to_vec(),
    };
    Ok((
        format!("rise:sa:{}", sa.id),
        principal,
        AuditSubject::ServiceAccount {
            sa_id: sa.id,
            project: project.name,
        },
    ))
}

/// Resolve a controller exchange (no `rise_project`).
fn resolve_controller(
    state: &AppState,
    issuer: &str,
    claims: &serde_json::Value,
) -> Result<(String, PrincipalClaims, AuditSubject), ExchangeError> {
    let Some(candidates) = state.controllers_by_issuer.get(issuer) else {
        // Issuer is a known SA issuer but no project was supplied and it is not a
        // controller issuer — nothing to exchange.
        return Err(ExchangeError::invalid_grant(
            "rise_project is required for this token",
        ));
    };

    match match_controller_identity(claims, candidates) {
        ControllerMatch::Single(ident) => {
            let principal = PrincipalClaims::Controller {
                identity_id: ident.id.clone(),
            };
            Ok((
                format!("rise:ctrl:{}", ident.id),
                principal,
                AuditSubject::Controller {
                    identity_id: ident.id.clone(),
                },
            ))
        }
        ControllerMatch::Multiple(_) => Err(ExchangeError::invalid_grant(
            "subject_token matched multiple controller identities (ambiguous configuration)",
        )),
        ControllerMatch::Unmatched(_) => Err(ExchangeError::invalid_grant(
            "subject_token could not be validated",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an unsigned JWT-shaped string `header.payload.sig` for peeking.
    fn jwt_with_payload(payload: serde_json::Value) -> String {
        let header = BASE64URL.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let body = BASE64URL.encode(serde_json::to_vec(&payload).unwrap());
        format!("{header}.{body}.sig")
    }

    #[test]
    fn peek_issuer_reads_iss() {
        let token =
            jwt_with_payload(serde_json::json!({ "iss": "https://gitlab.com", "sub": "x" }));
        assert_eq!(peek_issuer(&token).as_deref(), Some("https://gitlab.com"));
    }

    #[test]
    fn peek_issuer_handles_missing_and_malformed() {
        // No `iss` claim.
        let token = jwt_with_payload(serde_json::json!({ "sub": "x" }));
        assert_eq!(peek_issuer(&token), None);
        // Not a JWT at all.
        assert_eq!(peek_issuer("not-a-jwt"), None);
        // Two segments only.
        assert_eq!(peek_issuer("a.b"), None);
    }
}
