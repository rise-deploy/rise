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
//! **Group policy is dark until identity resources exist.** A *principal's* own
//! ties resolve through a live, active `User` resource of their name, and no
//! login path writes one yet, so every principal currently has an empty tie
//! set. That direction is safe for an Allow — a group binding simply grants
//! nothing — but it is *not* safe for a Deny: a cap expressed as a
//! group-targeted Deny is never collected, so it does not bite. Until identity
//! resolution lands, a restriction has to be expressed against a subject that
//! resolves today (`system:authenticated`, `org:<name>`, or the principal
//! itself) to actually be a restriction.
//!
//! The gate's question is the exception, and deliberately so: `groups_for_user`
//! resolves by *name* and does not require the row to exist or be active, since
//! every write that asks it is one that is about to bring the name into
//! existence or switch it on. So a `GroupMembership` marker written today is
//! already weighed when someone tries to activate the name it points at, even
//! though it grants that name nothing yet.
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
use rise_resource_api::{ResourceStore, SubjectId, UserSpec, API_VERSION_V1ALPHA1, USER_KIND};
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

    /// Live `group:<org>/<name>` ties of the User this subject names.
    ///
    /// The lookup is UID- *and* name-bound: a `GroupMembership` names its User
    /// (ADR-0001 §1), and the User resource must still carry that name under
    /// that UID. A name-bound marker left behind by a deleted User therefore
    /// confers nothing until a User of that name exists again, which is the
    /// reactivation §1 describes rather than a live tie.
    ///
    /// The UID is the *resource's*, resolved from the canonical name — never the
    /// credential's `rise_uid`. Those are the same value only once identity
    /// resolution assigns a principal its User resource's UID; passing the
    /// credential's would make a tie resolvable only when two independently
    /// generated identifiers happened to coincide, which is to say never.
    async fn group_ties(
        &self,
        subject: &SubjectId,
    ) -> Result<BTreeSet<SubjectId>, AuthorizationError> {
        let Some(user_uid) = self.lookup_user_resource(subject.name()).await? else {
            return Ok(BTreeSet::new());
        };
        let facts = self
            .memberships
            .groups_for_user(user_uid, subject.name())
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
    /// The group half reads through the request's own session, so the answer is
    /// consistent with everything else that transaction reads.
    ///
    /// It is *not* serialized against a concurrent revocation. PostgreSQL
    /// checks a serializable transaction's predicate reads only against writers
    /// that are themselves serializable, and every writer of `team_members` —
    /// the team API, the
    /// login sync, the Entra sync — runs at `READ COMMITTED`. So a revocation
    /// committing alongside this read neither aborts the transaction nor is
    /// seen by it, and a just-revoked operator lands one more write. That is
    /// ordinary revocation latency rather than an escalation, and closing it
    /// would take an advisory lock on the user id here and in every
    /// `team_members` writer — the same remedy, and the same reasoning, as the
    /// `TODO(multi-org)` in `resources/organization.rs`.
    async fn is_operator(&self) -> Result<bool, AuthorizationError> {
        let mut connection = self.session.acquire().await?;
        // The checked form: inside the write path this runs on the
        // transaction's own connection, where a lost race is an instruction to
        // replay rather than a reason to answer "not an operator" from a
        // transaction that is already doomed.
        crate::server::auth::roles::has_role_checked(
            &mut *connection,
            &self.operators.users,
            &self.operators.idp_groups,
            &self.user,
        )
        .await
        .map_err(|error| {
            match error
                .downcast_ref::<sqlx::Error>()
                .is_some_and(rise_resource_store_postgres::is_serialization_failure)
            {
                true => AuthorizationError::Store(rise_resource_api::StoreError::Serialization),
                false => AuthorizationError::Membership(format!(
                    "failed to resolve operator standing: {error:?}"
                )),
            }
        })
    }

    /// The UID of the live, active root `User` resource with this canonical
    /// name.
    ///
    /// `spec.active` is part of the answer, not a detail: ADR-0001 §1 makes an
    /// inactive User unable to log in and fails every token already issued for
    /// them. A tie that still resolved through one would be authority reachable
    /// by an identity the platform has switched off — and it is the premise the
    /// activation gate rests on, so the two have to agree.
    async fn lookup_user_resource(
        &self,
        name: &str,
    ) -> Result<Option<uuid::Uuid>, AuthorizationError> {
        let Some(row) = self
            .store
            .get_by_name(API_VERSION_V1ALPHA1, USER_KIND, name, None)
            .await?
            .filter(|row| row.deletion_timestamp.is_none())
        else {
            return Ok(None);
        };
        // A stored User whose spec will not parse is corrupt policy data, not an
        // active identity: fail closed rather than assume the default.
        let spec: UserSpec = serde_json::from_value(row.spec.clone()).map_err(|error| {
            AuthorizationError::CorruptPolicy(format!(
                "stored User '{}' ({}) is not valid: {error}",
                row.name, row.uid
            ))
        })?;
        Ok(spec.active.then_some(row.uid))
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
            groups: self.group_ties(principal.subject()).await?,
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
        // Resolved by *name*, not through a live, active `User` row — unlike
        // `group_ties`, which answers for a caller's own snapshot and must stay
        // strict.
        //
        // The gate asks this about a write that makes a credential path to this
        // name work: a new identity mapping, an activation, a create. At the
        // moment it asks, the row is absent or still inactive by construction,
        // so a lookup that required one would answer "no ties" for precisely
        // the writes whose whole effect is to make those ties deliver again. A
        // `GroupMembership` is a name-bound marker (ADR-0001 §1) that outlives
        // the User; what the write reaches is what the name reaches once live.
        //
        // For a mapping onto an already-live User the two forms agree, and
        // where they differ this one is the larger set — which is the direction
        // this seam is documented to fail in.
        let facts = self
            .memberships
            .group_ties_by_user_name(user.name())
            .await?;
        facts
            .iter()
            .map(|fact| {
                format!("group:{}/{}", fact.organization_name, fact.group_name)
                    .parse::<SubjectId>()
                    .map_err(|error| {
                        AuthorizationError::CorruptPolicy(format!(
                            "stored Group tie is not a valid subject: {error}"
                        ))
                    })
            })
            .collect()
    }
}
