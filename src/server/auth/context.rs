use crate::db::models::User;
use crate::db::service_accounts;
use crate::server::auth::controller::{
    match_controller_identity, ControllerIdentity, ControllerMatch,
};
use crate::server::auth::jwt::JwtValidator;
use crate::server::error::{ServerError, ServerErrorExt};
use crate::server::state::AppState;
use axum::{extract::FromRequestParts, http::request::Parts};
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
}

impl AuthContext {
    /// Get the authenticated Rise user.
    ///
    /// Returns the user for Rise JWTs. Returns 401 for external tokens
    /// (endpoints that don't support service account authentication should call this).
    pub fn user(&self) -> Result<&User, ServerError> {
        match self {
            AuthContext::User(user) => Ok(user),
            AuthContext::ExternalToken(_) => Err(ServerError::unauthorized(
                "This endpoint does not support service account authentication",
            )),
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
            AuthContext::ExternalToken(token) => {
                if let Some(controller_candidates) = controllers_by_issuer.get(&token.issuer) {
                    match match_controller_identity(&token.claims, controller_candidates) {
                        ControllerMatch::Single(ident) => {
                            tracing::warn!(
                                "Controller token {} from issuer '{}' attempted service-account auth for project '{}'",
                                ident.id,
                                token.issuer,
                                project.name
                            );
                            return Err(ServerError::unauthorized(
                                "Controller tokens cannot be used as service accounts",
                            ));
                        }
                        ControllerMatch::Multiple(matched) => {
                            let ids: Vec<&str> =
                                matched.iter().map(|ident| ident.id.as_str()).collect();
                            tracing::error!(
                                "Multiple controller identities matched JWT from issuer '{}' during service-account auth: {:?}",
                                token.issuer,
                                ids
                            );
                            return Err(ServerError::conflict(
                                "Token matched multiple controller identities; configuration is ambiguous",
                            ));
                        }
                        ControllerMatch::Unmatched(_) => {}
                    }
                }

                // Find service accounts for this project + issuer
                let service_accounts =
                    service_accounts::find_by_project_and_issuer(pool, project.id, &token.issuer)
                        .await
                        .internal_err("Failed to look up service accounts")?;

                if service_accounts.is_empty() {
                    return Err(ServerError::unauthorized(format!(
                        "No service accounts configured for issuer '{}' on project '{}'",
                        token.issuer, project.name
                    )));
                }

                // Try each SA's expected claims against the token
                let mut matching_sas = Vec::new();
                let mut last_error = None;
                for sa in &service_accounts {
                    let expected_claims: HashMap<String, String> =
                        match serde_json::from_value(sa.claims.clone()) {
                            Ok(claims) => claims,
                            Err(e) => {
                                tracing::error!(
                                    "Failed to deserialize claims for service account {}: {}",
                                    sa.id,
                                    e
                                );
                                return Err(ServerError::internal(
                                    "Invalid service account claims configuration",
                                ));
                            }
                        };

                    match JwtValidator::validate_custom_claims(&token.claims, &expected_claims) {
                        Ok(()) => {
                            matching_sas.push(sa);
                        }
                        Err(e) => {
                            tracing::info!("SA {} claim mismatch: {}", sa.id, e);
                            last_error = Some(e);
                        }
                    }
                }

                if matching_sas.is_empty() {
                    // No SA matched — include validation details (safe: same project)
                    return Err(ServerError::unauthorized(format!(
                        "Token claims do not match any service account for project '{}': {}",
                        project.name,
                        last_error
                            .map(|e| e.to_string())
                            .unwrap_or_else(|| "unknown error".to_string()),
                    )));
                }

                if matching_sas.len() > 1 {
                    let sa_ids: Vec<String> =
                        matching_sas.iter().map(|sa| sa.id.to_string()).collect();
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

                // Exactly one match — look up the SA's synthetic user
                let sa = matching_sas[0];
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
    /// (i.e. an external JWT that has not yet been resolved to a project).
    ///
    /// After calling `resolve_for_project`, use the returned `is_sa` bool instead.
    pub fn is_service_account(&self) -> bool {
        matches!(self, AuthContext::ExternalToken(_))
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

        // Try VerifiedExternalToken extension (external JWT path)
        if let Some(token) = parts.extensions.get::<VerifiedExternalToken>().cloned() {
            return Ok(AuthContext::ExternalToken(token));
        }

        // Neither was set — middleware should have rejected the request
        Err(ServerError::unauthorized("Not authenticated"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{models::ProjectStatus, projects, service_accounts, users};
    use axum::http::StatusCode;

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
