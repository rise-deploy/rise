//! Controller identity resolution.
//!
//! A Controller authenticates with an OIDC JWT matching one live
//! `ControllerTrustPolicy` beneath a live root `Controller` resource — the
//! identities themselves live in the generic resource store, not in static
//! configuration. [`resolve_external`] does that match for a raw external
//! token; [`resolve_access`] re-checks liveness for an already-exchanged Rise
//! access token, since the resource may have been deleted since it was
//! minted.
//!
//! The extractor consuming these ([`AnyAuth`](crate::server::auth::context::AnyAuth))
//! turns a resolved [`ControllerPrincipal`] into an ordinary
//! `controller:<name>` principal for the generic resource API
//! (`src/server/authz/mod.rs`), which evaluates it exactly like a User.

use rise_backend_auth::{match_trust_candidates, TrustCandidate, TrustMatch};
use rise_resource_api::{Issuer, ResourceApi, StoreError, API_VERSION_V1ALPHA1, CONTROLLER_KIND};
use rise_resource_store_postgres::TrustPolicyLookup;
use sqlx::PgPool;
use uuid::Uuid;

use crate::server::error::ServerError;
use crate::server::resources::error_map::store_error_to_server_error;

/// A resolved, currently-live Controller identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControllerPrincipal {
    /// The Controller resource's canonical name — the subject `controller:<name>`
    /// names, and the writer key stored under `status.controllers`.
    pub name: String,
    /// The Controller resource's UID, carried in exchanged access tokens so a
    /// recreated same-name Controller invalidates old tokens.
    pub uid: Uuid,
}

/// Axum extractor wrapping a resolved controller principal.
#[derive(Clone, Debug)]
pub struct ControllerAuthContext(pub ControllerPrincipal);

/// Outcome of matching a raw external token's claims against live
/// `ControllerTrustPolicy` candidates for its issuer.
#[derive(Debug)]
pub enum ControllerResolution {
    /// Exactly one Controller matched (one or more of its trust policies did).
    Controller(ControllerPrincipal),
    /// No live trust policy on this issuer matched — the token is not a
    /// controller token at all (it may still be a service-account token).
    NotAController,
    /// Trust policies belonging to more than one distinct Controller matched —
    /// configuration is ambiguous.
    Ambiguous(Vec<String>),
}

/// Match a verified external token's issuer and claims against every live
/// `ControllerTrustPolicy` for that issuer.
///
/// Several policies matching the *same* Controller is an ordinary match, not
/// an ambiguity — a Controller may declare more than one accepted issuer or
/// claim shape. Policies matching *different* Controllers is what makes a
/// token ambiguous.
pub async fn resolve_external(
    pool: &PgPool,
    issuer: &str,
    claims: &serde_json::Value,
) -> Result<ControllerResolution, StoreError> {
    let Ok(issuer) = Issuer::new(issuer) else {
        // A non-canonical `iss` can never equal a stored policy's issuer.
        return Ok(ControllerResolution::NotAController);
    };
    let candidates = TrustPolicyLookup::new(pool.clone())
        .controller_candidates_by_issuer(&issuer)
        .await?;
    if candidates.is_empty() {
        return Ok(ControllerResolution::NotAController);
    }

    let labels: Vec<String> = candidates
        .iter()
        .map(|candidate| format!("{}/{}", candidate.controller_name, candidate.policy_name))
        .collect();
    let trust_candidates: Vec<TrustCandidate<'_>> = candidates
        .iter()
        .zip(labels.iter())
        .map(|(candidate, label)| TrustCandidate {
            label,
            claims: candidate.spec.claims.as_map(),
        })
        .collect();

    match match_trust_candidates(claims, &trust_candidates) {
        TrustMatch::Single(index) => Ok(ControllerResolution::Controller(ControllerPrincipal {
            name: candidates[index].controller_name.clone(),
            uid: candidates[index].controller_uid,
        })),
        TrustMatch::Multiple(indices) => {
            let mut uids: Vec<Uuid> = indices
                .iter()
                .map(|&index| candidates[index].controller_uid)
                .collect();
            uids.sort();
            uids.dedup();
            match uids.as_slice() {
                [uid] => {
                    let name = candidates
                        .iter()
                        .find(|candidate| candidate.controller_uid == *uid)
                        .map(|candidate| candidate.controller_name.clone())
                        .expect("uid collected from candidates");
                    Ok(ControllerResolution::Controller(ControllerPrincipal {
                        name,
                        uid: *uid,
                    }))
                }
                _ => {
                    let mut names: Vec<String> = uids
                        .iter()
                        .filter_map(|uid| {
                            candidates
                                .iter()
                                .find(|candidate| candidate.controller_uid == *uid)
                                .map(|candidate| candidate.controller_name.clone())
                        })
                        .collect();
                    names.sort();
                    names.dedup();
                    Ok(ControllerResolution::Ambiguous(names))
                }
            }
        }
        TrustMatch::Unmatched(_) => Ok(ControllerResolution::NotAController),
    }
}

/// Whether any live `ControllerTrustPolicy` declares this issuer — the cheap
/// guard used before deciding whether an unrecognized issuer is worth a JWKS
/// fetch or a token-exchange attempt at all. A non-canonical `iss` can never
/// match a stored policy, so it answers `false` without a query.
pub async fn controller_issuer_exists(pool: &PgPool, issuer: &str) -> Result<bool, StoreError> {
    let Ok(issuer) = Issuer::new(issuer) else {
        return Ok(false);
    };
    TrustPolicyLookup::new(pool.clone())
        .controller_issuer_exists(&issuer)
        .await
}

/// Re-check liveness for a controller principal already extracted from
/// `AccessClaims` by the caller.
///
/// The caller is responsible for recognizing a `PrincipalClaims::Controller`
/// claim before calling in — this function always treats `name`/`uid` as a
/// controller assertion. A controller principal that no longer resolves to a
/// live Controller of that name and uid is a hard failure — never a
/// fall-through to user auth, since the token unambiguously claims to be a
/// controller.
pub async fn resolve_access(
    store: &dyn ResourceApi,
    name: &str,
    uid: Uuid,
) -> Result<ControllerPrincipal, ServerError> {
    let row = store.get(uid).await.map_err(store_error_to_server_error)?;
    let live = row.as_ref().is_some_and(|row| {
        row.api_version == API_VERSION_V1ALPHA1
            && row.kind == CONTROLLER_KIND
            && row.parent_uid.is_none()
            && row.name == name
            && row.deletion_timestamp.is_none()
    });
    if live {
        Ok(ControllerPrincipal {
            name: name.to_string(),
            uid,
        })
    } else {
        Err(ServerError::unauthorized(
            "controller identity is no longer live",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rise_resource_api::{CreateResourceParams, ResourceApi, CONTROLLER_TRUST_POLICY_KIND};
    use rise_resource_store_postgres::PgResourceStore;

    /// Create a live `Controller` and one `ControllerTrustPolicy` beneath it,
    /// trusting `issuer` with `claims` (which must include `aud`). Returns the
    /// Controller's `(name, uid)`.
    async fn create_controller_trust_policy(
        store: &PgResourceStore,
        controller_name: &str,
        issuer: &str,
        claims: serde_json::Value,
    ) -> (String, Uuid) {
        let controller = store
            .create(CreateResourceParams {
                api_version: API_VERSION_V1ALPHA1.to_string(),
                kind: CONTROLLER_KIND.to_string(),
                name: controller_name.to_string(),
                spec: serde_json::json!({}),
                ..Default::default()
            })
            .await
            .expect("create Controller");
        store
            .create(CreateResourceParams {
                api_version: API_VERSION_V1ALPHA1.to_string(),
                kind: CONTROLLER_TRUST_POLICY_KIND.to_string(),
                name: "trust".to_string(),
                parent_uid: Some(controller.uid),
                spec: serde_json::json!({"issuer": issuer, "claims": claims}),
                ..Default::default()
            })
            .await
            .expect("create ControllerTrustPolicy");
        (controller_name.to_string(), controller.uid)
    }

    #[sqlx::test]
    async fn resolve_external_collapses_two_policies_on_one_controller(pool: sqlx::PgPool) {
        rise_resource_store_postgres::run_migrations(&pool)
            .await
            .expect("resource store migrations");
        let store = PgResourceStore::new(pool.clone());
        let (name, uid) = create_controller_trust_policy(
            &store,
            "reconciler",
            "https://issuer.example.com",
            serde_json::json!({"aud": "rise-controller", "sub": "deploy-bot"}),
        )
        .await;
        // A second trust policy on the same Controller, same issuer, a
        // different (also-matching) claim shape.
        store
            .create(CreateResourceParams {
                api_version: API_VERSION_V1ALPHA1.to_string(),
                kind: CONTROLLER_TRUST_POLICY_KIND.to_string(),
                name: "trust-2".to_string(),
                parent_uid: Some(uid),
                spec: serde_json::json!({
                    "issuer": "https://issuer.example.com",
                    "claims": {"aud": "rise-controller"},
                }),
                ..Default::default()
            })
            .await
            .expect("create second ControllerTrustPolicy");

        let token_claims = serde_json::json!({
            "iss": "https://issuer.example.com",
            "aud": "rise-controller",
            "sub": "deploy-bot",
        });
        match resolve_external(&pool, "https://issuer.example.com", &token_claims)
            .await
            .expect("resolve_external")
        {
            ControllerResolution::Controller(principal) => {
                assert_eq!(principal.name, name);
                assert_eq!(principal.uid, uid);
            }
            other => panic!("expected a single collapsed match, got {other:?}"),
        }
    }

    #[sqlx::test]
    async fn resolve_external_two_distinct_controllers_is_ambiguous(pool: sqlx::PgPool) {
        rise_resource_store_postgres::run_migrations(&pool)
            .await
            .expect("resource store migrations");
        let store = PgResourceStore::new(pool.clone());
        create_controller_trust_policy(
            &store,
            "reconciler-a",
            "https://issuer.example.com",
            serde_json::json!({"aud": "rise-controller"}),
        )
        .await;
        create_controller_trust_policy(
            &store,
            "reconciler-b",
            "https://issuer.example.com",
            serde_json::json!({"aud": "rise-controller"}),
        )
        .await;

        let token_claims = serde_json::json!({
            "iss": "https://issuer.example.com",
            "aud": "rise-controller",
        });
        match resolve_external(&pool, "https://issuer.example.com", &token_claims)
            .await
            .expect("resolve_external")
        {
            ControllerResolution::Ambiguous(mut names) => {
                names.sort();
                assert_eq!(
                    names,
                    vec!["reconciler-a".to_string(), "reconciler-b".to_string()]
                );
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[sqlx::test]
    async fn resolve_access_live_controller_succeeds(pool: sqlx::PgPool) {
        rise_resource_store_postgres::run_migrations(&pool)
            .await
            .expect("resource store migrations");
        let store = PgResourceStore::new(pool.clone());
        let (name, uid) = create_controller_trust_policy(
            &store,
            "reconciler",
            "https://issuer.example.com",
            serde_json::json!({"aud": "x"}),
        )
        .await;

        let principal = resolve_access(&store, &name, uid)
            .await
            .expect("live controller resolves");
        assert_eq!(principal.name, name);
        assert_eq!(principal.uid, uid);
    }

    #[sqlx::test]
    async fn resolve_access_deleted_controller_fails(pool: sqlx::PgPool) {
        rise_resource_store_postgres::run_migrations(&pool)
            .await
            .expect("resource store migrations");
        let store = PgResourceStore::new(pool.clone());
        let (name, uid) = create_controller_trust_policy(
            &store,
            "reconciler",
            "https://issuer.example.com",
            serde_json::json!({"aud": "x"}),
        )
        .await;
        store.delete(uid).await.expect("delete Controller");

        let err = resolve_access(&store, &name, uid)
            .await
            .expect_err("a deleted controller must not resolve");
        assert_eq!(err.status, axum::http::StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test]
    async fn resolve_access_name_mismatch_fails(pool: sqlx::PgPool) {
        rise_resource_store_postgres::run_migrations(&pool)
            .await
            .expect("resource store migrations");
        let store = PgResourceStore::new(pool.clone());
        create_controller_trust_policy(
            &store,
            "reconciler-a",
            "https://issuer.example.com",
            serde_json::json!({"aud": "x"}),
        )
        .await;
        let (_, uid_b) = create_controller_trust_policy(
            &store,
            "reconciler-b",
            "https://issuer.example.com",
            serde_json::json!({"aud": "x"}),
        )
        .await;

        // `uid_b` resolves to a live Controller, but not one named `reconciler-a`.
        let err = resolve_access(&store, "reconciler-a", uid_b)
            .await
            .expect_err("uid/name mismatch must not resolve");
        assert_eq!(err.status, axum::http::StatusCode::UNAUTHORIZED);
    }
}
