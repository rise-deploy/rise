use rise_resource_api::{
    Effect, KindMatcher, PolicyStatement, Scope, SubjectId, SubresourceMatcher, VerbMatcher,
};
use uuid::Uuid;

use crate::engine::{AuthorizationError, ResourceTree};

/// One `rise.dev/rbac` authorization-detail permission (ADR-0001 §7).
///
/// The grammar is a Role statement's without an effect: a ceiling only ever
/// removes authority, so every entry is implicitly an Allow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapPermission {
    pub verbs: VerbMatcher,
    pub kinds: KindMatcher,
    /// Omission covers the main resource only, exactly as in a Role statement.
    pub subresources: Option<SubresourceMatcher>,
}

impl CapPermission {
    fn to_statement(&self) -> PolicyStatement {
        PolicyStatement {
            effect: Effect::Allow,
            kinds: self.kinds.clone(),
            verbs: self.verbs.clone(),
            subresources: self.subresources.clone(),
        }
    }
}

/// One authorization-detail entry: a single qualified Scope and the permissions
/// the token retains within it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapEntry {
    pub scope: Scope,
    pub permissions: Vec<CapPermission>,
}

/// The signed Allow ceiling a credential carries.
///
/// A token narrows what its identity may do; it never grants. Absent
/// authorization details mean the full live policy of the target identity,
/// while a present-but-empty detail set admits nothing rather than falling back
/// to full access.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorizationCap {
    Unrestricted,
    Restricted(Vec<CapEntry>),
}

impl AuthorizationCap {
    /// The ceiling that applies to one resource: the union of every entry whose
    /// Scope covers it. `None` means unrestricted.
    pub(crate) fn ceiling_for(&self, target: &ResourceTree) -> Option<Vec<PolicyStatement>> {
        let Self::Restricted(entries) = self else {
            return None;
        };
        Some(
            entries
                .iter()
                .filter(|entry| target.covered_by(&entry.scope))
                .flat_map(|entry| entry.permissions.iter().map(CapPermission::to_statement))
                .collect(),
        )
    }

    /// The ceiling that applies across a whole policy domain, for the grant
    /// gate's before/after comparison.
    ///
    /// An entry counts only when its own Scope covers the domain outright. One
    /// that covers part of it does not license the writer throughout, and the
    /// gate compares authority over domains rather than over the resources that
    /// happen to exist — so a partial entry is dropped, leaving a restricted
    /// credential with no covering entry holding nothing there.
    pub(crate) fn statements_covering(
        &self,
        scope: &Scope,
        scope_covers: &dyn Fn(&Scope, &Scope) -> bool,
    ) -> Option<Vec<PolicyStatement>> {
        let Self::Restricted(entries) = self else {
            return None;
        };
        Some(
            entries
                .iter()
                .filter(|entry| {
                    entry.scope.is_wildcard()
                        || (!scope.is_wildcard() && scope_covers(&entry.scope, scope))
                })
                .flat_map(|entry| entry.permissions.iter().map(CapPermission::to_statement))
                .collect(),
        )
    }
}

/// The only identity the engine accepts.
///
/// Authentication resolves a credential to exactly one live, active identity
/// resource before this type exists; no claim, header, or raw string crosses
/// into evaluation. Group and virtual subjects are never principals, so
/// construction rejects them rather than leaving the evaluator to notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedPrincipal {
    subject: SubjectId,
    subject_uid: Uuid,
    authorization_cap: AuthorizationCap,
}

impl AuthenticatedPrincipal {
    pub fn new(
        subject: SubjectId,
        subject_uid: Uuid,
        authorization_cap: AuthorizationCap,
    ) -> Result<Self, AuthorizationError> {
        if subject.is_virtual() || subject.kind() == "group" {
            return Err(AuthorizationError::invalid_principal(format!(
                "{subject} cannot authenticate: only User, ServiceAccount, and Controller identities are principals"
            )));
        }
        Ok(Self {
            subject,
            subject_uid,
            authorization_cap,
        })
    }

    pub fn subject(&self) -> &SubjectId {
        &self.subject
    }

    pub fn subject_uid(&self) -> Uuid {
        self.subject_uid
    }

    pub fn authorization_cap(&self) -> &AuthorizationCap {
        &self.authorization_cap
    }

    /// Whether this principal is a User, the only kind that can hold Group ties
    /// or org-admin standing.
    pub fn is_user(&self) -> bool {
        self.subject.kind() == "user"
    }
}
