use crate::db::models::User;
use crate::db::service_accounts;
use crate::server::auth::controller::{ControllerAuthContext, ControllerIdentity};
use crate::server::auth::sa_match::{match_service_account, SaMatchError};
use crate::server::error::{ServerError, ServerErrorExt};
use crate::server::state::AppState;
use axum::{extract::FromRequestParts, http::request::Parts, http::StatusCode};
use rise_backend_auth::{AccessClaims, PrincipalClaims};
use sqlx::PgPool;
use std::collections::HashMap;

/// A JWKS-validated external token (phase 1 of two-phase SA auth).
///
/// Stored in request extensions by the auth middleware after verifying the JWT
/// signature and expiry via JWKS. Custom claims have NOT been validated at this point;
/// that happens in phase 2 when the target project is known.
#[derive(Clone, Debug)]
pub struct VerifiedExternalToken {
    pub issuer: String,
    pub claims: serde_json::Value,
}

/// Authentication context for request handlers.
///
/// This replaces `Extension<User>` in all handlers and supports two-phase
/// service account authentication:
/// - `User`: A Rise JWT was validated — the user is known immediately.
/// - `ExternalToken`: An external JWT was JWKS-validated (signature + expiry).
///   The token's claims still need to be matched against a project's service
///   accounts via `resolve_for_project`.
#[derive(Clone, Debug)]
pub enum AuthContext {
    User(User),
    ExternalToken(VerifiedExternalToken),
    /// A Rise access token (RFC 8693 exchanged principal). In Phase 1 this is
    /// only ever a service account or controller — the exchange never mints a
    /// `User` access token. Recognized by handlers via `resolve_for_project`.
    Access(AccessClaims),
}

impl AuthContext {
    /// Get the authenticated Rise user.
    ///
    /// Returns the user for Rise JWTs. Returns 401 for service-account / access
    /// tokens (endpoints that don't support service account authentication
    /// should call this).
    pub fn user(&self) -> Result<&User, ServerError> {
        match self {
            AuthContext::User(user) => Ok(user),
            AuthContext::ExternalToken(_) | AuthContext::Access(_) => {
                Err(ServerError::unauthorized(
                    "This endpoint does not support service account authentication",
                ))
            }
        }
    }

    /// The embedded access-token claims, if this context is a Rise access token.
    ///
    /// Used by `create_deployment` to read the service-account environment
    /// restriction snapshot without a DB lookup.
    pub fn access_claims(&self) -> Option<&AccessClaims> {
        match self {
            AuthContext::Access(claims) => Some(claims),
            _ => None,
        }
    }

    /// Resolve authentication for a project-scoped endpoint.
    ///
    /// - For Rise JWTs: returns `(user, false)` (the bool indicates `is_service_account`).
    /// - For external tokens: looks up service accounts for `(project_id, issuer)`,
    ///   validates claims against each SA's expected claims, and returns the SA's
    ///   synthetic user on first match. Error messages include expected claim values
    ///   since they are scoped to the same project (no cross-project leakage).
    pub async fn resolve_for_project(
        &self,
        pool: &PgPool,
        project: &crate::db::models::Project,
        controllers_by_issuer: &HashMap<String, Vec<ControllerIdentity>>,
    ) -> Result<(User, bool), ServerError> {
        match self {
            AuthContext::User(user) => Ok((user.clone(), false)),
            AuthContext::Access(claims) => resolve_access_for_project(pool, project, claims).await,
            AuthContext::ExternalToken(token) => {
                // Find service accounts for this project + issuer, then run the
                // shared matcher (controller rejection + per-SA claim matching).
                let service_accounts =
                    service_accounts::find_by_project_and_issuer(pool, project.id, &token.issuer)
                        .await
                        .internal_err("Failed to look up service accounts")?;

                let sa = match match_service_account(
                    &token.claims,
                    &token.issuer,
                    &service_accounts,
                    controllers_by_issuer,
                ) {
                    Ok(sa) => sa,
                    Err(SaMatchError::ControllerToken) => {
                        tracing::warn!(
                            "Controller token from issuer '{}' attempted service-account auth for project '{}'",
                            token.issuer,
                            project.name
                        );
                        return Err(ServerError::unauthorized(
                            "Controller tokens cannot be used as service accounts",
                        ));
                    }
                    Err(SaMatchError::AmbiguousController) => {
                        tracing::error!(
                            "Multiple controller identities matched JWT from issuer '{}' during service-account auth",
                            token.issuer
                        );
                        return Err(ServerError::conflict(
                            "Token matched multiple controller identities; configuration is ambiguous",
                        ));
                    }
                    Err(SaMatchError::MalformedClaims(sa_id)) => {
                        tracing::error!(
                            "Failed to deserialize claims for service account {}",
                            sa_id
                        );
                        return Err(ServerError::internal(
                            "Invalid service account claims configuration",
                        ));
                    }
                    Err(SaMatchError::NoMatch {
                        had_candidates: false,
                        ..
                    }) => {
                        return Err(ServerError::unauthorized(format!(
                            "No service accounts configured for issuer '{}' on project '{}'",
                            token.issuer, project.name
                        )));
                    }
                    Err(SaMatchError::NoMatch {
                        had_candidates: true,
                        last_error,
                    }) => {
                        return Err(ServerError::unauthorized(format!(
                            "Token claims do not match any service account for project '{}': {}",
                            project.name,
                            last_error
                                .map(|e| e.to_string())
                                .unwrap_or_else(|| "unknown error".to_string()),
                        )));
                    }
                    Err(SaMatchError::Ambiguous(sa_ids)) => {
                        tracing::error!(
                            "Multiple service accounts matched JWT on project '{}': {:?}. \
                             This indicates ambiguous claim configuration.",
                            project.name,
                            sa_ids
                        );
                        return Err(ServerError::conflict(
                            "Multiple service accounts match the provided claims",
                        ));
                    }
                };

                // Exactly one match — look up the SA's synthetic user
                let user = crate::db::users::find_by_id(pool, sa.user_id)
                    .await
                    .internal_err("Failed to look up service account user")?
                    .ok_or_else(|| {
                        ServerError::internal("Service account user not found in database")
                    })?;

                tracing::info!(
                    "Service account {} authenticated for project '{}'",
                    user.email,
                    project.name
                );

                Ok((user, true))
            }
        }
    }

    /// Returns `true` if this auth context represents a service account token
    /// (an external JWT not yet resolved to a project, or an exchanged access
    /// token carrying a service-account principal).
    ///
    /// After calling `resolve_for_project`, use the returned `is_sa` bool instead.
    pub fn is_service_account(&self) -> bool {
        match self {
            AuthContext::ExternalToken(_) => true,
            AuthContext::Access(claims) => {
                matches!(claims.principal, PrincipalClaims::ServiceAccount { .. })
            }
            AuthContext::User(_) => false,
        }
    }
}

/// Resolve a Rise access token for a project-scoped endpoint.
///
/// The principal was fully resolved at exchange time, so this is a snap decision
/// plus a single synthetic-user lookup:
/// - `ServiceAccount`: the token is bound to one project. Assert the bound
///   `project_id` matches the project the handler resolved (whether named in the
///   request or discovered from a deployment id). A mismatch is reported as an
///   auth failure (masked to 404 by callers) — an SA may act only within its
///   bound project.
/// - `Controller` / `User`: not valid service-account principals on these
///   project-scoped endpoints.
async fn resolve_access_for_project(
    pool: &PgPool,
    project: &crate::db::models::Project,
    claims: &AccessClaims,
) -> Result<(User, bool), ServerError> {
    match &claims.principal {
        PrincipalClaims::ServiceAccount {
            synthetic_user_id,
            project_id,
            ..
        } => {
            if *project_id != project.id {
                tracing::warn!(
                    "Access token bound to project {} used against project '{}' ({})",
                    project_id,
                    project.name,
                    project.id
                );
                return Err(ServerError::unauthorized(
                    "Access token is not valid for this project",
                ));
            }
            let user = crate::db::users::find_by_id(pool, *synthetic_user_id)
                .await
                .internal_err("Failed to look up service account user")?
                .ok_or_else(|| {
                    ServerError::internal("Service account user not found in database")
                })?;
            Ok((user, true))
        }
        PrincipalClaims::Controller { .. } => Err(ServerError::unauthorized(
            "Controller tokens cannot be used as service accounts",
        )),
        PrincipalClaims::User { .. } => Err(ServerError::unauthorized(
            "This endpoint does not support user access tokens",
        )),
    }
}

impl FromRequestParts<AppState> for AuthContext {
    type Rejection = ServerError;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Try User extension first (Rise JWT path)
        if let Some(user) = parts.extensions.get::<User>().cloned() {
            return Ok(AuthContext::User(user));
        }

        // Try the exchanged access-token extension (Rise access token path)
        if let Some(claims) = parts.extensions.get::<AccessClaims>().cloned() {
            return Ok(AuthContext::Access(claims));
        }

        // Try VerifiedExternalToken extension (legacy raw external JWT path)
        if let Some(token) = parts.extensions.get::<VerifiedExternalToken>().cloned() {
            return Ok(AuthContext::ExternalToken(token));
        }

        // None was set — middleware should have rejected the request
        Err(ServerError::unauthorized("Not authenticated"))
    }
}

/// Authentication context that accepts either a user/SA token or a controller token.
///
/// Used by endpoints that must handle both kinds of callers (e.g. `dispatch_put`).
/// Controller auth is tried first (it is more specific — requires both a
/// `VerifiedExternalToken` extension *and* matching `ControllerIdentity` claims).
/// If that fails, regular user/SA auth is attempted.
#[derive(Clone, Debug)]
pub enum AnyAuth {
    User(AuthContext),
    Controller(ControllerAuthContext),
}

impl FromRequestParts<AppState> for AnyAuth {
    type Rejection = ServerError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Try controller first (more specific — requires VerifiedExternalToken
        // AND matching ControllerIdentity claims).
        // Only fall back to user/SA auth on 401 (not a controller token).
        // Any other error (e.g. 409 ambiguous config) is propagated immediately.
        match ControllerAuthContext::from_request_parts(parts, state).await {
            Ok(ctrl) => return Ok(AnyAuth::Controller(ctrl)),
            Err(e) if e.status == StatusCode::UNAUTHORIZED => {}
            Err(e) => return Err(e),
        }
        // Fall back to user/SA auth
        Ok(AnyAuth::User(
            AuthContext::from_request_parts(parts, state).await?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{models::ProjectStatus, projects, service_accounts, users};

    fn empty_controller_index() -> HashMap<String, Vec<ControllerIdentity>> {
        HashMap::new()
    }

    /// Helper: create a project and an external token auth context.
    async fn setup(
        pool: &PgPool,
        issuer: &str,
        sa_claims: &HashMap<String, String>,
        token_claims: serde_json::Value,
    ) -> (crate::db::models::Project, AuthContext) {
        let owner = users::create(pool, "owner@example.com").await.unwrap();
        let project = projects::create(
            pool,
            "test-project",
            ProjectStatus::Stopped,
            "public".to_string(),
            Some(owner.id),
            None,
            None,
        )
        .await
        .unwrap();
        service_accounts::create(pool, project.id, issuer, sa_claims)
            .await
            .unwrap();
        let auth = AuthContext::ExternalToken(VerifiedExternalToken {
            issuer: issuer.to_string(),
            claims: token_claims,
        });
        (project, auth)
    }

    #[sqlx::test]
    async fn test_resolve_single_match(pool: PgPool) {
        let mut expected = HashMap::new();
        expected.insert("sub".to_string(), "deploy-bot".to_string());

        let token_claims = serde_json::json!({"sub": "deploy-bot", "iss": "https://gitlab.com"});

        let (project, auth) = setup(&pool, "https://gitlab.com", &expected, token_claims).await;

        let controllers_by_issuer = empty_controller_index();
        let (user, is_sa) = auth
            .resolve_for_project(&pool, &project, &controllers_by_issuer)
            .await
            .unwrap();
        assert!(is_sa);
        assert!(user.email.contains("test-project"));
    }

    #[sqlx::test]
    async fn test_resolve_no_match(pool: PgPool) {
        let mut expected = HashMap::new();
        expected.insert("sub".to_string(), "deploy-bot".to_string());

        let token_claims = serde_json::json!({"sub": "wrong-subject", "iss": "https://gitlab.com"});

        let (project, auth) = setup(&pool, "https://gitlab.com", &expected, token_claims).await;

        let controllers_by_issuer = empty_controller_index();
        let err = auth
            .resolve_for_project(&pool, &project, &controllers_by_issuer)
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
        assert!(err.message.contains("do not match"));
    }

    #[sqlx::test]
    async fn test_resolve_rejects_controller_token_before_sa_match(pool: PgPool) {
        let mut expected = HashMap::new();
        expected.insert("aud".to_string(), "rise-controller".to_string());
        expected.insert("sub".to_string(), "deploy-bot".to_string());

        let token_claims = serde_json::json!({
            "aud": "rise-controller",
            "sub": "deploy-bot",
            "iss": "https://issuer.example.com"
        });

        let (project, auth) =
            setup(&pool, "https://issuer.example.com", &expected, token_claims).await;
        let controllers_by_issuer = HashMap::from([(
            "https://issuer.example.com".to_string(),
            vec![ControllerIdentity {
                id: "controller.example.com".to_string(),
                issuer: "https://issuer.example.com".to_string(),
                claims: HashMap::from([
                    ("aud".to_string(), "rise-controller".to_string()),
                    ("sub".to_string(), "deploy-bot".to_string()),
                ]),
            }],
        )]);

        let err = auth
            .resolve_for_project(&pool, &project, &controllers_by_issuer)
            .await
            .unwrap_err();

        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
        assert!(err.message.contains("Controller tokens cannot be used"));
    }

    #[sqlx::test]
    async fn test_resolve_allows_same_issuer_different_audience_sa(pool: PgPool) {
        let mut expected = HashMap::new();
        expected.insert("aud".to_string(), "rise-service-account".to_string());
        expected.insert("sub".to_string(), "deploy-bot".to_string());

        let token_claims = serde_json::json!({
            "aud": "rise-service-account",
            "sub": "deploy-bot",
            "iss": "https://issuer.example.com"
        });

        let (project, auth) =
            setup(&pool, "https://issuer.example.com", &expected, token_claims).await;
        let controllers_by_issuer = HashMap::from([(
            "https://issuer.example.com".to_string(),
            vec![ControllerIdentity {
                id: "controller.example.com".to_string(),
                issuer: "https://issuer.example.com".to_string(),
                claims: HashMap::from([
                    ("aud".to_string(), "rise-controller".to_string()),
                    ("sub".to_string(), "deploy-bot".to_string()),
                ]),
            }],
        )]);

        let (user, is_sa) = auth
            .resolve_for_project(&pool, &project, &controllers_by_issuer)
            .await
            .unwrap();

        assert!(is_sa);
        assert!(user.email.contains("test-project"));
    }

    #[sqlx::test]
    async fn test_resolve_collision_returns_conflict(pool: PgPool) {
        let claims = HashMap::new(); // empty claims match everything
        let token_claims = serde_json::json!({"iss": "https://gitlab.com"});

        let (project, auth) = setup(&pool, "https://gitlab.com", &claims, token_claims).await;

        // Create a second SA with the same empty claims → both will match
        service_accounts::create(&pool, project.id, "https://gitlab.com", &claims)
            .await
            .unwrap();

        let controllers_by_issuer = empty_controller_index();
        let err = auth
            .resolve_for_project(&pool, &project, &controllers_by_issuer)
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::CONFLICT);
    }

    #[sqlx::test]
    async fn test_resolve_malformed_claims_fails_closed(pool: PgPool) {
        let owner = users::create(&pool, "owner@example.com").await.unwrap();
        let project = projects::create(
            &pool,
            "test-project",
            ProjectStatus::Stopped,
            "public".to_string(),
            Some(owner.id),
            None,
            None,
        )
        .await
        .unwrap();

        // Insert a SA with non-string claim values (invalid for HashMap<String, String>)
        let bad_claims = serde_json::json!({"sub": 12345});
        service_accounts::create_with_raw_claims(
            &pool,
            project.id,
            "https://gitlab.com",
            bad_claims,
        )
        .await
        .unwrap();

        let auth = AuthContext::ExternalToken(VerifiedExternalToken {
            issuer: "https://gitlab.com".to_string(),
            claims: serde_json::json!({"sub": "12345"}),
        });

        let controllers_by_issuer = empty_controller_index();
        let err = auth
            .resolve_for_project(&pool, &project, &controllers_by_issuer)
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(err.message.contains("Invalid service account claims"));
    }

    /// An access token bound to a project resolves to the SA's synthetic user
    /// when used against that project.
    #[sqlx::test]
    async fn test_resolve_access_token_matching_project(pool: PgPool) {
        let owner = users::create(&pool, "owner@example.com").await.unwrap();
        let sa_user = users::create(&pool, "sa@test-project.example.com")
            .await
            .unwrap();
        let project = projects::create(
            &pool,
            "test-project",
            ProjectStatus::Stopped,
            "public".to_string(),
            Some(owner.id),
            None,
            None,
        )
        .await
        .unwrap();

        let claims = AccessClaims {
            iss: "https://rise.test".to_string(),
            aud: "https://rise.test".to_string(),
            sub: format!("rise:sa:{}", uuid::Uuid::new_v4()),
            iat: 0,
            exp: u64::MAX,
            jti: "jti".to_string(),
            principal: PrincipalClaims::ServiceAccount {
                service_account_id: uuid::Uuid::new_v4(),
                synthetic_user_id: sa_user.id,
                project_id: project.id,
                project_name: project.name.clone(),
                allowed_environment_ids: None,
                scopes: vec![],
            },
        };
        let auth = AuthContext::Access(claims);
        let (user, is_sa) = auth
            .resolve_for_project(&pool, &project, &empty_controller_index())
            .await
            .unwrap();
        assert!(is_sa);
        assert_eq!(user.id, sa_user.id);
    }

    /// An access token bound to project P must NOT resolve against project Q.
    #[sqlx::test]
    async fn test_resolve_access_token_project_mismatch(pool: PgPool) {
        let owner = users::create(&pool, "owner@example.com").await.unwrap();
        let project = projects::create(
            &pool,
            "other-project",
            ProjectStatus::Stopped,
            "public".to_string(),
            Some(owner.id),
            None,
            None,
        )
        .await
        .unwrap();

        let claims = AccessClaims {
            iss: "https://rise.test".to_string(),
            aud: "https://rise.test".to_string(),
            sub: "rise:sa:x".to_string(),
            iat: 0,
            exp: u64::MAX,
            jti: "jti".to_string(),
            principal: PrincipalClaims::ServiceAccount {
                service_account_id: uuid::Uuid::new_v4(),
                synthetic_user_id: uuid::Uuid::new_v4(),
                // Bound to a different project than `project`.
                project_id: uuid::Uuid::new_v4(),
                project_name: "bound-project".to_string(),
                allowed_environment_ids: None,
                scopes: vec![],
            },
        };
        let auth = AuthContext::Access(claims);
        let err = auth
            .resolve_for_project(&pool, &project, &empty_controller_index())
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test]
    async fn test_resolve_rise_jwt_returns_user(pool: PgPool) {
        let user = users::create(&pool, "regular@example.com").await.unwrap();
        let project = projects::create(
            &pool,
            "test-project",
            ProjectStatus::Stopped,
            "public".to_string(),
            Some(user.id),
            None,
            None,
        )
        .await
        .unwrap();

        let auth = AuthContext::User(user.clone());
        let controllers_by_issuer = empty_controller_index();
        let (resolved_user, is_sa) = auth
            .resolve_for_project(&pool, &project, &controllers_by_issuer)
            .await
            .unwrap();
        assert!(!is_sa);
        assert_eq!(resolved_user.id, user.id);
    }
}
