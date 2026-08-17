use std::collections::BTreeSet;

use rise_resource_api::SubjectId;

use crate::engine::{AuthenticatedPrincipal, AuthorizationError};

/// The live membership facts the engine cannot derive from policy rows alone.
///
/// Both are live: a removed `GroupMembership` or a deactivated `UserIdentity`
/// changes the next request's answer, with nothing carried in a token.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrincipalMembership {
    /// Canonical `group:<org>/<name>` subjects the principal currently belongs
    /// to. Only Users hold Group ties.
    pub groups: BTreeSet<SubjectId>,
    /// Whether any live, active `UserIdentity` of this User matches the
    /// process's restart-loaded operator selector set.
    pub is_operator: bool,
}

/// The engine's one product-specific seam (ADR-0001, Implementation structure).
///
/// Everything else evaluation needs is an ordinary `ResourceStore` read. This
/// stays a seam because operator status intersects live identity rows with
/// process configuration, which the engine deliberately knows nothing about.
#[async_trait::async_trait]
pub trait MembershipResolver: Send + Sync {
    async fn resolve(
        &self,
        principal: &AuthenticatedPrincipal,
    ) -> Result<PrincipalMembership, AuthorizationError>;

    /// The live Group ties of a User who is not the caller.
    ///
    /// The grant gate needs this for one case: adding or retargeting an identity
    /// mapping makes the *whole* effective policy of the mapped User reachable
    /// through a new credential (ADR-0001 §5), and that policy includes whatever
    /// the User's Groups grant. Aggregating only the bindings that name the User
    /// directly would under-report the delta, which on a gate is the unsafe
    /// direction.
    ///
    /// Everywhere else the gate deliberately avoids expanding a recipient's
    /// membership: over-reporting a delta only blocks writes, so authored-subject
    /// aggregation is both cheaper and fail-closed. This method exists for the
    /// one shape where the arrow points the other way.
    ///
    /// `user` must be a canonical `user:<name>` subject. Implementations return
    /// canonical `group:<org>/<name>` subjects and must not report a tie for an
    /// unknown or inactive User.
    async fn groups_for_user(
        &self,
        user: &SubjectId,
    ) -> Result<BTreeSet<SubjectId>, AuthorizationError>;
}
