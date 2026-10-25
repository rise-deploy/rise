use std::collections::HashMap;
use std::fmt;

use rise_resource_api::{
    LocallyNormalizedPlatformRoleBindingSpec, LocallyNormalizedRoleBindingSpec, PolicyStatement,
    ResourceRow, ResourceStore, RoleRefKind, RoleSpec, SubjectMembership, API_VERSION_V1ALPHA1,
    ORGANIZATION_KIND, PLATFORM_ROLE_BINDING_KIND, PLATFORM_ROLE_KIND, ROLE_BINDING_KIND,
    ROLE_KIND,
};
use uuid::Uuid;

use crate::engine::AuthorizationError;
use crate::policy::BindingTier;

/// Which policy kind a binding is, and therefore which placement tier it
/// carries. This is the binding's own kind, never its `roleRef`'s: a Deny
/// reached through an org `RoleBinding` is org policy even when the Role it
/// names is a `PlatformRole` (ADR-0001 §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingKind {
    RoleBinding,
    PlatformRoleBinding,
}

impl fmt::Display for BindingKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::RoleBinding => ROLE_BINDING_KIND,
            Self::PlatformRoleBinding => PLATFORM_ROLE_BINDING_KIND,
        })
    }
}

/// The Role a binding names, retained for explain output and for the structural
/// org-admin predicate, which reads the reference and never the Role's body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleReference {
    pub kind: RoleRefKind,
    pub name: String,
}

/// Enough of a binding's identity to explain or audit its contribution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingProvenance {
    pub uid: Uuid,
    pub kind: BindingKind,
    pub role: RoleReference,
    /// The binding's own resource name, so an audit finding can identify it
    /// without a second lookup. `None` for a binding the grant gate simulates
    /// from a request rather than loads from a stored row.
    pub name: Option<String>,
}

/// One live binding with its `roleRef` already resolved to policy statements.
///
/// A dangling reference resolves to no statements rather than an error: ADR-0001
/// deletes a Role without deleting its bindings, exactly as it deletes a User
/// without deleting the memberships naming it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingFact {
    pub provenance: BindingProvenance,
    pub tier: BindingTier,
    pub subject: rise_resource_api::BindingSubject,
    pub subject_membership: SubjectMembership,
    pub scope: rise_resource_api::Scope,
    pub selector: Option<rise_resource_api::LabelSelector>,
    pub statements: Vec<PolicyStatement>,
    /// Whether `roleRef` resolved to a live Role/PlatformRole body.
    ///
    /// `false` always pairs with empty `statements` — evaluation treats a
    /// dangling reference as contributing nothing either way — but the audit
    /// needs to tell a dangling reference apart from a resolved Role whose
    /// body happens to hold zero statements, which is `true` with the same
    /// empty list.
    pub role_resolved: bool,
}

/// Root-parented `PlatformRoleBinding`s, which are the complete platform tier.
pub(crate) async fn load_platform_bindings(
    store: &dyn ResourceStore,
) -> Result<Vec<BindingFact>, AuthorizationError> {
    let rows = store
        .list(API_VERSION_V1ALPHA1, PLATFORM_ROLE_BINDING_KIND, None)
        .await?;
    let mut roles = RoleCache::default();
    let mut bindings = Vec::new();
    for row in rows.iter().filter(|row| is_live(row)) {
        let spec: LocallyNormalizedPlatformRoleBindingSpec = parse_spec(row)?;
        let resolved = roles
            .resolve(
                store,
                RoleRefKind::PlatformRole,
                spec.role_ref().name.as_str(),
                None,
            )
            .await?;
        let role_resolved = resolved.is_some();
        bindings.push(BindingFact {
            provenance: BindingProvenance {
                uid: row.uid,
                kind: BindingKind::PlatformRoleBinding,
                role: RoleReference {
                    kind: RoleRefKind::PlatformRole,
                    name: spec.role_ref().name.clone(),
                },
                name: Some(row.name.clone()),
            },
            tier: BindingTier::Platform,
            subject: spec.subject().clone(),
            subject_membership: spec.subject_membership(),
            scope: spec.scope().clone(),
            selector: spec.label_selector().cloned(),
            statements: resolved.unwrap_or_default(),
            role_resolved,
        });
    }
    Ok(bindings)
}

/// Org `RoleBinding`s parented under one Organization.
///
/// Write-time containment keeps an org binding's scope inside its own parent's
/// subtree, so a resource in that org can only be reached by bindings placed
/// there. Reading this one collection is therefore the complete org tier for
/// that resource, and needs no scope index.
pub(crate) async fn load_organization_bindings(
    store: &dyn ResourceStore,
    organization: &str,
) -> Result<Vec<BindingFact>, AuthorizationError> {
    let Some(org_row) = store
        .get_by_name(API_VERSION_V1ALPHA1, ORGANIZATION_KIND, organization, None)
        .await?
        .filter(is_live)
    else {
        return Ok(Vec::new());
    };
    let rows = store
        .list(API_VERSION_V1ALPHA1, ROLE_BINDING_KIND, Some(org_row.uid))
        .await?;
    let mut roles = RoleCache::default();
    let mut bindings = Vec::new();
    for row in rows.iter().filter(|row| is_live(row)) {
        let spec: LocallyNormalizedRoleBindingSpec = parse_spec(row)?;
        let role_ref = spec.role_ref();
        // Reference direction (§4): an org binding reaches its own org's Roles
        // or any PlatformRole. A bare name never falls back across that line.
        let role_parent = match role_ref.kind {
            RoleRefKind::Role => Some(org_row.uid),
            RoleRefKind::PlatformRole => None,
        };
        let resolved = roles
            .resolve(store, role_ref.kind, role_ref.name.as_str(), role_parent)
            .await?;
        let role_resolved = resolved.is_some();
        bindings.push(BindingFact {
            provenance: BindingProvenance {
                uid: row.uid,
                kind: BindingKind::RoleBinding,
                role: RoleReference {
                    kind: role_ref.kind,
                    name: role_ref.name.clone(),
                },
                name: Some(row.name.clone()),
            },
            tier: BindingTier::Organization(organization.to_owned()),
            subject: spec.subject().clone(),
            // An org binding's recipient boundary is structural, so the field
            // does not exist on the kind; `Any` adds no second constraint.
            subject_membership: SubjectMembership::Any,
            scope: spec.scope().clone(),
            selector: spec.label_selector().cloned(),
            statements: resolved.unwrap_or_default(),
            role_resolved,
        });
    }
    Ok(bindings)
}

/// A cached resolution: `None` for a dangling reference, `Some` for a resolved
/// Role's statements (possibly empty).
type ResolvedRole = Option<Vec<PolicyStatement>>;

/// Role bodies resolved during one load.
///
/// Keyed by placement as well as name: Role names are unique per Organization,
/// not globally, so dropping the parent from the key would let one org's Role
/// answer for another's the moment this cache outlives a single org's load.
#[derive(Default)]
struct RoleCache {
    statements: HashMap<(&'static str, Option<Uuid>, String), ResolvedRole>,
}

impl RoleCache {
    /// Resolve a `roleRef` to its live statements.
    ///
    /// `None` marks a dangling reference — no live Role/PlatformRole at that
    /// placement — distinctly from a resolved Role whose body happens to have
    /// zero statements, which is `Some(vec![])`. Evaluation folds both into
    /// "contributes nothing"; the audit needs to tell them apart.
    async fn resolve(
        &mut self,
        store: &dyn ResourceStore,
        kind: RoleRefKind,
        name: &str,
        parent_uid: Option<Uuid>,
    ) -> Result<ResolvedRole, AuthorizationError> {
        let stored_kind = match kind {
            RoleRefKind::Role => ROLE_KIND,
            RoleRefKind::PlatformRole => PLATFORM_ROLE_KIND,
        };
        let key = (stored_kind, parent_uid, name.to_owned());
        if let Some(resolved) = self.statements.get(&key) {
            return Ok(resolved.clone());
        }
        let resolved = match store
            .get_by_name(API_VERSION_V1ALPHA1, stored_kind, name, parent_uid)
            .await?
            .filter(is_live)
        {
            Some(row) => Some(parse_spec::<RoleSpec>(&row)?.statements),
            None => None,
        };
        self.statements.insert(key, resolved.clone());
        Ok(resolved)
    }
}

pub(crate) fn is_live(row: &ResourceRow) -> bool {
    row.deletion_timestamp.is_none()
}

/// Read a stored `Role`/`PlatformRole` body's statements.
///
/// Shared with the grant gate, which resolves a `roleRef` outside binding
/// collection but must interpret the row identically — a second parse with
/// different failure behaviour is how a Deny gets silently dropped.
pub(crate) fn parse_role_statements(
    row: &ResourceRow,
) -> Result<Vec<PolicyStatement>, AuthorizationError> {
    Ok(parse_spec::<RoleSpec>(row)?.statements)
}

/// Stored policy that no longer parses is an error, never a skipped row.
///
/// Skipping would silently drop the row's `Deny` statements, turning corrupt
/// data into a privilege gain; failing the request keeps it a visible outage.
fn parse_spec<T: serde::de::DeserializeOwned>(row: &ResourceRow) -> Result<T, AuthorizationError> {
    serde_json::from_value(row.spec.clone()).map_err(|error| {
        AuthorizationError::corrupt_policy(format!(
            "stored {} '{}' ({}) is not valid policy: {error}",
            row.kind, row.name, row.uid
        ))
    })
}
