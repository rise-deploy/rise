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

/// Re-check liveness for an already-exchanged Rise access token carrying a
/// controller principal.
///
/// Returns `Ok(None)` when `claims` does not carry a controller principal at
/// all (so the caller can fall through to other principal kinds). A
/// controller principal that no longer resolves to a live Controller of that
/// name and uid is a hard failure — never a fall-through to user auth, since
/// the token unambiguously claims to be a controller.
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
