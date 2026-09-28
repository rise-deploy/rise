//! The `token` subresource of ServiceAccount and Controller resources
//! (ADR-0001 §7).
//!
//! `POST …/serviceaccounts/{org}/{name}/token` and `POST …/controllers/{name}/token`
//! issue a Rise identity token for exactly the resource the URL names, in one
//! of two disjoint modes:
//!
//! - **Workload token exchange.** The request carries an external workload JWT
//!   as `subject_token` and no Rise credential. Rise validates it against the
//!   trust-policy children of that one target and, on exactly one match,
//!   issues a token for the target. This is authentication *as* the workload's
//!   configured identity: no RBAC check, and no search for which identity a
//!   token might belong to. Every failure after the route is entered — a
//!   missing or draining target, an unaccepted issuer, a bad signature, no or
//!   several matching policies — is the same coarse 401, so the route never
//!   confirms whether a named identity or policy exists.
//! - **Delegated issuance.** The request carries a Rise credential and no
//!   external assertion. The caller needs `(create, <kind>, token)` on that
//!   exact target under its own capped effective policy; the minted token
//!   records the caller in a bounded `act` chain and exercises the *target's*
//!   live policy, never the caller's.
//!
//! Both modes accept `authorization_details` that only ever narrow the issued
//! token. The subresource persists nothing: a token is a credential, not a
//! resource, and it is create-only.

use std::collections::HashMap;
use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as BASE64URL, Engine as _};
use rise_authz::engine::{AuthorizationCap, ResourceTree};
use rise_backend_auth::{
    audience_matches, is_rise_issued_jwt, validate_custom_claims, ActorClaim, IdentityTokenSpec,
    JwksKeySource, RiseTokenSigner, MAX_DELEGATION_DEPTH,
};
use rise_resource_api::{
    CollectionInfo, Issuer, ResourceStore, SubjectId, TrustPolicyClaims, Verb, API_GROUP,
    CONTROLLER_KIND, SERVICE_ACCOUNT_KIND,
};
use rise_resource_store_postgres::{TrustPolicyFact, TrustPolicyLookup};
use serde::Deserialize;

use super::handlers::{classify_path, resolve_leaf, LeafRef, ResolvedCollection, ResolvedPath};
use super::path::{RawResourcePath, Subresource};
use crate::server::auth::context::{AnyAuth, AuthContext};
use crate::server::auth::exchange::models::{
    ExchangeResponse, GRANT_TYPE_TOKEN_EXCHANGE, MAX_SUBJECT_TOKEN_LEN, TOKEN_TYPE_JWT,
};
use crate::server::authz::ResourceAuthorizer;
use crate::server::error::ServerError;
use crate::server::rate_limit::{rate_limit_response, OAuthRateLimiter};
use crate::server::resources::error_map::{store_error_to_server_error, RESOURCE_NOT_FOUND};

/// The one body every workload-exchange failure returns after the route is
/// entered. Byte-identical across causes on purpose (ADR-0001 §7).
pub const WORKLOAD_EXCHANGE_REJECTED: &str = "the assertion was not accepted for this target";

/// Rate-limit bucket for assertions whose target is not yet resolved.
const PRE_VALIDATION_BUCKET: &str = "resource-token-exchange";

/// What a `/token` route needs beyond the resource store.
pub struct TokenService {
    signer: Arc<RiseTokenSigner>,
    jwks: Arc<dyn JwksKeySource>,
    trust_policies: TrustPolicyLookup,
    /// The Rise public URL: the `aud` of every issued token, and the issuer no
    /// trust policy may name as an external source.
    audience: String,
    /// The platform-global maximum token lifetime (ADR-0001 §7).
    max_ttl_seconds: u64,
    rate_limiter: Option<Arc<OAuthRateLimiter>>,
}

impl TokenService {
    pub fn new(
        signer: Arc<RiseTokenSigner>,
        jwks: Arc<dyn JwksKeySource>,
        trust_policies: TrustPolicyLookup,
        audience: String,
        max_ttl_seconds: u64,
        rate_limiter: Option<Arc<OAuthRateLimiter>>,
    ) -> Self {
        Self {
            signer,
            jwks,
            trust_policies,
            audience,
            max_ttl_seconds,
            rate_limiter,
        }
    }

    /// Rise's own issuer, which a trust policy may never name as an external
    /// source (ADR-0001 §7, scenario 47).
    pub fn rise_issuer(&self) -> &str {
        &self.audience
    }
}

/// The two kinds that register `token`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetKind {
    ServiceAccount,
    Controller,
}

/// Whether a collection registers the `token` subresource. Registration is
/// fixed to the two built-in identity kinds (ADR-0001 §2).
fn registered_target(info: &CollectionInfo) -> Option<TargetKind> {
    if info.api_version.split('/').next() != Some(API_GROUP) {
        return None;
    }
    match info.kind.as_str() {
        SERVICE_ACCOUNT_KIND => Some(TargetKind::ServiceAccount),
        CONTROLLER_KIND => Some(TargetKind::Controller),
        _ => None,
    }
}

/// The ordinary answer for a subresource route that does not exist. A kind
/// that does not register `token` has no such route, so this is returned
/// before any authentication or authorization (ADR-0001 §7, scenario 45).
fn route_not_found() -> ServerError {
    ServerError::not_found("no such route: this kind does not register the token subresource")
}

/// Request body of a `/token` POST.
///
/// `subject_token` selects workload exchange; its absence selects delegated
/// issuance. The RFC 8693 fields are validated when present so a caller of the
/// transitional exchange endpoint can move here with the same body.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenRequest {
    #[serde(default)]
    pub grant_type: Option<String>,
    #[serde(default)]
    pub subject_token: Option<String>,
    #[serde(default)]
    pub subject_token_type: Option<String>,
    /// RFC 9396 narrowing; `null` is as invalid as any other malformed value.
    #[serde(default, deserialize_with = "present_even_if_null")]
    pub authorization_details: Option<serde_json::Value>,
    /// The `aud` to mint for (RFC 8693 `audience`). Omitted means Rise's own
    /// API; any other value produces a token for an external verifier, which
    /// checks it against Rise's published JWKS and which Rise's API refuses.
    #[serde(default)]
    pub audience: Option<String>,
    /// Requested lifetime in seconds, clamped to the platform maximum.
    #[serde(default)]
    pub expires_in: Option<u64>,
}

/// Longest accepted `audience`, matching the workload identity endpoint.
const MAX_AUDIENCE_LEN: usize = 1024;

/// Keep an explicit `null` distinct from an omitted field: the omitted claim
/// means the full live policy, and `null` must not quietly mean the same.
fn present_even_if_null<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<serde_json::Value>, D::Error> {
    serde_json::Value::deserialize(deserializer).map(Some)
}

/// A request body after shape validation, which depends on nothing but the
/// body itself — so a 400 here is never an answer about the target.
#[derive(Debug)]
struct ValidatedRequest {
    assertion: Option<String>,
    cap: AuthorizationCap,
    /// The `aud` to mint; Rise's own URL unless the caller named another.
    audience: String,
    ttl_seconds: u64,
}

fn validate_request(
    service: &TokenService,
    body: serde_json::Value,
) -> Result<ValidatedRequest, ServerError> {
    let body: TokenRequest = serde_json::from_value(body)
        .map_err(|e| ServerError::bad_request(format!("invalid request body: {e}")))?;
    if let Some(grant_type) = &body.grant_type {
        if grant_type != GRANT_TYPE_TOKEN_EXCHANGE {
            return Err(ServerError::bad_request("unsupported grant_type"));
        }
    }
    let assertion = match body.subject_token.as_deref().map(str::trim) {
        None => {
            if body.subject_token_type.is_some() {
                return Err(ServerError::bad_request(
                    "subject_token_type requires a subject_token",
                ));
            }
            None
        }
        Some(token) => {
            if body.subject_token_type.as_deref() != Some(TOKEN_TYPE_JWT) {
                return Err(ServerError::bad_request("unsupported subject_token_type"));
            }
            if token.is_empty() {
                return Err(ServerError::bad_request("subject_token must not be empty"));
            }
            if token.len() > MAX_SUBJECT_TOKEN_LEN {
                return Err(ServerError::bad_request("subject_token too large"));
            }
            Some(token.to_owned())
        }
    };
    let cap = match &body.authorization_details {
        None => AuthorizationCap::Unrestricted,
        Some(serde_json::Value::Array(entries)) => AuthorizationCap::from_details(Some(entries))
            .map_err(|error| ServerError::bad_request(error.to_string()))?,
        Some(_) => {
            return Err(ServerError::bad_request(
                "authorization_details must be a non-empty list of rise.dev/rbac entries",
            ))
        }
    };
    let audience = match body.audience.as_deref().map(str::trim) {
        None => service.audience.clone(),
        Some("") => return Err(ServerError::bad_request("audience must not be empty")),
        Some(audience) if audience.len() > MAX_AUDIENCE_LEN => {
            return Err(ServerError::bad_request("audience value too long"))
        }
        Some(audience) => audience.to_owned(),
    };
    // A ceiling is a statement about Rise's own RBAC; on a token another
    // verifier reads it would only look like a restriction.
    if audience != service.audience && !matches!(cap, AuthorizationCap::Unrestricted) {
        return Err(ServerError::bad_request(
            "authorization_details narrows a token for Rise's own API; \
             it cannot be combined with an external audience",
        ));
    }
    let ttl_seconds = match body.expires_in {
        None => service.max_ttl_seconds,
        Some(0) => return Err(ServerError::bad_request("expires_in must be positive")),
        Some(requested) => requested.min(service.max_ttl_seconds),
    };
    Ok(ValidatedRequest {
        assertion,
        cap,
        audience,
        ttl_seconds,
    })
}

/// The identity a token is minted for: the resource, its evaluation target,
/// and the canonical subject the two spell.
struct Target {
    uid: uuid::Uuid,
    name: String,
    kind: TargetKind,
    subject: SubjectId,
}

impl Target {
    fn from_tree(
        uid: uuid::Uuid,
        kind: TargetKind,
        tree: ResourceTree,
    ) -> Result<Self, ServerError> {
        let name = tree.leaf().name.clone();
        let subject = match kind {
            TargetKind::ServiceAccount => {
                let organization = tree.organization().ok_or_else(|| {
                    ServerError::internal("a ServiceAccount is not contained by an Organization")
                })?;
                format!("serviceaccount:{organization}/{name}")
            }
            TargetKind::Controller => format!("controller:{name}"),
        };
        let subject: SubjectId = subject.parse().map_err(|error| {
            ServerError::internal(format!("target is not a valid subject: {error}"))
        })?;
        Ok(Self {
            uid,
            name,
            kind,
            subject,
        })
    }

    fn kind_name(&self) -> &'static str {
        match self.kind {
            TargetKind::ServiceAccount => SERVICE_ACCOUNT_KIND,
            TargetKind::Controller => CONTROLLER_KIND,
        }
    }
}

fn mint(
    service: &TokenService,
    target: &Target,
    request: &ValidatedRequest,
    act: Option<ActorClaim>,
) -> Result<Response, ServerError> {
    let (token, claims) = service
        .signer
        .sign_identity_jwt(IdentityTokenSpec {
            subject: target.subject.as_ref(),
            rise_uid: target.uid,
            audience: &request.audience,
            ttl_secs: request.ttl_seconds,
            authorization_details: request.cap.to_details(),
            act,
        })
        .map_err(|error| {
            tracing::error!("failed to sign identity token: {error:?}");
            ServerError::internal("failed to sign identity token")
        })?;
    Ok(Json(ExchangeResponse {
        access_token: token,
        token_type: "Bearer".to_string(),
        issued_token_type: TOKEN_TYPE_JWT.to_string(),
        expires_in: claims.exp.saturating_sub(claims.iat),
    })
    .into_response())
}

// -----------------------------------------------------------------------------
// Workload token exchange
// -----------------------------------------------------------------------------

/// The unauthenticated POST path of the resource API.
///
/// A request without a Rise credential is a workload token exchange or it is
/// nothing: any path that is not a registered `/token` route answers 401, and
/// a `/token` route on a kind that does not register it answers the ordinary
/// route-not-found, before authentication (ADR-0001 §7).
pub(super) async fn dispatch_unauthenticated(
    store: &Arc<dyn ResourceStore>,
    service: &TokenService,
    raw_path: RawResourcePath,
    body: serde_json::Value,
    client_ip: &str,
) -> Result<Response, ServerError> {
    let not_authenticated = || {
        ServerError::unauthorized("Missing authentication token (cookie or Authorization header)")
    };
    // Body shape first: it depends on nothing about the target, so its 400s
    // reveal nothing, and a request with no assertion carries no credential of
    // any kind.
    let request = validate_request(service, body)?;
    let Some(assertion) = request.assertion.as_deref() else {
        return Err(not_authenticated());
    };
    if let Some(limiter) = &service.rate_limiter {
        if let Err(retry_after) = limiter
            .increment_and_check(client_ip, None, PRE_VALIDATION_BUCKET)
            .await
        {
            return Ok(rate_limit_response(retry_after));
        }
    }

    // Which collections exist is visible to authenticated callers only, so an
    // unresolvable path is answered as the missing credential it is. A store
    // that cannot answer at all is a different matter and is reported as such.
    let (resolved, leaf) = match classify_path(store, raw_path).await {
        Ok(ResolvedPath::Subresource {
            resolved,
            leaf,
            subresource: Subresource::Token,
        }) => (resolved, leaf),
        Err(error) if error.status.is_server_error() => return Err(error),
        Ok(_) | Err(_) => return Err(not_authenticated()),
    };
    let Some(kind) = registered_target(&resolved.info) else {
        return Err(route_not_found());
    };

    workload_exchange(
        store, service, &resolved, &leaf, kind, assertion, &request, client_ip,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn workload_exchange(
    store: &Arc<dyn ResourceStore>,
    service: &TokenService,
    resolved: &ResolvedCollection,
    leaf: &LeafRef,
    kind: TargetKind,
    assertion: &str,
    request: &ValidatedRequest,
    client_ip: &str,
) -> Result<Response, ServerError> {
    // From here on every refusal is the same 401 (ADR-0001 §7, scenario 45).
    // The cause is logged for the operator; the caller learns only that the
    // assertion was not accepted for the route it chose.
    let rejected = |reason: &str| {
        tracing::warn!(
            target: "rise::audit",
            kind = %resolved.info.kind,
            "resource.token_exchange_rejected: {reason}"
        );
        ServerError::unauthorized(WORKLOAD_EXCHANGE_REJECTED)
    };

    let Some(issuer) = peek_issuer(assertion) else {
        return Err(rejected("assertion is not a well-formed JWT"));
    };
    if is_rise_issued_jwt(&issuer, service.rise_issuer()) {
        return Err(rejected(
            "assertion is Rise-issued; only external assertions are exchanged",
        ));
    }
    let Ok(issuer) = Issuer::new(&issuer) else {
        return Err(rejected("assertion issuer is not a canonical issuer URL"));
    };

    // Target resolution is part of authentication on this route: a target that
    // is absent, of another kind, or draining fails the same way as a bad
    // assertion.
    let row = match resolve_leaf(store, resolved, leaf).await {
        Ok(row) => row,
        Err(error) => {
            return Err(rejected(&format!(
                "target did not resolve: {}",
                error.message
            )))
        }
    };
    let chain = store
        .ancestors(row.uid)
        .await
        .map_err(store_error_to_server_error)?;
    if chain.iter().any(|node| node.deletion_timestamp.is_some()) {
        return Err(rejected("target or an ancestor is being deleted"));
    }
    let tree = ResourceTree::from_rows(&chain)
        .map_err(|error| ServerError::internal(format!("target ancestry is invalid: {error}")))?;
    let target = Target::from_tree(row.uid, kind, tree)?;

    // Only trust-policy children of this exact target are consulted, narrowed
    // by issuer before any signature work; no other identity's policies exist
    // as far as this route is concerned.
    let policies = trust_policies_for(service, &target, &issuer).await?;
    if policies.is_empty() {
        return Err(rejected(
            "no trust policy of the target accepts this issuer",
        ));
    }
    if let Some(limiter) = &service.rate_limiter {
        // Keyed on the issuer, which a trust policy has now vouched for.
        if let Err(retry_after) = limiter
            .increment_and_check(client_ip, None, issuer.as_str())
            .await
        {
            return Ok(rate_limit_response(retry_after));
        }
    }
    let verified =
        match rise_backend_auth::verify_external_jwt(assertion, issuer.as_str(), &*service.jwks)
            .await
        {
            Ok(verified) => verified,
            Err(error) => {
                return Err(rejected(&format!("assertion did not verify: {error:?}")));
            }
        };
    let claims = verified.claims();

    let matching: Vec<&TrustPolicyFact<TrustPolicyClaims>> = policies
        .iter()
        .filter(|policy| claims_match(claims, &policy.spec))
        .collect();
    let policy = match matching.as_slice() {
        [policy] => *policy,
        [] => {
            return Err(rejected(
                "assertion claims match no trust policy of the target",
            ))
        }
        _ => {
            return Err(rejected(
                "assertion claims match more than one trust policy",
            ))
        }
    };

    let response = mint(service, &target, request, None)?;
    tracing::info!(
        target: "rise::audit",
        subject = %target.subject,
        uid = %target.uid,
        kind = %target.kind_name(),
        name = %target.name,
        trust_policy = %policy.name,
        source_iss = %issuer,
        source_sub = claims.get("sub").and_then(|v| v.as_str()).unwrap_or(""),
        audience = %request.audience,
        restricted = !matches!(request.cap, AuthorizationCap::Unrestricted),
        "resource.token_exchanged"
    );
    Ok(response)
}

/// The target's live trust policies for one issuer, as one claim-matcher
/// shape regardless of the target kind.
async fn trust_policies_for(
    service: &TokenService,
    target: &Target,
    issuer: &Issuer,
) -> Result<Vec<TrustPolicyFact<TrustPolicyClaims>>, ServerError> {
    let policies = match target.kind {
        TargetKind::ServiceAccount => service
            .trust_policies
            .for_service_account(target.uid, issuer)
            .await
            .map_err(store_error_to_server_error)?
            .into_iter()
            .map(|fact| TrustPolicyFact {
                uid: fact.uid,
                name: fact.name,
                parent_uid: fact.parent_uid,
                spec: fact.spec.claims,
            })
            .collect(),
        TargetKind::Controller => service
            .trust_policies
            .for_controller(target.uid, issuer)
            .await
            .map_err(store_error_to_server_error)?
            .into_iter()
            .map(|fact| TrustPolicyFact {
                uid: fact.uid,
                name: fact.name,
                parent_uid: fact.parent_uid,
                spec: fact.spec.claims,
            })
            .collect(),
    };
    Ok(policies)
}

/// Whether verified claims satisfy one trust policy's closed matcher: `aud`
/// accepts a string or an array (RFC 7519 §4.1.3), every other constraint is a
/// string claim with glob `*` support.
fn claims_match(claims: &serde_json::Value, policy: &TrustPolicyClaims) -> bool {
    let Some(expected_aud) = policy.get("aud") else {
        return false;
    };
    if !audience_matches(
        claims.get("aud").unwrap_or(&serde_json::Value::Null),
        expected_aud,
    ) {
        return false;
    }
    let expected: HashMap<String, String> = policy
        .iter()
        .filter(|(key, _)| *key != "aud")
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
    validate_custom_claims(claims, &expected).is_ok()
}

/// Peek the (unvalidated) `iss` claim of a JWT without verifying its signature.
fn peek_issuer(token: &str) -> Option<String> {
    let payload_b64 = token.split('.').nth(1)?;
    let payload = BASE64URL.decode(payload_b64).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    value.get("iss")?.as_str().map(str::to_owned)
}

// -----------------------------------------------------------------------------
// Delegated issuance
// -----------------------------------------------------------------------------

/// The authenticated `/token` path.
///
/// The caller must hold `(create, <kind>, token)` on the exact target under
/// its own capped policy (ADR-0001 §7, scenarios 46 and 57). Target trust
/// policies play no part, and an external assertion in the body is refused
/// outright: the two modes never mix (scenario 47).
pub(super) async fn delegated_issuance(
    authz: &ResourceAuthorizer,
    store: &Arc<dyn ResourceStore>,
    service: &TokenService,
    auth: &AnyAuth,
    resolved: &ResolvedCollection,
    leaf: &LeafRef,
    body: serde_json::Value,
) -> Result<Response, ServerError> {
    let request = validate_request(service, body)?;
    if request.assertion.is_some() {
        return Err(ServerError::bad_request(
            "a request carries either a subject_token or a Rise credential, never both",
        ));
    }
    let Some(kind) = registered_target(&resolved.info) else {
        return Err(route_not_found());
    };

    let authz = authz.read_context(auth).await?;
    let row = resolve_leaf(store, resolved, leaf).await?;
    let target_tree = authz
        .tree(row.uid)
        .await
        .map_err(|error| match error.status {
            StatusCode::NOT_FOUND | StatusCode::BAD_REQUEST => {
                ServerError::not_found(RESOURCE_NOT_FOUND)
            }
            _ => error,
        })?;
    let token_subresource = Subresource::Token.keyword().parse().map_err(|error| {
        ServerError::internal(format!("subresource keyword is not a valid name: {error}"))
    })?;
    authz
        .require_visible(&target_tree, Verb::Create, Some(&token_subresource))
        .await?;
    // A draining identity is no longer a token target: the token would fail
    // authentication on its first use, so it is refused at issuance instead.
    if row.deletion_timestamp.is_some() {
        return Err(ServerError::not_found(RESOURCE_NOT_FOUND));
    }
    let target = Target::from_tree(row.uid, kind, target_tree)?;

    // The chain records every delegator, this caller outermost. Its length is
    // bounded so the audit provenance a token carries is always complete.
    let (act, caller_jti) = match auth {
        AnyAuth::User(AuthContext::Identity(identity)) => (
            ActorClaim {
                sub: identity.subject.to_string(),
                rise_uid: identity.uid,
                act: identity.act.clone().map(Box::new),
            },
            Some(identity.jti.as_str()),
        ),
        AnyAuth::Controller(controller) => (
            ActorClaim {
                sub: format!("controller:{}", controller.0.name),
                rise_uid: controller.0.uid,
                act: None,
            },
            None,
        ),
        AnyAuth::User(other) => {
            // The choke point already refused a session naming no User
            // resource, so this is the principal the grant was checked for.
            let user = other.user_principal().ok_or_else(|| {
                ServerError::unauthorized("This session does not name a Rise User")
            })?;
            (
                ActorClaim {
                    sub: user.subject().to_string(),
                    rise_uid: user.uid,
                    act: None,
                },
                None,
            )
        }
    };
    if act.depth() > MAX_DELEGATION_DEPTH {
        return Err(ServerError::forbidden(format!(
            "delegation chain would exceed the platform limit of {MAX_DELEGATION_DEPTH}"
        )));
    }

    let response = mint(service, &target, &request, Some(act))?;
    tracing::info!(
        target: "rise::audit",
        actor = %authz.actor(),
        actor_subject = %authz.subject(),
        actor_jti = caller_jti,
        subject = %target.subject,
        uid = %target.uid,
        kind = %target.kind_name(),
        name = %target.name,
        audience = %request.audience,
        restricted = !matches!(request.cap, AuthorizationCap::Unrestricted),
        "resource.token_delegated"
    );
    Ok(response)
}

/// Whether a raw resource path could address a `token` subresource. Purely
/// syntactic: the authenticated POST path uses it to decide whether to
/// classify before entering the create loop.
pub(super) fn may_be_token_route(raw_path: &RawResourcePath) -> bool {
    match raw_path {
        RawResourcePath::Collection { segments, .. } => {
            segments.last().map(String::as_str) == Some(Subresource::Token.keyword())
        }
        RawResourcePath::PendingDeletion | RawResourcePath::PolicyAudit => false,
    }
}

/// Refuse a trust policy that names Rise's own issuer as its external source
/// (ADR-0001 §7, scenario 47). Called on every trust-policy create; the spec
/// is immutable afterwards.
pub(super) fn reject_rise_issuer_in_trust_policy(
    service: &TokenService,
    info: &CollectionInfo,
    spec: &serde_json::Value,
) -> Result<(), ServerError> {
    let is_trust_policy = info.api_version.split('/').next() == Some(API_GROUP)
        && matches!(
            info.kind.as_str(),
            rise_resource_api::CONTROLLER_TRUST_POLICY_KIND
                | rise_resource_api::SERVICE_ACCOUNT_TRUST_POLICY_KIND
        );
    if !is_trust_policy {
        return Ok(());
    }
    let issuer = spec.get("issuer").and_then(|value| value.as_str());
    if issuer.is_some_and(|issuer| is_rise_issued_jwt(issuer, service.rise_issuer())) {
        return Err(ServerError::bad_request(
            "a trust policy may not name Rise's own issuer as an external source",
        ));
    }
    Ok(())
}

#[cfg(test)]
impl TokenService {
    /// The signer, so a test can decode what the route minted.
    pub(super) fn signer(&self) -> &RiseTokenSigner {
        &self.signer
    }
}

/// Test support: a signer for `https://rise.test`, a static JWKS for one
/// external issuer, and assertions signed by that issuer.
#[cfg(test)]
pub(super) mod tests {
    use std::sync::OnceLock;

    use base64::engine::general_purpose::STANDARD as BASE64;
    use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header};
    use rise_backend_auth::AuthError;

    use super::*;

    /// The external issuer the static JWKS answers for.
    pub const ISSUER: &str = "https://ci.example.com";
    /// Rise's own public URL: the issuer and audience of every minted token.
    pub const RISE_URL: &str = "https://rise.test";
    const KID: &str = "test-kid";

    /// One RSA key pair for the whole test binary: generating one per test
    /// would dominate the suite's runtime.
    fn issuer_keys() -> &'static (EncodingKey, DecodingKey) {
        static KEYS: OnceLock<(EncodingKey, DecodingKey)> = OnceLock::new();
        KEYS.get_or_init(|| {
            use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey};
            let private = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048)
                .expect("generate issuer key");
            let public = rsa::RsaPublicKey::from(&private);
            let private_pem = private
                .to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
                .expect("encode private key");
            let public_pem = public
                .to_public_key_pem(rsa::pkcs8::LineEnding::LF)
                .expect("encode public key");
            (
                EncodingKey::from_rsa_pem(private_pem.as_bytes()).expect("encoding key"),
                DecodingKey::from_rsa_pem(public_pem.as_bytes()).expect("decoding key"),
            )
        })
    }

    struct StaticJwks;

    #[async_trait::async_trait]
    impl JwksKeySource for StaticJwks {
        async fn decoding_keys(
            &self,
            issuer: &str,
        ) -> Result<HashMap<String, DecodingKey>, AuthError> {
            if issuer != ISSUER {
                return Err(AuthError::Jwks {
                    issuer: issuer.to_owned(),
                    detail: "no JWKS for this issuer".to_owned(),
                });
            }
            Ok(HashMap::from([(KID.to_owned(), issuer_keys().1.clone())]))
        }
    }

    pub fn signer() -> Arc<RiseTokenSigner> {
        Arc::new(
            RiseTokenSigner::new(
                &BASE64.encode([7u8; 32]),
                RISE_URL.to_owned(),
                3600,
                vec!["sub".into(), "email".into()],
                None,
                None,
            )
            .expect("test signer"),
        )
    }

    /// A service over the test database with a 600-second platform maximum
    /// and no rate limiting.
    pub fn service(pool: sqlx::PgPool) -> TokenService {
        TokenService::new(
            signer(),
            Arc::new(StaticJwks),
            TrustPolicyLookup::new(pool),
            RISE_URL.to_owned(),
            600,
            None,
        )
    }

    /// An assertion from [`ISSUER`] carrying `claims`, with `iss`, `iat`, and
    /// `exp` filled in unless the caller set them.
    pub fn assertion(claims: serde_json::Value) -> String {
        assertion_from(ISSUER, claims)
    }

    /// An assertion claiming to come from `issuer`, signed with the test
    /// issuer's key. The JWKS only vouches for [`ISSUER`], so any other value
    /// fails verification — which is what a test naming one wants to prove.
    pub fn assertion_from(issuer: &str, mut claims: serde_json::Value) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        let object = claims.as_object_mut().expect("claims are an object");
        object
            .entry("iss")
            .or_insert_with(|| serde_json::Value::String(issuer.to_owned()));
        object.entry("iat").or_insert_with(|| now.into());
        object.entry("exp").or_insert_with(|| (now + 300).into());
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(KID.to_owned());
        jsonwebtoken::encode(&header, &claims, &issuer_keys().0).expect("sign assertion")
    }

    #[test]
    fn claims_match_treats_aud_as_string_or_array_and_the_rest_as_string_globs() {
        let policy = TrustPolicyClaims::new(
            [
                ("aud".to_owned(), "rise".to_owned()),
                ("sub".to_owned(), "repo:acme/*".to_owned()),
            ]
            .into_iter()
            .collect(),
        )
        .unwrap();
        assert!(claims_match(
            &serde_json::json!({ "aud": "rise", "sub": "repo:acme/app" }),
            &policy
        ));
        assert!(claims_match(
            &serde_json::json!({ "aud": ["other", "rise"], "sub": "repo:acme/app" }),
            &policy
        ));
        assert!(!claims_match(
            &serde_json::json!({ "aud": "other", "sub": "repo:acme/app" }),
            &policy
        ));
        assert!(!claims_match(
            &serde_json::json!({ "aud": "rise", "sub": "repo:beta/app" }),
            &policy
        ));
        assert!(!claims_match(
            &serde_json::json!({ "aud": "rise" }),
            &policy
        ));
    }

    /// ADR-0001 scenario 52
    /// ADR-0001 scenario 54
    ///
    /// Body validation depends on nothing about the target: a malformed
    /// `authorization_details` set fails closed regardless, and `expires_in`
    /// is clamped to the platform maximum here, at the route, before any
    /// target is resolved.
    #[tokio::test]
    async fn validate_request_checks_only_the_body() {
        let service = TokenService::new(
            signer(),
            Arc::new(StaticJwks),
            TrustPolicyLookup::new(sqlx::PgPool::connect_lazy("postgres://unused").unwrap()),
            RISE_URL.to_owned(),
            600,
            None,
        );
        let ok = validate_request(&service, serde_json::json!({})).unwrap();
        assert!(ok.assertion.is_none());
        assert_eq!(ok.cap, AuthorizationCap::Unrestricted);
        assert_eq!(ok.ttl_seconds, 600);

        let clamped =
            validate_request(&service, serde_json::json!({ "expires_in": 99999 })).unwrap();
        assert_eq!(clamped.ttl_seconds, 600);
        let shorter = validate_request(&service, serde_json::json!({ "expires_in": 30 })).unwrap();
        assert_eq!(shorter.ttl_seconds, 30);

        let rejected = [
            serde_json::json!({ "expires_in": 0 }),
            serde_json::json!({ "grant_type": "password" }),
            serde_json::json!({ "subject_token": "a.b.c" }),
            serde_json::json!({ "subject_token": "a.b.c", "subject_token_type": "urn:ietf:params:oauth:token-type:access_token" }),
            serde_json::json!({ "subject_token": "  ", "subject_token_type": TOKEN_TYPE_JWT }),
            serde_json::json!({ "subject_token_type": TOKEN_TYPE_JWT }),
            serde_json::json!({ "authorization_details": [] }),
            serde_json::json!({ "authorization_details": null }),
            serde_json::json!({ "authorization_details": {} }),
            serde_json::json!({ "authorization_details": [{ "type": "other", "scope": "*", "permissions": [{ "verbs": "*", "kinds": "*" }] }] }),
            serde_json::json!({ "unknown": true }),
            serde_json::json!({ "audience": "" }),
            serde_json::json!({ "audience": "x".repeat(MAX_AUDIENCE_LEN + 1) }),
            serde_json::json!({
                "audience": "https://vault.example.com",
                "authorization_details": [{ "type": "rise.dev/rbac", "scope": "*", "permissions": [{ "verbs": "*", "kinds": "*" }] }]
            }),
        ];
        for body in rejected {
            let err = validate_request(&service, body.clone()).unwrap_err();
            assert_eq!(
                err.status,
                StatusCode::BAD_REQUEST,
                "{body}: {}",
                err.message
            );
        }

        let narrowed = validate_request(
            &service,
            serde_json::json!({
                "grant_type": GRANT_TYPE_TOKEN_EXCHANGE,
                "subject_token": "a.b.c",
                "subject_token_type": TOKEN_TYPE_JWT,
                "authorization_details": [{
                    "type": "rise.dev/rbac",
                    "scope": "rise.dev/Organization/acme",
                    "permissions": [{ "verbs": ["get"], "kinds": ["rise.dev/ServiceAccount"] }]
                }]
            }),
        )
        .unwrap();
        assert_eq!(narrowed.assertion.as_deref(), Some("a.b.c"));
        assert_eq!(narrowed.audience, RISE_URL);

        let external = validate_request(
            &service,
            serde_json::json!({ "audience": " https://vault.example.com " }),
        )
        .unwrap();
        assert_eq!(external.audience, "https://vault.example.com");
        assert_eq!(external.cap, AuthorizationCap::Unrestricted);
        assert!(
            matches!(narrowed.cap, AuthorizationCap::Restricted(ref entries) if entries.len() == 1)
        );
    }
}
