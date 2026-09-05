use rise_resource_api::{
    ControllerTrustPolicySpec, ExternalSubject, Issuer, ServiceAccountTrustPolicySpec, StoreError,
    UserIdentitySpec, UserSpec,
};
use sqlx::PgPool;
use uuid::Uuid;

use crate::session::PgSession;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserIdentityFact {
    pub identity_uid: Uuid,
    pub identity_name: String,
    pub identity: UserIdentitySpec,
    pub user_uid: Uuid,
    pub user_name: String,
    pub user: UserSpec,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustPolicyFact<T> {
    pub uid: Uuid,
    pub name: String,
    pub parent_uid: Uuid,
    pub spec: T,
}

/// One controller candidate matching a token's issuer: a live
/// `ControllerTrustPolicy` and its live root `Controller` parent.
///
/// Distinct from [`TrustPolicyFact`] because a candidate search is keyed by
/// issuer alone (the controller identity is what authentication is trying to
/// resolve), while `for_controller` is target-bound and already knows the
/// Controller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControllerCandidate {
    pub controller_uid: Uuid,
    pub controller_name: String,
    pub policy_uid: Uuid,
    pub policy_name: String,
    pub spec: ControllerTrustPolicySpec,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupMembershipFact {
    pub membership_uid: Uuid,
    pub membership_name: String,
    pub group_uid: Uuid,
    pub group_name: String,
    pub organization_uid: Uuid,
    /// The owning Organization's canonical name, which is what a
    /// `group:<org>/<name>` subject carries.
    pub organization_name: String,
}

/// Exposed so tests can assert the query planner uses the identity projection
/// index for the statement that actually runs, not a hand-copied lookalike.
#[doc(hidden)]
pub const USER_IDENTITY_BY_EXTERNAL_IDENTITY_SQL: &str = r#"
SELECT identity.uid AS identity_uid,
       identity.name AS identity_name,
       identity.spec AS identity_spec,
       parent.uid AS user_uid,
       parent.name AS user_name,
       parent.spec AS user_spec
FROM resource_store.resources identity
JOIN resource_store.resources parent
  ON parent.uid = identity.parent_uid
 AND parent.api_version = 'rise.dev/v1alpha1'
 AND parent.kind = 'User'
 AND parent.parent_uid IS NULL
 AND parent.deletion_timestamp IS NULL
WHERE identity.api_version = 'rise.dev/v1alpha1'
  AND split_part(identity.api_version, '/', 1) = 'rise.dev'
  AND identity.kind = 'UserIdentity'
  AND identity.deletion_timestamp IS NULL
  AND (identity.spec->>'issuer') COLLATE "C" = $1 COLLATE "C"
  AND (identity.spec->>'subject') COLLATE "C" = $2 COLLATE "C"
LIMIT 2
"#;

#[doc(hidden)]
pub const CONTROLLER_TRUST_POLICIES_SQL: &str = r#"
SELECT policy.uid, policy.name, policy.parent_uid, policy.spec
FROM resource_store.resources policy
JOIN resource_store.resources target
  ON target.uid = $1
 AND target.api_version = 'rise.dev/v1alpha1'
 AND target.kind = 'Controller'
 AND target.parent_uid IS NULL
 AND target.deletion_timestamp IS NULL
WHERE policy.api_version = 'rise.dev/v1alpha1'
  AND split_part(policy.api_version, '/', 1) = 'rise.dev'
  AND policy.kind = 'ControllerTrustPolicy'
  AND policy.deletion_timestamp IS NULL
  AND policy.parent_uid = target.uid
  AND (policy.spec->>'issuer') COLLATE "C" = $2 COLLATE "C"
ORDER BY policy.name, policy.uid
"#;

/// Every live `ControllerTrustPolicy` matching a token's issuer, joined to its
/// live root `Controller` parent. Unlike [`CONTROLLER_TRUST_POLICIES_SQL`] this
/// is not target-bound: authentication does not yet know which Controller the
/// token belongs to, which is exactly what this resolves.
///
/// The predicate on `policy` must match the partial index
/// `controller_trust_policies_issuer` byte-for-byte (the `split_part` check
/// included, even though `api_version` is also compared as a literal) for the
/// planner to use it.
#[doc(hidden)]
pub const CONTROLLER_CANDIDATES_BY_ISSUER_SQL: &str = r#"
SELECT controller.uid AS controller_uid,
       controller.name AS controller_name,
       policy.uid AS policy_uid,
       policy.name AS policy_name,
       policy.spec AS policy_spec
FROM resource_store.resources policy
JOIN resource_store.resources controller
  ON controller.uid = policy.parent_uid
 AND controller.api_version = 'rise.dev/v1alpha1'
 AND controller.kind = 'Controller'
 AND controller.parent_uid IS NULL
 AND controller.deletion_timestamp IS NULL
WHERE policy.api_version = 'rise.dev/v1alpha1'
  AND split_part(policy.api_version, '/', 1) = 'rise.dev'
  AND policy.kind = 'ControllerTrustPolicy'
  AND policy.deletion_timestamp IS NULL
  AND (policy.spec->>'issuer') COLLATE "C" = $1 COLLATE "C"
ORDER BY controller.name, controller.uid, policy.name, policy.uid
"#;

/// Whether any live `ControllerTrustPolicy` under a live Controller declares
/// this issuer — the cheap existence check the auth middleware uses to decide
/// whether an unrecognized issuer is worth a JWKS fetch at all.
#[doc(hidden)]
pub const CONTROLLER_ISSUER_EXISTS_SQL: &str = r#"
SELECT EXISTS (
    SELECT 1
    FROM resource_store.resources policy
    JOIN resource_store.resources controller
      ON controller.uid = policy.parent_uid
     AND controller.api_version = 'rise.dev/v1alpha1'
     AND controller.kind = 'Controller'
     AND controller.parent_uid IS NULL
     AND controller.deletion_timestamp IS NULL
    WHERE policy.api_version = 'rise.dev/v1alpha1'
      AND split_part(policy.api_version, '/', 1) = 'rise.dev'
      AND policy.kind = 'ControllerTrustPolicy'
      AND policy.deletion_timestamp IS NULL
      AND (policy.spec->>'issuer') COLLATE "C" = $1 COLLATE "C"
)
"#;

#[doc(hidden)]
pub const SERVICE_ACCOUNT_TRUST_POLICIES_SQL: &str = r#"
SELECT policy.uid, policy.name, policy.parent_uid, policy.spec
FROM resource_store.resources policy
JOIN resource_store.resources target
  ON target.uid = $1
 AND target.api_version = 'rise.dev/v1alpha1'
 AND target.kind = 'ServiceAccount'
 AND target.deletion_timestamp IS NULL
JOIN resource_store.resources organization
  ON organization.uid = target.parent_uid
 AND organization.api_version = 'rise.dev/v1alpha1'
 AND organization.kind = 'Organization'
 AND organization.parent_uid IS NULL
 AND organization.deletion_timestamp IS NULL
WHERE policy.api_version = 'rise.dev/v1alpha1'
  AND split_part(policy.api_version, '/', 1) = 'rise.dev'
  AND policy.kind = 'ServiceAccountTrustPolicy'
  AND policy.deletion_timestamp IS NULL
  AND policy.parent_uid = target.uid
  AND (policy.spec->>'issuer') COLLATE "C" = $2 COLLATE "C"
ORDER BY policy.name, policy.uid
"#;

#[doc(hidden)]
pub const GROUPS_FOR_USER_SQL: &str = r#"
SELECT membership.uid AS membership_uid,
       membership.name AS membership_name,
       parent.uid AS group_uid,
       parent.name AS group_name,
       organization.uid AS organization_uid,
       organization.name AS organization_name
FROM resource_store.resources membership
JOIN resource_store.resources member
  ON member.uid = $1
 AND member.api_version = 'rise.dev/v1alpha1'
 AND member.kind = 'User'
 AND member.parent_uid IS NULL
 AND member.name = $2
 AND member.deletion_timestamp IS NULL
JOIN resource_store.resources parent
  ON parent.uid = membership.parent_uid
 AND parent.api_version = 'rise.dev/v1alpha1'
 AND parent.kind = 'Group'
 AND parent.deletion_timestamp IS NULL
JOIN resource_store.resources organization
  ON organization.uid = parent.parent_uid
 AND organization.api_version = 'rise.dev/v1alpha1'
 AND organization.kind = 'Organization'
 AND organization.parent_uid IS NULL
 AND organization.deletion_timestamp IS NULL
WHERE membership.api_version = 'rise.dev/v1alpha1'
  AND split_part(membership.api_version, '/', 1) = 'rise.dev'
  AND membership.kind = 'GroupMembership'
  AND membership.deletion_timestamp IS NULL
  AND membership.name COLLATE "C" = member.name COLLATE "C"
ORDER BY parent.name, parent.uid, membership.uid
"#;

#[derive(Clone)]
pub struct IdentityLookup {
    pool: PgPool,
}

impl IdentityLookup {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Resolve a live mapping, including inactive mappings, and its live User.
    /// Callers decide whether the two typed `active` flags permit login.
    pub async fn by_external_identity(
        &self,
        issuer: &Issuer,
        subject: &ExternalSubject,
    ) -> Result<Option<UserIdentityFact>, StoreError> {
        // At most one row can match while `user_identities_issuer_subject_unique`
        // holds. Fetch two and refuse to guess if the invariant is ever broken:
        // this decides who a login belongs to, so returning an arbitrary row
        // would be worse than failing.
        let rows = sqlx::query_as::<_, IdentityFactRow>(USER_IDENTITY_BY_EXTERNAL_IDENTITY_SQL)
            .bind(issuer.as_str())
            .bind(subject.as_str())
            .fetch_all(&self.pool)
            .await
            .map_err(StoreError::backend)?;
        if rows.len() > 1 {
            return Err(StoreError::backend(std::io::Error::other(
                "external identity resolves to more than one live UserIdentity",
            )));
        }
        rows.into_iter().next().map(TryInto::try_into).transpose()
    }
}

#[derive(Clone)]
pub struct TrustPolicyLookup {
    pool: PgPool,
}

impl TrustPolicyLookup {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn for_controller(
        &self,
        controller_uid: Uuid,
        issuer: &Issuer,
    ) -> Result<Vec<TrustPolicyFact<ControllerTrustPolicySpec>>, StoreError> {
        let rows = sqlx::query_as::<_, TrustFactRow>(CONTROLLER_TRUST_POLICIES_SQL)
            .bind(controller_uid)
            .bind(issuer.as_str())
            .fetch_all(&self.pool)
            .await
            .map_err(StoreError::backend)?;
        parse_trust_rows(rows)
    }

    pub async fn for_service_account(
        &self,
        service_account_uid: Uuid,
        issuer: &Issuer,
    ) -> Result<Vec<TrustPolicyFact<ServiceAccountTrustPolicySpec>>, StoreError> {
        let rows = sqlx::query_as::<_, TrustFactRow>(SERVICE_ACCOUNT_TRUST_POLICIES_SQL)
            .bind(service_account_uid)
            .bind(issuer.as_str())
            .fetch_all(&self.pool)
            .await
            .map_err(StoreError::backend)?;
        parse_trust_rows(rows)
    }

    /// Every live Controller candidate for a token's issuer, keyed only by
    /// issuer since authentication does not yet know the Controller's uid.
    pub async fn controller_candidates_by_issuer(
        &self,
        issuer: &Issuer,
    ) -> Result<Vec<ControllerCandidate>, StoreError> {
        let rows = sqlx::query_as::<_, ControllerCandidateRow>(CONTROLLER_CANDIDATES_BY_ISSUER_SQL)
            .bind(issuer.as_str())
            .fetch_all(&self.pool)
            .await
            .map_err(StoreError::backend)?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    /// Whether any live `ControllerTrustPolicy` declares this issuer, without
    /// fetching or parsing the candidates themselves.
    pub async fn controller_issuer_exists(&self, issuer: &Issuer) -> Result<bool, StoreError> {
        sqlx::query_scalar(CONTROLLER_ISSUER_EXISTS_SQL)
            .bind(issuer.as_str())
            .fetch_one(&self.pool)
            .await
            .map_err(StoreError::backend)
    }
}

#[derive(Clone)]
pub struct MembershipLookup {
    db: PgSession,
}

impl MembershipLookup {
    pub fn new(pool: PgPool) -> Self {
        Self {
            db: PgSession::pool(pool),
        }
    }

    /// The same lookup, reading through `session`.
    ///
    /// ADR-0001 §5 requires an authorization-changing write to re-read the
    /// relevant memberships inside its own transaction; this is how the
    /// membership seam joins it.
    pub fn in_session(session: PgSession) -> Self {
        Self { db: session }
    }

    pub async fn groups_for_user(
        &self,
        user_uid: Uuid,
        user_name: &str,
    ) -> Result<Vec<GroupMembershipFact>, StoreError> {
        let mut connection = self.db.acquire().await?;
        let rows = sqlx::query_as::<_, MembershipFactRow>(GROUPS_FOR_USER_SQL)
            .bind(user_uid)
            .bind(user_name)
            .fetch_all(&mut *connection)
            .await
            .map_err(crate::session::map_sqlx_error)?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// The ties bound to a User name, independent of whether a live, active
    /// `User` resource carries it. See [`GROUP_TIES_BY_USER_NAME_SQL`].
    pub async fn group_ties_by_user_name(
        &self,
        user_name: &str,
    ) -> Result<Vec<GroupMembershipFact>, StoreError> {
        let mut connection = self.db.acquire().await?;
        let rows = sqlx::query_as::<_, MembershipFactRow>(GROUP_TIES_BY_USER_NAME_SQL)
            .bind(user_name)
            .fetch_all(&mut *connection)
            .await
            .map_err(crate::session::map_sqlx_error)?;
        Ok(rows.into_iter().map(Into::into).collect())
    }
}

/// The Group ties bound to a User *name*, whether or not a live, active `User`
/// resource currently carries it.
///
/// A `GroupMembership` is a name-bound marker (ADR-0001 §1): deleting the User
/// leaves it in place, and recreating or reactivating the name makes it deliver
/// again. `GROUPS_FOR_USER_SQL` answers "what does this User reach *now*", which
/// is right for a caller's own snapshot. This answers "what would this name
/// reach once it is live", which is the question the grant gate has to ask about
/// a write that is bringing the name into existence or switching it on — at that
/// moment the row does not exist or is still inactive, and the first query
/// necessarily returns nothing.
#[doc(hidden)]
pub const GROUP_TIES_BY_USER_NAME_SQL: &str = r#"
SELECT membership.uid AS membership_uid,
       membership.name AS membership_name,
       parent.uid AS group_uid,
       parent.name AS group_name,
       organization.uid AS organization_uid,
       organization.name AS organization_name
FROM resource_store.resources membership
JOIN resource_store.resources parent
  ON parent.uid = membership.parent_uid
 AND parent.api_version = 'rise.dev/v1alpha1'
 AND parent.kind = 'Group'
 AND parent.deletion_timestamp IS NULL
JOIN resource_store.resources organization
  ON organization.uid = parent.parent_uid
 AND organization.api_version = 'rise.dev/v1alpha1'
 AND organization.kind = 'Organization'
 AND organization.parent_uid IS NULL
 AND organization.deletion_timestamp IS NULL
WHERE membership.api_version = 'rise.dev/v1alpha1'
  AND split_part(membership.api_version, '/', 1) = 'rise.dev'
  AND membership.kind = 'GroupMembership'
  AND membership.deletion_timestamp IS NULL
  AND membership.name COLLATE "C" = $1 COLLATE "C"
ORDER BY parent.name, parent.uid, membership.uid
"#;

#[derive(sqlx::FromRow)]
struct IdentityFactRow {
    identity_uid: Uuid,
    identity_name: String,
    identity_spec: serde_json::Value,
    user_uid: Uuid,
    user_name: String,
    user_spec: serde_json::Value,
}

impl TryFrom<IdentityFactRow> for UserIdentityFact {
    type Error = StoreError;

    fn try_from(row: IdentityFactRow) -> Result<Self, Self::Error> {
        Ok(Self {
            identity_uid: row.identity_uid,
            identity_name: row.identity_name,
            identity: parse_spec(row.identity_spec, "UserIdentity")?,
            user_uid: row.user_uid,
            user_name: row.user_name,
            user: parse_spec(row.user_spec, "User")?,
        })
    }
}

#[derive(sqlx::FromRow)]
struct TrustFactRow {
    uid: Uuid,
    name: String,
    parent_uid: Uuid,
    spec: serde_json::Value,
}

#[derive(sqlx::FromRow)]
struct ControllerCandidateRow {
    controller_uid: Uuid,
    controller_name: String,
    policy_uid: Uuid,
    policy_name: String,
    policy_spec: serde_json::Value,
}

impl TryFrom<ControllerCandidateRow> for ControllerCandidate {
    type Error = StoreError;

    fn try_from(row: ControllerCandidateRow) -> Result<Self, Self::Error> {
        Ok(Self {
            controller_uid: row.controller_uid,
            controller_name: row.controller_name,
            policy_uid: row.policy_uid,
            policy_name: row.policy_name,
            spec: parse_spec(row.policy_spec, "ControllerTrustPolicy")?,
        })
    }
}

fn parse_trust_rows<T>(rows: Vec<TrustFactRow>) -> Result<Vec<TrustPolicyFact<T>>, StoreError>
where
    T: serde::de::DeserializeOwned,
{
    rows.into_iter()
        .map(|row| {
            Ok(TrustPolicyFact {
                uid: row.uid,
                name: row.name,
                parent_uid: row.parent_uid,
                spec: parse_spec(row.spec, "trust policy")?,
            })
        })
        .collect()
}

#[derive(sqlx::FromRow)]
struct MembershipFactRow {
    membership_uid: Uuid,
    membership_name: String,
    group_uid: Uuid,
    group_name: String,
    organization_uid: Uuid,
    organization_name: String,
}

impl From<MembershipFactRow> for GroupMembershipFact {
    fn from(row: MembershipFactRow) -> Self {
        Self {
            membership_uid: row.membership_uid,
            membership_name: row.membership_name,
            group_uid: row.group_uid,
            group_name: row.group_name,
            organization_uid: row.organization_uid,
            organization_name: row.organization_name,
        }
    }
}

fn parse_spec<T: serde::de::DeserializeOwned>(
    value: serde_json::Value,
    kind: &str,
) -> Result<T, StoreError> {
    serde_json::from_value(value)
        .map_err(|error| StoreError::Validation(format!("stored {kind} spec is invalid: {error}")))
}
