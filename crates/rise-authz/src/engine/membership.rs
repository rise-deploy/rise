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
}
