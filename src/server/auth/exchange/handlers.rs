//! RFC 8693 token-exchange handler.
//!
//! Exchanges an external OIDC subject token (+ optional Rise identity) for a
//! short-lived, Rise-signed **access token** that fully encodes the resolved
//! principal. This is the single place an external CI identity (`iss` + `sub`)
//! maps to a Rise principal, so every successful mint is audit-logged.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as BASE64URL, Engine as _};

use crate::db::{projects, service_accounts, users};
use crate::server::auth::controller::{self, ControllerResolution};
use crate::server::auth::sa_match::{match_service_account, SaMatchError};
use crate::server::rate_limit::extract_client_ip;
use crate::server::state::AppState;
use rise_backend_auth::{
    is_rise_issued_jwt, verify_external_jwt, AuthError, PrincipalClaims, Scope,
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

    // 2. Issuer guard: only live controller trust policies or known SA
    //    issuers. Avoid leaking unknown-issuer vs no-match beyond the coarse
    //    invalid_grant.
    let issuer_known = controller::controller_issuer_exists(&state.db_pool, &issuer)
        .await
        .map_err(|e| {
            tracing::error!(
                "Token exchange: failed to check controller issuer existence: {:?}",
                e
            );
            ExchangeError::temporarily_unavailable("issuer lookup failed")
        })?
        || {
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

    // 4 / 5. Resolve the principal. An `identity` selects a service account by
    // its synthetic-user email; without one, this is a controller exchange.
    let (sub, principal, audit) = if let Some(identity) = req
        .identity
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        resolve_service_account(&state.db_pool, identity, &issuer, claims).await?
    } else {
        resolve_controller(&state.db_pool, &issuer, claims).await?
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
        AuditSubject::Controller { name, uid } => tracing::info!(
            controller = %name,
            controller_uid = %uid,
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
#[derive(Debug)]
enum AuditSubject {
    ServiceAccount { sa_id: uuid::Uuid, project: String },
    Controller { name: String, uid: uuid::Uuid },
}

/// Resolve a project service-account exchange. `identity` is the SA's
/// synthetic-user email — a *selector*: the subject token must still prove it
/// is allowed to assume that SA (its `issuer_url` and expected claims must
/// match the verified token). Every failure that depends on whether a given
/// identity exists collapses to the same coarse `invalid_grant` so SA emails
/// cannot be enumerated through this endpoint.
async fn resolve_service_account(
    pool: &sqlx::PgPool,
    identity: &str,
    issuer: &str,
    claims: &serde_json::Value,
) -> Result<(String, PrincipalClaims, AuditSubject), ExchangeError> {
    // The coarse, non-leaky rejection shared by every "this identity can't be
    // assumed with this token" outcome (unknown email, not-an-SA, wrong issuer,
    // claim mismatch). Must stay byte-identical so the cases are indistinguishable.
    let reject = || ExchangeError::invalid_grant("subject_token could not be validated");

    // A token satisfying a live controller trust policy is never a service
    // account, whatever claims it also happens to carry.
    match controller::resolve_external(pool, issuer, claims)
        .await
        .map_err(|e| {
            tracing::error!("Token exchange: controller lookup failed: {:?}", e);
            ExchangeError::temporarily_unavailable("controller lookup failed")
        })? {
        ControllerResolution::Controller(_) | ControllerResolution::Ambiguous(_) => {
            return Err(reject());
        }
        ControllerResolution::NotAController => {}
    }

    // identity (email) -> synthetic user.
    let user = match users::find_by_email(pool, identity).await {
        Ok(Some(u)) => u,
        Ok(None) => return Err(reject()),
        Err(e) => {
            tracing::error!("Token exchange: failed to look up identity user: {:?}", e);
            return Err(ExchangeError::temporarily_unavailable(
                "identity lookup failed",
            ));
        }
    };

    // synthetic user -> the (single, active) service account.
    let sa = match service_accounts::find_active_by_user_id(pool, user.id).await {
        Ok(Some(sa)) => sa,
        Ok(None) => return Err(reject()),
        Err(e) => {
            tracing::error!("Token exchange: failed to look up service account: {:?}", e);
            return Err(ExchangeError::temporarily_unavailable(
                "service account lookup failed",
            ));
        }
    };

    // SECURITY: the email lookup is not issuer-scoped (unlike the by-project
    // query), so an SA configured to trust issuer A must not be assumable with a
    // token from issuer B. Enforce the issuer match before validating claims.
    if sa.issuer_url != issuer {
        return Err(reject());
    }

    // Validate the token's claims against the SA. The slice is single-element,
    // so `Ambiguous` is unreachable, but it is handled defensively rather than
    // via `unreachable!`.
    match match_service_account(claims, std::slice::from_ref(&sa)) {
        Ok(_) => {}
        Err(SaMatchError::MalformedClaims(sa_id)) => {
            tracing::error!(
                "Token exchange: malformed claims on service account {}",
                sa_id
            );
            return Err(ExchangeError::temporarily_unavailable(
                "service account claims configuration is invalid",
            ));
        }
        // NoMatch / Ambiguous(*) both fail closed and coarse.
        Err(_) => return Err(reject()),
    };

    // SA -> project (for the principal's project_name).
    let project = match projects::find_by_id(pool, sa.project_id).await {
        Ok(Some(p)) => p,
        // An active SA always has a project; a missing one is an invariant violation.
        Ok(None) => {
            tracing::error!(
                "Token exchange: service account {} references missing project {}",
                sa.id,
                sa.project_id
            );
            return Err(ExchangeError::temporarily_unavailable(
                "project lookup failed",
            ));
        }
        Err(e) => {
            tracing::error!("Token exchange: failed to look up project: {:?}", e);
            return Err(ExchangeError::temporarily_unavailable(
                "project lookup failed",
            ));
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

/// Resolve a controller exchange (no `identity` supplied).
async fn resolve_controller(
    pool: &sqlx::PgPool,
    issuer: &str,
    claims: &serde_json::Value,
) -> Result<(String, PrincipalClaims, AuditSubject), ExchangeError> {
    match controller::resolve_external(pool, issuer, claims)
        .await
        .map_err(|e| {
            tracing::error!("Token exchange: controller lookup failed: {:?}", e);
            ExchangeError::temporarily_unavailable("controller lookup failed")
        })? {
        ControllerResolution::Controller(principal) => {
            let claim = PrincipalClaims::Controller {
                name: principal.name.clone(),
                uid: principal.uid,
            };
            Ok((
                format!("controller:{}", principal.name),
                claim,
                AuditSubject::Controller {
                    name: principal.name,
                    uid: principal.uid,
                },
            ))
        }
        ControllerResolution::Ambiguous(_) => Err(ExchangeError::invalid_grant(
            "subject_token matched multiple controller identities (ambiguous configuration)",
        )),
        // Issuer is a known SA issuer but no identity was supplied and it is
        // not a controller issuer — nothing to exchange.
        ControllerResolution::NotAController => Err(ExchangeError::invalid_grant(
            "identity is required for this token",
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

    mod resolve_by_identity {
        use super::*;
        use crate::db::models::ProjectStatus;
        use crate::db::{projects, service_accounts, users};
        use std::collections::HashMap;

        /// Create a project + SA trusting `issuer` with `sub`-matching claims;
        /// return `(sa_id, project_id, the SA's synthetic-user email)`.
        async fn setup(pool: &sqlx::PgPool, issuer: &str) -> (uuid::Uuid, uuid::Uuid, String) {
            // resolve_service_account also checks live controller trust
            // policies, which live in the resource-store schema.
            rise_resource_store_postgres::run_migrations(pool)
                .await
                .expect("resource store migrations");
            let owner = users::create(pool, "owner@example.com").await.unwrap();
            let project = projects::create(
                pool,
                "demo",
                ProjectStatus::Stopped,
                "public".to_string(),
                Some(owner.id),
                None,
                None,
            )
            .await
            .unwrap();
            let claims = HashMap::from([("sub".to_string(), "deploy-bot".to_string())]);
            let sa = service_accounts::create(pool, project.id, issuer, &claims)
                .await
                .unwrap();
            let email = users::find_by_id(pool, sa.user_id)
                .await
                .unwrap()
                .unwrap()
                .email;
            (sa.id, project.id, email)
        }

        fn is_invalid_grant(err: &ExchangeError) -> bool {
            matches!(
                err,
                ExchangeError::OAuth {
                    error: "invalid_grant",
                    ..
                }
            )
        }

        #[sqlx::test]
        async fn succeeds_for_matching_issuer_and_claims(pool: sqlx::PgPool) {
            let (sa_id, project_id, email) = setup(&pool, "https://gitlab.com").await;
            let token_claims =
                serde_json::json!({ "sub": "deploy-bot", "iss": "https://gitlab.com" });
            let (sub, principal, _audit) =
                resolve_service_account(&pool, &email, "https://gitlab.com", &token_claims)
                    .await
                    .unwrap();
            assert_eq!(sub, format!("rise:sa:{sa_id}"));
            match principal {
                PrincipalClaims::ServiceAccount { project_id: p, .. } => assert_eq!(p, project_id),
                _ => panic!("expected SA principal"),
            }
        }

        #[sqlx::test]
        async fn rejects_issuer_mismatch(pool: sqlx::PgPool) {
            // The SA trusts gitlab; a token from another issuer must not assume it
            // even though the claims match (the security guard the email lookup
            // needs, since it isn't issuer-scoped).
            let (_sa, _proj, email) = setup(&pool, "https://gitlab.com").await;
            let token_claims =
                serde_json::json!({ "sub": "deploy-bot", "iss": "https://evil.example" });
            let err = resolve_service_account(&pool, &email, "https://evil.example", &token_claims)
                .await
                .unwrap_err();
            assert!(is_invalid_grant(&err));
        }

        #[sqlx::test]
        async fn rejects_unknown_identity(pool: sqlx::PgPool) {
            // resolve_service_account also checks live controller trust
            // policies, which live in the resource-store schema.
            rise_resource_store_postgres::run_migrations(&pool)
                .await
                .expect("resource store migrations");
            let token_claims = serde_json::json!({ "sub": "x", "iss": "https://gitlab.com" });
            let err = resolve_service_account(
                &pool,
                "nobody+0@sa.rise.local",
                "https://gitlab.com",
                &token_claims,
            )
            .await
            .unwrap_err();
            assert!(is_invalid_grant(&err));
        }
    }
}
