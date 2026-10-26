use rise_resource_api::{
    Effect, KindMatcher, PolicyStatement, Scope, SubjectId, SubresourceMatcher, VerbMatcher,
};
use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;

use crate::engine::{AuthorizationError, ResourceTree};

/// The RFC 9396 `type` of every Rise authorization-detail entry (ADR-0001 §7).
pub const RBAC_DETAIL_TYPE: &str = "rise.dev/rbac";

/// One `rise.dev/rbac` authorization-detail permission (ADR-0001 §7).
///
/// The grammar is a Role statement's without an effect: a ceiling only ever
/// removes authority, so every entry is implicitly an Allow. The serialized
/// form is the wire shape of one `permissions` element; the matchers reject an
/// empty axis, a non-`*` wildcard, and duplicates exactly as a Role does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CapPermission {
    pub verbs: VerbMatcher,
    pub kinds: KindMatcher,
    /// Omission covers the main resource only, exactly as in a Role statement.
    /// An explicit `null` is rejected rather than read as omission.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub subresources: Option<SubresourceMatcher>,
}

fn deserialize_optional_non_null<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

/// The wire shape of one `authorization_details` entry.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthorizationDetail {
    #[serde(rename = "type")]
    detail_type: String,
    scope: Scope,
    permissions: Vec<CapPermission>,
}

impl TryFrom<AuthorizationDetail> for CapEntry {
    type Error = String;

    fn try_from(detail: AuthorizationDetail) -> Result<Self, Self::Error> {
        if detail.detail_type != RBAC_DETAIL_TYPE {
            return Err(format!(
                "authorization detail type must be '{RBAC_DETAIL_TYPE}', got '{}'",
                detail.detail_type
            ));
        }
        if detail.permissions.is_empty() {
            return Err("authorization detail permissions must not be empty".into());
        }
        Ok(Self {
            scope: detail.scope,
            permissions: detail.permissions,
        })
    }
}

impl From<CapEntry> for AuthorizationDetail {
    fn from(entry: CapEntry) -> Self {
        Self {
            detail_type: RBAC_DETAIL_TYPE.to_owned(),
            scope: entry.scope,
            permissions: entry.permissions,
        }
    }
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
///
/// Serializes as an RFC 9396 entry of type [`RBAC_DETAIL_TYPE`]; deserializing
/// rejects any other type and an empty permission list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "AuthorizationDetail", into = "AuthorizationDetail")]
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
    /// Parse a credential's `authorization_details` claim, or a `/token`
    /// request's narrowing, into a ceiling (ADR-0001 §7).
    ///
    /// `None` is the omitted claim and means the full live policy. A present
    /// value must be a non-empty array of well-formed `rise.dev/rbac` entries:
    /// an empty array, a non-array, an unknown type, or a malformed entry is an
    /// error rather than a fallback to full access, and the caller treats that
    /// error as an invalid credential.
    pub fn from_details(details: Option<&[serde_json::Value]>) -> Result<Self, AuthorizationError> {
        let Some(details) = details else {
            return Ok(Self::Unrestricted);
        };
        if details.is_empty() {
            return Err(AuthorizationError::invalid_input(
                "authorization_details must not be an empty list",
            ));
        }
        details
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                serde_json::from_value::<CapEntry>(entry.clone()).map_err(|error| {
                    AuthorizationError::invalid_input(format!(
                        "authorization_details[{index}] is invalid: {error}"
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Self::Restricted)
    }

    /// The canonical claim value for this ceiling: `None` for an unrestricted
    /// credential, otherwise the entries in wire form.
    pub fn to_details(&self) -> Option<Vec<serde_json::Value>> {
        match self {
            Self::Unrestricted => None,
            Self::Restricted(entries) => Some(
                entries
                    .iter()
                    .map(|entry| {
                        serde_json::to_value(entry).expect("a CapEntry serializes to JSON")
                    })
                    .collect(),
            ),
        }
    }

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
