//! Rise's implementation of the engine's one product-specific seam.
//!
//! ADR-0001's Implementation structure leaves exactly one fact outside the
//! evaluation engine: which Groups a principal currently belongs to, and whether
//! they are an operator. Everything else evaluation needs is an ordinary
//! `ResourceStore` read.
//!
//! Both answers are live. Group ties come from `GroupMembership` resources —
//! read through the same session as the caller's request, so an
//! authorization-changing write sees the memberships its own transaction sees
//! (ADR-0001 §5). Operator status is the intersection of live identity facts
//! with process configuration, which the engine deliberately knows nothing
//! about.
//!
//! **Transitional operator derivation.** ADR-0001 §1 defines an operator as an
//! active User with a live, active `UserIdentity` matching the restart-loaded
//! `operatorIdentities` selector set. Those identity resources are not yet
//! written by any login path, so this resolver derives the same status from the
//! configuration that governs it today: the `auth.operator_users` email
//! allowlist and the `auth.operator_idp_groups` IdP groups, resolved through
//! `auth::roles`. The seam is what matters — the engine asks one question and
//! gets one live answer — and the derivation moves to `UserIdentity` when
//! identity resources go live, with no change above this module.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use rise_authz::engine::{
    AuthenticatedPrincipal, AuthorizationError, MembershipResolver, PrincipalMembership,
};
use rise_resource_api::{ResourceStore, SubjectId, API_VERSION_V1ALPHA1, USER_KIND};
use rise_resource_store_postgres::{MembershipLookup, PgSession};

use crate::db::models::User;

/// The configured operator selectors, as this install expresses them today.
#[derive(Clone)]
pub struct OperatorSelectors {
    pub users: Arc<Vec<String>>,
    pub idp_groups: Arc<Vec<String>>,
}

/// Resolves live Group ties and operator standing for one request.
pub struct RiseMembershipResolver {
    session: PgSession,
    store: Arc<dyn ResourceStore>,
    memberships: MembershipLookup,
    operators: OperatorSelectors,
    /// The authenticated User this resolver answers for. Authentication already
    /// resolved the credential to this row, and the principal carries its UID,
    /// so the resolver reads the *live* facts — Group ties and IdP groups —
    /// rather than re-resolving who the caller is.
    user: User,
}

impl RiseMembershipResolver {
    /// `store` and `session` must address the same unit of work: on a write
    /// path both are the transaction's, so every membership fact the gate reads
    /// is one the mutation commits against.
    pub fn new(
        session: PgSession,
        store: Arc<dyn ResourceStore>,
        operators: OperatorSelectors,
        user: User,
    ) -> Self {
        Self {
            memberships: MembershipLookup::in_session(session.clone()),
            session,
            store,
            operators,
            user,
        }
    }

    /// Live `group:<org>/<name>` ties of the User named by `subject`.
    ///
    /// The lookup is UID- *and* name-bound: a `GroupMembership` names its User
    /// (ADR-0001 §1), and the User resource must still carry that name under
    /// that UID. A name-bound marker left behind by a deleted User therefore
    /// confers nothing until a User of that name exists again, which is the
    /// reactivation §1 describes rather than a live tie.
    async fn group_ties(
        &self,
        subject: &SubjectId,
        subject_uid: uuid::Uuid,
    ) -> Result<BTreeSet<SubjectId>, AuthorizationError> {
        let facts = self
            .memberships
            .groups_for_user(subject_uid, subject.name())
            .await?;
        facts
            .iter()
            .map(|fact| {
                format!("group:{}/{}", fact.organization_name, fact.group_name)
                    .parse::<SubjectId>()
                    .map_err(|error| {
                        // Both halves are stored resource names, so this is a
                        // corrupt row rather than bad input — and a tie the
                        // engine cannot express is not one to silently drop.
                        AuthorizationError::Membership(format!(
                            "GroupMembership {} resolves to an unparseable subject: {error}",
                            fact.membership_uid
                        ))
                    })
            })
            .collect()
    }

    /// Whether the User behind this principal currently holds operator standing.
    ///
    /// The group half reads through the request's own session, so a write
    /// transaction measures the writer against the same facts it commits
    /// against: revoking an operator's IdP group concurrently with their write
    /// forces one of the two transactions to retry rather than letting the write
    /// commit on a stale answer.
    async fn is_operator(&self) -> Result<bool, AuthorizationError> {
        let mut connection = self.session.acquire().await?;
        Ok(crate::server::auth::roles::has_role(
            &mut *connection,
            &self.operators.users,
            &self.operators.idp_groups,
            &self.user,
        )
        .await)
    }

    /// The UID of the live root `User` resource with this canonical name.
    async fn lookup_user_resource(
        &self,
        name: &str,
    ) -> Result<Option<uuid::Uuid>, AuthorizationError> {
        Ok(self
            .store
            .get_by_name(API_VERSION_V1ALPHA1, USER_KIND, name, None)
            .await?
            .filter(|row| row.deletion_timestamp.is_none())
            .map(|row| row.uid))
    }
}

#[async_trait]
impl MembershipResolver for RiseMembershipResolver {
    async fn resolve(
        &self,
        principal: &AuthenticatedPrincipal,
    ) -> Result<PrincipalMembership, AuthorizationError> {
        // Group ties and operator standing belong to Users alone (ADR-0001 §1);
        // the engine rejects a resolver that claims either for a workload
        // identity, so answering empty here is the contract, not a shortcut.
        if !principal.is_user() {
            return Ok(PrincipalMembership::default());
        }
        // The resolver is built for one request's principal; answering for
        // another would attribute one caller's ties to another.
        if principal.subject_uid() != self.user.id {
            return Err(AuthorizationError::Membership(format!(
                "resolver holds user {} but was asked about {}",
                self.user.id,
                principal.subject_uid()
            )));
        }
        Ok(PrincipalMembership {
            groups: self
                .group_ties(principal.subject(), principal.subject_uid())
                .await?,
            is_operator: self.is_operator().await?,
        })
    }

    async fn groups_for_user(
        &self,
        user: &SubjectId,
    ) -> Result<BTreeSet<SubjectId>, AuthorizationError> {
        if user.kind() != "user" {
            return Err(AuthorizationError::InvalidInput(format!(
                "{user} is not a User subject"
            )));
        }
        // The lookup is UID-bound and this caller has only a name, so resolve
        // the User resource first. A name with no live User resource has no
        // ties — the mapping's parent identity is whatever the name means once
        // such a resource exists.
        let Some(row) = self
            .lookup_user_resource(user.name())
            .await?
            .map(|uid| (uid, user.clone()))
        else {
            return Ok(BTreeSet::new());
        };
        self.group_ties(&row.1, row.0).await
    }
}
