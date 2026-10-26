//! Authentication of Rise identity tokens as resource principals (ADR-0001 §7).
//!
//! An identity token names a ServiceAccount or Controller *resource* by
//! canonical subject and UID. Verifying its signature only proves Rise minted
//! it; it becomes a principal when `(sub, rise_uid)` still identifies one live
//! resource. That check runs on every request, which is what makes deleting the
//! target — or recreating the same name under a new UID — fail every token
//! already issued for it, with nothing to revoke.

use rise_authz::engine::AuthorizationCap;
use rise_backend_auth::{ActorClaim, IdentityClaims};
use rise_resource_api::{
    ResourceStore, SubjectId, API_GROUP, CONTROLLER_KIND, ORGANIZATION_KIND, SERVICE_ACCOUNT_KIND,
};
use uuid::Uuid;

/// A ServiceAccount or Controller resource that authenticated with an identity
/// token, resolved against the live store for this request.
#[derive(Clone, Debug)]
pub struct ResourcePrincipal {
    /// The canonical subject (`serviceaccount:<org>/<name>` or
    /// `controller:<name>`).
    pub subject: SubjectId,
    /// The live resource's UID, equal to the credential's `rise_uid`.
    pub uid: Uuid,
    /// The credential's signed Allow ceiling.
    pub cap: AuthorizationCap,
    /// The delegation chain the credential records, for audit and for the next
    /// hop of delegated issuance.
    pub act: Option<ActorClaim>,
    /// The credential's audit id.
    pub jti: String,
}

/// Why an identity token did not resolve to a principal.
///
/// Every variant is reported to the caller as the same 401: which of these
/// applied is a fact about a resource the caller may hold nothing on.
#[derive(Debug)]
pub enum IdentityRejection {
    /// The token was minted for another verifier, not for Rise's API.
    Audience(String),
    /// The subject is malformed, or names a kind that cannot hold a token.
    Subject(String),
    /// No live resource matches both the subject and the UID.
    NoLiveResource,
    /// `authorization_details` is present but not a valid ceiling.
    Cap(String),
    /// The store could not answer.
    Store(rise_resource_api::StoreError),
}

impl std::fmt::Display for IdentityRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Audience(audience) => write!(f, "token is for audience {audience:?}"),
            Self::Subject(detail) => write!(f, "invalid subject: {detail}"),
            Self::NoLiveResource => f.write_str("no live resource matches the subject and UID"),
            Self::Cap(detail) => write!(f, "invalid authorization_details: {detail}"),
            Self::Store(error) => write!(f, "store error: {error:?}"),
        }
    }
}

/// The resource kind an identity subject must resolve to.
pub(crate) fn expected_kind(subject: &SubjectId) -> Option<&'static str> {
    match subject.kind() {
        "serviceaccount" => Some(SERVICE_ACCOUNT_KIND),
        "controller" => Some(CONTROLLER_KIND),
        _ => None,
    }
}

/// Resolve verified identity claims to a live resource principal.
///
/// Requires, in order: an `aud` equal to `expected_audience` (Rise's own
/// public URL — a token minted for another verifier is never a principal
/// here, however valid its signature); a canonical ServiceAccount or
/// Controller subject; a live (not tombstoned) resource at `rise_uid` whose
/// group, kind, and name match the subject, contained — for a ServiceAccount —
/// by a live Organization of the subject's org name; and a well-formed
/// `authorization_details` claim, if one is present. The subject is compared
/// to the resource *by name*, so the UID is what binds the credential to one
/// incarnation of that name.
pub async fn resolve_identity(
    store: &dyn ResourceStore,
    claims: &IdentityClaims,
    expected_audience: &str,
) -> Result<ResourcePrincipal, IdentityRejection> {
    if claims.aud != expected_audience {
        return Err(IdentityRejection::Audience(claims.aud.clone()));
    }
    let subject: SubjectId = claims
        .sub
        .parse()
        .map_err(|error| IdentityRejection::Subject(format!("{error}")))?;
    let kind = expected_kind(&subject).ok_or_else(|| {
        IdentityRejection::Subject(format!(
            "{subject} is not a ServiceAccount or Controller subject"
        ))
    })?;

    let chain = store
        .ancestors(claims.rise_uid)
        .await
        .map_err(IdentityRejection::Store)?;
    let Some(leaf) = chain.last() else {
        return Err(IdentityRejection::NoLiveResource);
    };
    // A draining ancestor drains the identity with it: the Organization's
    // policy resources are tombstoned alongside it, so a ServiceAccount under
    // one must not keep authenticating into what remains of the subtree.
    if chain.iter().any(|row| row.deletion_timestamp.is_some()) {
        return Err(IdentityRejection::NoLiveResource);
    }
    let group = leaf.api_version.split('/').next().unwrap_or_default();
    if leaf.uid != claims.rise_uid
        || group != API_GROUP
        || leaf.kind != kind
        || leaf.name != subject.name()
    {
        return Err(IdentityRejection::NoLiveResource);
    }
    match subject.organization() {
        // A ServiceAccount lives directly under its Organization (ADR-0001 §1).
        Some(organization) => {
            let contained = chain.len() == 2
                && chain[0].kind == ORGANIZATION_KIND
                && chain[0].api_version.split('/').next().unwrap_or_default() == API_GROUP
                && chain[0].name == organization;
            if !contained {
                return Err(IdentityRejection::NoLiveResource);
            }
        }
        // A Controller is root-scoped.
        None => {
            if chain.len() != 1 {
                return Err(IdentityRejection::NoLiveResource);
            }
        }
    }

    let cap = AuthorizationCap::from_details(claims.authorization_details.as_deref())
        .map_err(|error| IdentityRejection::Cap(error.to_string()))?;

    Ok(ResourcePrincipal {
        subject,
        uid: leaf.uid,
        cap,
        act: claims.act.clone(),
        jti: claims.jti.clone(),
    })
}
