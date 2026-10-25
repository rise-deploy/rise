//! Diagnostics over shapes admission accepts but that grant nothing, or
//! nothing durable.
//!
//! Audit never rejects a write — that stays admission's job — and it is
//! deliberately bounded: every entry point takes a [`AuditScope`] naming one
//! Organization (or every live one) and a finding cap, and never scans the
//! whole store. Detectors are ordinary functions, named one per
//! [`FindingCategory`] row, so each can be exercised on its own; the only
//! thing [`AuthorizationEngine::audit`] adds is loading the binding facts
//! once per tier and running them in a fixed order.

use std::collections::BTreeMap;

use rise_resource_api::{
    resource_owner_binding_spec, Effect, LabelKey, LabelSelector, PathSegment, PolicyStatement,
    ResourceRow, ResourceStore, RoleRefKind, Scope, StoreError, SubjectId, SubjectMembership,
    UserSpec, API_GROUP, API_VERSION_V1ALPHA1, CONTROLLER_KIND, GROUP_KIND, GROUP_MEMBERSHIP_KIND,
    MAX_PARENT_CHAIN_DEPTH, ORGANIZATION_KIND, ORG_ADMIN_PLATFORM_ROLE, OWNER_LABEL_KEY,
    SERVICE_ACCOUNT_KIND, USER_KIND,
};
use uuid::Uuid;

use crate::engine::bindings::{
    is_live, load_organization_bindings, load_platform_bindings, BindingFact, BindingKind,
};
use crate::engine::{
    qualifies_as_org_admin, AuthorizationEngine, AuthorizationError, MembershipResolver,
    ResourceTree,
};
use crate::policy::{
    deny_covers_allow, domain_covers, replaces, resolve_subject, subject_covers, ApplicableBinding,
    BindingTier, PolicyDomain,
};

/// What to audit, and how much work to do.
///
/// Every detector is bounded by this: one Organization (or every live one
/// when `organization` is `None`), and a hard cap on findings so a large or
/// unhealthy install cannot make one audit call unbounded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditScope {
    pub organization: Option<String>,
    pub max_findings: usize,
}

/// The result of one [`AuthorizationEngine::audit`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditReport {
    pub findings: Vec<AuditFinding>,
    /// Whether more findings existed than `max_findings` allowed through.
    pub truncated: bool,
    /// What the run actually looked at, so an empty `findings` list reads as
    /// "scanned and healthy" rather than "scanned nothing".
    pub scanned: AuditScanned,
}

/// What one [`AuthorizationEngine::audit`] call read, independent of what it
/// found. A caller with zero findings and an empty `scanned` block skipped
/// the install rather than confirming it healthy — this is what tells the two
/// apart.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AuditScanned {
    /// Every Organization the run loaded bindings for, in scan order.
    pub organizations: Vec<String>,
    /// Live root-parented `PlatformRoleBinding` rows the run loaded.
    pub platform_bindings: usize,
    /// Live `RoleBinding` rows loaded across every scanned Organization.
    pub organization_bindings: usize,
}

/// How urgently a finding deserves attention.
///
/// `Info` marks a shape that is *currently* inert but self-heals if the
/// install's live facts change (a membership marker outliving its User, a
/// binding that will start mattering once someone joins a Group) — nothing an
/// operator broke. `Warning` marks a shape that is permanently inert, or
/// actively misleading, until someone edits it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warning,
}

/// The resource a finding is about, or one it names in `related`.
///
/// `kind` is the resource's own kind name (`RoleBinding`, `Organization`,
/// `GroupMembership`, ...), not a [`rise_resource_api::ResourceKind`]: a
/// finding can be about a root-parented resource with no API group ambiguity
/// to carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindingSubject {
    pub uid: Uuid,
    pub kind: String,
    pub name: String,
    pub organization: Option<String>,
}

/// Which side of a wildcard replacement shadows an Allow.
///
/// Populated by the shadowing detectors added alongside `ShadowedAllow`'s
/// pure predicates; declared now so every category this crate ever emits is
/// visible on [`FindingCategory`] from this change forward.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shadow {
    Deny,
    Replacement,
}

/// One diagnosable shape ADR-0001's admission accepts.
///
/// Every variant here is a row in the audit's detector table; a detector
/// function produces exactly one category.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindingCategory {
    /// `roleRef` names no live Role/PlatformRole.
    DanglingRoleRef,
    /// A literal subject names no live, active identity.
    StaleSubject,
    /// A non-wildcard scope names a resource that no longer exists.
    StaleScope,
    /// A `GroupMembership` names a User with no live, active row.
    StaleMembership,
    /// `subjectMembership: ResourceOrganization` can never bite.
    NoOpMembershipConstraint,
    /// An org `RoleBinding`'s subject can never, or does not currently,
    /// belong to that Organization.
    RecipientBoundaryNoOp,
    /// `rise.dev/owner` resolves to nobody the seeded ownership binding reaches.
    InertOwnerLabel,
    /// A binding's `labelSelector` matches no resource in its scope.
    SelectorMatchesNothing,
    /// A more specific binding drops this Allow, either through a Deny or
    /// through wildcard replacement.
    ShadowedAllow { by: Shadow },
    /// No qualifying binding gives this Organization a live, active admin.
    OrganizationWithoutAdmin,
    /// A `PlatformRole/org-admin` reference that does not satisfy the exact
    /// org-root, scope-only shape ADR-0001 §5 requires to confer standing.
    NonQualifyingOrgAdminReference,
}

/// One diagnosed shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditFinding {
    /// The resource the finding is about: a binding, Role, GroupMembership,
    /// labelled resource, or Organization.
    pub subject: FindingSubject,
    pub category: FindingCategory,
    pub severity: Severity,
    /// Other live rows involved (the shadowing binding, ...). Empty when the
    /// finding names something that does not exist — there is no row to
    /// reference, so `detail` carries that identification instead.
    pub related: Vec<FindingSubject>,
    /// One actionable sentence, naming the missing or related thing by kind
    /// and name.
    pub detail: String,
}

impl AuthorizationEngine {
    /// Run every detector this change carries, over the bindings in scope.
    ///
    /// Findings on tombstoned rows are never produced: platform bindings, org
    /// bindings, Groups, and Organizations are all read live-filtered before a
    /// detector ever sees them. There is no [`crate::engine::AuthorizationSnapshot`]
    /// here — the audit has no principal, so it evaluates nothing and decides
    /// nothing.
    pub async fn audit(&self, scope: &AuditScope) -> Result<AuditReport, AuthorizationError> {
        let store = self.store.as_ref();
        let memberships = self.memberships.as_ref();
        let mut findings = Vec::new();
        let mut truncated = false;
        let mut scanned = AuditScanned::default();

        let platform = load_platform_bindings(store).await?;
        scanned.platform_bindings = platform.len();
        append_capped(
            &mut findings,
            &mut truncated,
            scope.max_findings,
            detect_dangling_role_ref(&platform),
        );
        append_capped(
            &mut findings,
            &mut truncated,
            scope.max_findings,
            detect_stale_subject(store, &platform).await?,
        );
        append_capped(
            &mut findings,
            &mut truncated,
            scope.max_findings,
            detect_stale_scope(store, &platform).await?,
        );
        append_capped(
            &mut findings,
            &mut truncated,
            scope.max_findings,
            detect_no_op_membership_constraint(&platform),
        );

        // Every live `rise.dev/owner` setter, resolved to its own organization
        // once so the per-organization loop below can filter without a second
        // store round trip per row. Root-scoped setters (no organization) are
        // only in scope when the whole install is being audited: a named-org
        // scope can never contain one.
        let owner_setters = owner_label_setters_by_organization(store).await?;
        if scope.organization.is_none() {
            let root_scoped: Vec<ResourceRow> = owner_setters
                .iter()
                .filter(|(_, organization)| organization.is_none())
                .map(|(row, _)| row.clone())
                .collect();
            append_capped(
                &mut findings,
                &mut truncated,
                scope.max_findings,
                detect_inert_owner_label(store, memberships, None, &[], &root_scoped).await?,
            );
        }
        append_capped(
            &mut findings,
            &mut truncated,
            scope.max_findings,
            detect_selector_matches_nothing(store, &platform).await?,
        );
        append_capped(
            &mut findings,
            &mut truncated,
            scope.max_findings,
            detect_shadowed_by_deny(&platform, &platform),
        );
        append_capped(
            &mut findings,
            &mut truncated,
            scope.max_findings,
            detect_shadowed_by_replacement(&platform, &platform),
        );

        append_capped(
            &mut findings,
            &mut truncated,
            scope.max_findings,
            detect_non_qualifying_org_admin_reference(&platform),
        );

        for organization in self.audit_organizations(scope).await? {
            let org_bindings = load_organization_bindings(store, &organization.name).await?;
            scanned.organizations.push(organization.name.clone());
            scanned.organization_bindings += org_bindings.len();
            append_capped(
                &mut findings,
                &mut truncated,
                scope.max_findings,
                detect_dangling_role_ref(&org_bindings),
            );
            append_capped(
                &mut findings,
                &mut truncated,
                scope.max_findings,
                detect_stale_subject(store, &org_bindings).await?,
            );
            append_capped(
                &mut findings,
                &mut truncated,
                scope.max_findings,
                detect_stale_scope(store, &org_bindings).await?,
            );
            let groups = live_groups(store, organization.uid).await?;
            append_capped(
                &mut findings,
                &mut truncated,
                scope.max_findings,
                detect_stale_membership(store, &organization.name, &groups).await?,
            );
            append_capped(
                &mut findings,
                &mut truncated,
                scope.max_findings,
                detect_recipient_boundary_no_op(memberships, &organization.name, &org_bindings)
                    .await?,
            );
            let org_setters: Vec<ResourceRow> = owner_setters
                .iter()
                .filter(|(_, setter_organization)| {
                    setter_organization.as_deref() == Some(organization.name.as_str())
                })
                .map(|(row, _)| row.clone())
                .collect();
            append_capped(
                &mut findings,
                &mut truncated,
                scope.max_findings,
                detect_inert_owner_label(
                    store,
                    memberships,
                    Some(&organization.name),
                    &org_bindings,
                    &org_setters,
                )
                .await?,
            );
            append_capped(
                &mut findings,
                &mut truncated,
                scope.max_findings,
                detect_selector_matches_nothing(store, &org_bindings).await?,
            );
            append_capped(
                &mut findings,
                &mut truncated,
                scope.max_findings,
                detect_shadowed_by_deny(&platform, &org_bindings),
            );
            append_capped(
                &mut findings,
                &mut truncated,
                scope.max_findings,
                detect_shadowed_by_replacement(&platform, &org_bindings),
            );
            append_capped(
                &mut findings,
                &mut truncated,
                scope.max_findings,
                detect_organization_without_admin(store, &organization, &org_bindings).await?,
            );
            append_capped(
                &mut findings,
                &mut truncated,
                scope.max_findings,
                detect_non_qualifying_org_admin_reference(&org_bindings),
            );
        }

        Ok(AuditReport {
            findings,
            truncated,
            scanned,
        })
    }

    /// The live Organizations `scope` names: one, or every live one.
    async fn audit_organizations(
        &self,
        scope: &AuditScope,
    ) -> Result<Vec<ResourceRow>, AuthorizationError> {
        match &scope.organization {
            Some(name) => Ok(self
                .store
                .get_by_name(API_VERSION_V1ALPHA1, ORGANIZATION_KIND, name, None)
                .await?
                .filter(is_live)
                .into_iter()
                .collect()),
            None => Ok(self
                .store
                .list(API_VERSION_V1ALPHA1, ORGANIZATION_KIND, None)
                .await?
                .into_iter()
                .filter(is_live)
                .collect()),
        }
    }

    /// Write-time diagnostics for exactly one binding row (ADR-0001 §5:
    /// "diagnostics never reject writes").
    ///
    /// Runs the row-level detectors (dangling `roleRef`, stale subject, stale
    /// scope, the no-op membership constraint, the recipient-boundary no-op,
    /// an unmatched selector, and a non-qualifying org-admin reference) over
    /// the one tier the binding is placed in — platform for a root-parented
    /// row, that Organization for an org-parented one — never the whole
    /// install, so this is cheap enough to run after every binding write.
    /// `uid` naming a row that no longer exists, or an org-parented row whose
    /// parent has already drained, answers with no findings rather than an
    /// error: there is nothing left to diagnose.
    pub async fn audit_binding(&self, uid: Uuid) -> Result<Vec<AuditFinding>, AuthorizationError> {
        let store = self.store.as_ref();
        let Some(row) = store.get(uid).await? else {
            return Ok(Vec::new());
        };
        let (bindings, organization) = match row.parent_uid {
            None => (load_platform_bindings(store).await?, None),
            Some(parent_uid) => {
                let Some(parent) = store.get(parent_uid).await?.filter(is_live) else {
                    return Ok(Vec::new());
                };
                let org_bindings = load_organization_bindings(store, &parent.name).await?;
                (org_bindings, Some(parent.name))
            }
        };

        let mut findings = Vec::new();
        findings.extend(detect_dangling_role_ref(&bindings));
        findings.extend(detect_stale_subject(store, &bindings).await?);
        findings.extend(detect_stale_scope(store, &bindings).await?);
        findings.extend(detect_no_op_membership_constraint(&bindings));
        if let Some(organization) = &organization {
            findings.extend(
                detect_recipient_boundary_no_op(self.memberships.as_ref(), organization, &bindings)
                    .await?,
            );
        }
        findings.extend(detect_selector_matches_nothing(store, &bindings).await?);
        findings.extend(detect_non_qualifying_org_admin_reference(&bindings));

        findings.retain(|finding| finding.subject.uid == uid);
        Ok(findings)
    }
}

/// Append findings up to the cap, latching `truncated` once it is hit.
///
/// Once latched, later calls append nothing more: a report is either whole or
/// marked partial, never partially-marked.
fn append_capped(
    findings: &mut Vec<AuditFinding>,
    truncated: &mut bool,
    max_findings: usize,
    mut new: Vec<AuditFinding>,
) {
    if *truncated || new.is_empty() {
        return;
    }
    if findings.len() + new.len() > max_findings {
        new.truncate(max_findings.saturating_sub(findings.len()));
        *truncated = true;
    }
    findings.append(&mut new);
}

/// The [`FindingSubject`] identifying a binding row itself.
fn binding_subject(binding: &BindingFact) -> FindingSubject {
    FindingSubject {
        uid: binding.provenance.uid,
        kind: binding.provenance.kind.to_string(),
        name: binding
            .provenance
            .name
            .clone()
            .unwrap_or_else(|| binding.provenance.uid.to_string()),
        organization: match &binding.tier {
            BindingTier::Platform => None,
            BindingTier::Organization(organization) => Some(organization.clone()),
        },
    }
}

async fn live_row_by_name(
    store: &dyn ResourceStore,
    kind: &str,
    name: &str,
    parent_uid: Option<Uuid>,
) -> Result<Option<ResourceRow>, AuthorizationError> {
    Ok(store
        .get_by_name(API_VERSION_V1ALPHA1, kind, name, parent_uid)
        .await?
        .filter(is_live))
}

async fn live_groups(
    store: &dyn ResourceStore,
    organization_uid: Uuid,
) -> Result<Vec<ResourceRow>, AuthorizationError> {
    Ok(store
        .list(API_VERSION_V1ALPHA1, GROUP_KIND, Some(organization_uid))
        .await?
        .into_iter()
        .filter(is_live)
        .collect())
}

/// A stored `User`'s `spec.active`, failing closed on a row that will not parse.
fn user_is_active(row: &ResourceRow) -> Result<bool, AuthorizationError> {
    let spec: UserSpec = serde_json::from_value(row.spec.clone()).map_err(|error| {
        AuthorizationError::corrupt_policy(format!(
            "stored User '{}' ({}) is not valid: {error}",
            row.name, row.uid
        ))
    })?;
    Ok(spec.active)
}

// ---------------------------------------------------------------------------
// Row 1: DanglingRoleRef
// ---------------------------------------------------------------------------

/// A live binding whose `roleRef` names no live Role/PlatformRole.
///
/// Evaluation already treats this as zero statements (`RoleCache::resolve`);
/// this only reports it. Always `Warning`: nothing about a dangling reference
/// self-heals — an operator has to repair or remove the binding.
pub fn detect_dangling_role_ref(bindings: &[BindingFact]) -> Vec<AuditFinding> {
    bindings
        .iter()
        .filter(|binding| !binding.role_resolved)
        .map(|binding| {
            let role_kind = match binding.provenance.role.kind {
                RoleRefKind::Role => "Role",
                RoleRefKind::PlatformRole => "PlatformRole",
            };
            let placement = match (&binding.tier, binding.provenance.role.kind) {
                (BindingTier::Organization(organization), RoleRefKind::Role) => {
                    format!(" in Organization '{organization}'")
                }
                _ => String::new(),
            };
            AuditFinding {
                subject: binding_subject(binding),
                category: FindingCategory::DanglingRoleRef,
                severity: Severity::Warning,
                related: Vec::new(),
                detail: format!(
                    "roleRef names {role_kind} '{}'{placement}, which no longer exists; the binding grants nothing.",
                    binding.provenance.role.name
                ),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Row 2: StaleSubject
// ---------------------------------------------------------------------------

/// A literal subject that names no live, active identity.
///
/// Templates and virtual subjects (`system:*`, `org:*`) are skipped: they name
/// a population or a predicate, never a stored row.
pub async fn detect_stale_subject(
    store: &dyn ResourceStore,
    bindings: &[BindingFact],
) -> Result<Vec<AuditFinding>, AuthorizationError> {
    let mut findings = Vec::new();
    for binding in bindings {
        let Some(subject) = binding.subject.literal() else {
            continue;
        };
        let stale = match subject.kind() {
            "user" => match live_row_by_name(store, USER_KIND, subject.name(), None).await? {
                Some(row) => !user_is_active(&row)?,
                None => true,
            },
            "controller" => live_row_by_name(store, CONTROLLER_KIND, subject.name(), None)
                .await?
                .is_none(),
            "group" => stale_org_native(store, GROUP_KIND, subject).await?,
            "serviceaccount" => stale_org_native(store, SERVICE_ACCOUNT_KIND, subject).await?,
            _ => continue,
        };
        if stale {
            findings.push(AuditFinding {
                subject: binding_subject(binding),
                category: FindingCategory::StaleSubject,
                severity: Severity::Warning,
                related: Vec::new(),
                detail: format!(
                    "subject '{subject}' names no live, active identity; the binding grants nothing."
                ),
            });
        }
    }
    Ok(findings)
}

/// Whether an org-native (`group:`/`serviceaccount:`) subject names no live row.
async fn stale_org_native(
    store: &dyn ResourceStore,
    kind: &str,
    subject: &SubjectId,
) -> Result<bool, AuthorizationError> {
    let Some(organization) = subject.organization() else {
        return Ok(true);
    };
    let Some(org_row) = live_row_by_name(store, ORGANIZATION_KIND, organization, None).await?
    else {
        return Ok(true);
    };
    Ok(
        live_row_by_name(store, kind, subject.name(), Some(org_row.uid))
            .await?
            .is_none(),
    )
}

// ---------------------------------------------------------------------------
// Row 3: StaleScope
// ---------------------------------------------------------------------------

/// A non-wildcard scope whose `(kind, names...)` chain resolves to no live row.
pub async fn detect_stale_scope(
    store: &dyn ResourceStore,
    bindings: &[BindingFact],
) -> Result<Vec<AuditFinding>, AuthorizationError> {
    let mut findings = Vec::new();
    for binding in bindings {
        let Some(resource_kind) = binding.scope.resource_kind() else {
            continue;
        };
        let Some(chain) = kind_chain(store, resource_kind.group(), resource_kind.kind()).await?
        else {
            // The kind itself is unregistered; nothing to validate the scope
            // against, and admission would have refused the write anyway.
            continue;
        };
        let names: Vec<&str> = binding.scope.names().collect();
        if names.len() != chain.len() {
            // A malformed scope is not this detector's concern; it should
            // never have passed admission.
            continue;
        }
        let segments: Vec<PathSegment> = chain
            .into_iter()
            .zip(names)
            .map(|((kind, api_versions), name)| PathSegment::Name {
                api_versions,
                kind,
                name: name.to_owned(),
            })
            .collect();
        let stale = match store.resolve_path(&segments).await {
            Ok(rows) => rows.iter().any(|row| row.deletion_timestamp.is_some()),
            Err(StoreError::NotFound | StoreError::ParentNotFound) => true,
            Err(other) => return Err(other.into()),
        };
        if stale {
            findings.push(AuditFinding {
                subject: binding_subject(binding),
                category: FindingCategory::StaleScope,
                severity: Severity::Warning,
                related: Vec::new(),
                detail: format!(
                    "scope '{}' names no live resource; the binding grants nothing.",
                    binding.scope
                ),
            });
        }
    }
    Ok(findings)
}

/// The `(kind, declared API versions)` chain a kind hangs under, root-first,
/// walked through the same registry admission resolves a `Scope` against.
///
/// `None` when the leaf kind, or an ancestor it declares, is not registered.
async fn kind_chain(
    store: &dyn ResourceStore,
    group: &str,
    kind: &str,
) -> Result<Option<Vec<(String, Vec<String>)>>, AuthorizationError> {
    let mut chain = Vec::new();
    let mut current_group = group.to_owned();
    let mut current_kind = kind.to_owned();
    loop {
        if chain.len() >= MAX_PARENT_CHAIN_DEPTH {
            return Ok(None);
        }
        let Some(info) = store
            .resolve_collection_by_kind(&current_group, &current_kind)
            .await?
        else {
            return Ok(None);
        };
        chain.push((current_kind.clone(), info.declared_api_versions.clone()));
        match info.parent {
            Some(parent) => {
                let Some((parent_group, _)) = parent.api_version.split_once('/') else {
                    return Ok(None);
                };
                current_group = parent_group.to_owned();
                current_kind = parent.kind.clone();
            }
            None => break,
        }
    }
    chain.reverse();
    Ok(Some(chain))
}

// ---------------------------------------------------------------------------
// Row 4: StaleMembership
// ---------------------------------------------------------------------------

/// A `GroupMembership` naming a User with no live, active root row.
///
/// Markers deliberately outlive Users (ADR-0001 §1) — recreating or
/// reactivating the name makes the tie live again — so this is `Info`, never
/// `Warning`.
pub async fn detect_stale_membership(
    store: &dyn ResourceStore,
    organization: &str,
    groups: &[ResourceRow],
) -> Result<Vec<AuditFinding>, AuthorizationError> {
    let mut findings = Vec::new();
    for group in groups {
        let memberships = store
            .list(API_VERSION_V1ALPHA1, GROUP_MEMBERSHIP_KIND, Some(group.uid))
            .await?;
        for membership in memberships.iter().filter(|row| is_live(row)) {
            let live_active =
                match live_row_by_name(store, USER_KIND, &membership.name, None).await? {
                    Some(row) => user_is_active(&row)?,
                    None => false,
                };
            if live_active {
                continue;
            }
            findings.push(AuditFinding {
                subject: FindingSubject {
                    uid: membership.uid,
                    kind: GROUP_MEMBERSHIP_KIND.to_owned(),
                    name: membership.name.clone(),
                    organization: Some(organization.to_owned()),
                },
                category: FindingCategory::StaleMembership,
                severity: Severity::Info,
                related: Vec::new(),
                detail: format!(
                    "GroupMembership names User '{}', which does not exist or is inactive.",
                    membership.name
                ),
            });
        }
    }
    Ok(findings)
}

// ---------------------------------------------------------------------------
// Row 5: NoOpMembershipConstraint
// ---------------------------------------------------------------------------

/// A `PlatformRoleBinding` whose `subjectMembership: ResourceOrganization`
/// constraint can never bite.
///
/// Purely structural: admission has already fixed a scope's ancestor-name
/// count to match its registered kind depth, so a single-segment scope is
/// provably root-scoped without a store lookup.
pub fn detect_no_op_membership_constraint(bindings: &[BindingFact]) -> Vec<AuditFinding> {
    bindings
        .iter()
        .filter(|binding| binding.provenance.kind == BindingKind::PlatformRoleBinding)
        .filter(|binding| binding.subject_membership == SubjectMembership::ResourceOrganization)
        .filter_map(|binding| {
            let detail = no_op_membership_reason(binding)?;
            Some(AuditFinding {
                subject: binding_subject(binding),
                category: FindingCategory::NoOpMembershipConstraint,
                severity: Severity::Warning,
                related: Vec::new(),
                detail,
            })
        })
        .collect()
}

fn no_op_membership_reason(binding: &BindingFact) -> Option<String> {
    if let Some(subject) = binding.subject.literal() {
        match subject.kind() {
            "group" | "serviceaccount" | "org" => {
                return Some(format!(
                    "subjectMembership: ResourceOrganization can never bite: subject '{subject}' already carries its own organization."
                ));
            }
            "controller" => {
                return Some(format!(
                    "subjectMembership: ResourceOrganization can never bite: subject '{subject}' belongs to no organization and is never affiliated."
                ));
            }
            _ => {}
        }
    }
    let is_root_scoped_non_organization = binding.scope.names().len() == 1
        && binding
            .scope
            .resource_kind()
            .is_some_and(|kind| !(kind.group() == API_GROUP && kind.kind() == ORGANIZATION_KIND));
    if is_root_scoped_non_organization {
        return Some(format!(
            "subjectMembership: ResourceOrganization can never bite: scope '{}' names a root-scoped resource with no organization to compare against.",
            binding.scope
        ));
    }
    None
}

// ---------------------------------------------------------------------------
// Row 6: RecipientBoundaryNoOp
// ---------------------------------------------------------------------------

/// An org `RoleBinding` whose literal subject can never, or does not
/// currently, belong to its own Organization.
///
/// Admission already rejects the foreign-org shapes (a `group:`/`serviceaccount:`/
/// `org:` subject naming another Organization), so only `controller:` (permanent)
/// and `user:` (contingent on live membership) reach this detector.
pub async fn detect_recipient_boundary_no_op(
    memberships: &dyn MembershipResolver,
    organization: &str,
    org_bindings: &[BindingFact],
) -> Result<Vec<AuditFinding>, AuthorizationError> {
    let mut findings = Vec::new();
    for binding in org_bindings {
        let Some(subject) = binding.subject.literal() else {
            continue;
        };
        match subject.kind() {
            "controller" => {
                findings.push(AuditFinding {
                    subject: binding_subject(binding),
                    category: FindingCategory::RecipientBoundaryNoOp,
                    severity: Severity::Warning,
                    related: Vec::new(),
                    detail: format!(
                        "subject '{subject}' belongs to no organization; this RoleBinding can never reach it."
                    ),
                });
            }
            "user" => {
                if user_belongs_to_organization(memberships, org_bindings, subject, organization)
                    .await?
                {
                    continue;
                }
                findings.push(AuditFinding {
                    subject: binding_subject(binding),
                    category: FindingCategory::RecipientBoundaryNoOp,
                    severity: Severity::Info,
                    related: Vec::new(),
                    detail: format!(
                        "subject '{subject}' has no live Group tie in Organization '{organization}' and is not a qualifying admin there; this RoleBinding currently grants nothing."
                    ),
                });
            }
            _ => {}
        }
    }
    Ok(findings)
}

/// Whether a `user:` subject currently belongs to `organization`, either
/// through a live Group tie or the direct org-admin bootstrap edge (ADR-0001
/// §5) — the same two paths `AuthorizationEngine::subject_belongs_to` checks
/// for a live caller.
async fn user_belongs_to_organization(
    memberships: &dyn MembershipResolver,
    org_bindings: &[BindingFact],
    user: &SubjectId,
    organization: &str,
) -> Result<bool, AuthorizationError> {
    let groups = memberships.groups_for_user(user).await?;
    if groups
        .iter()
        .any(|group| group.organization() == Some(organization))
    {
        return Ok(true);
    }
    Ok(org_bindings.iter().any(|candidate| {
        qualifies_as_org_admin(candidate, organization) && candidate.subject.literal() == Some(user)
    }))
}

// ---------------------------------------------------------------------------
// Row 7: InertOwnerLabel
// ---------------------------------------------------------------------------

/// Every live `rise.dev/owner` setter in the store, paired with the
/// organization its own ancestry resolves to (`None` for a root-scoped
/// resource).
///
/// Resolved once regardless of [`AuditScope`]: `list_label_setters` has no
/// per-organization form, so the audit reads it once and filters in memory
/// rather than repeat the same bounded scan per organization in scope.
async fn owner_label_setters_by_organization(
    store: &dyn ResourceStore,
) -> Result<Vec<(ResourceRow, Option<String>)>, AuthorizationError> {
    let key: LabelKey = OWNER_LABEL_KEY
        .parse()
        .expect("shipped owner label key parses");
    let setters = store
        .list_label_setters(&key, LABEL_SETTER_SCAN_LIMIT)
        .await?;
    let mut resolved = Vec::with_capacity(setters.len());
    for row in setters {
        let ancestry = store.ancestors(row.uid).await?;
        let organization = ResourceTree::from_rows(&ancestry)?
            .organization()
            .map(str::to_owned);
        resolved.push((row, organization));
    }
    Ok(resolved)
}

/// A `rise.dev/owner` value that the seeded `${ref.subject}` ownership binding
/// (ADR-0001 §6.2) cannot turn into a live grant.
///
/// Resolution mirrors exactly what the live binding does: parse the value as
/// the seeded binding's dynamic subject against the resource's own
/// organization. A value that fails to parse is always `Warning` — nothing
/// about it self-heals. A Group or ServiceAccount it names is org-native, so a
/// missing live row is also permanent and `Warning`. A User it names is only
/// `Info`: the seeded binding's `ResourceOrganization` clamp means the grant is
/// live exactly while that User is affiliated with the resource's
/// organization, which can change without anyone touching the label.
/// Root-scoped resources (no organization) are skipped for the User case only
/// — the clamp has nothing to compare against there — but a malformed value on
/// a root-scoped resource is still reported.
pub async fn detect_inert_owner_label(
    store: &dyn ResourceStore,
    memberships: &dyn MembershipResolver,
    organization: Option<&str>,
    org_bindings: &[BindingFact],
    setters: &[ResourceRow],
) -> Result<Vec<AuditFinding>, AuthorizationError> {
    let seeded_subject = resource_owner_binding_spec().subject;
    let mut findings = Vec::new();
    for row in setters {
        let Some(value) = row.labels.get(OWNER_LABEL_KEY) else {
            // Cannot happen for a row `list_label_setters(OWNER_LABEL_KEY, _)`
            // returned, but a detector must not panic on a store's promise.
            continue;
        };
        let subject = || FindingSubject {
            uid: row.uid,
            kind: row.kind.clone(),
            name: row.name.clone(),
            organization: organization.map(str::to_owned),
        };

        let resolved = match resolve_subject(&seeded_subject, Some(value.as_str()), organization) {
            Ok(resolved) => resolved,
            Err(_) => {
                findings.push(AuditFinding {
                    subject: subject(),
                    category: FindingCategory::InertOwnerLabel,
                    severity: Severity::Warning,
                    related: Vec::new(),
                    detail: format!(
                        "label '{OWNER_LABEL_KEY}' is set to '{value}', which does not resolve to a valid subject; the seeded ownership binding grants nobody."
                    ),
                });
                continue;
            }
        };

        match resolved.kind() {
            "group" | "serviceaccount" => {
                let kind = if resolved.kind() == "group" {
                    GROUP_KIND
                } else {
                    SERVICE_ACCOUNT_KIND
                };
                if stale_org_native(store, kind, &resolved).await? {
                    findings.push(AuditFinding {
                        subject: subject(),
                        category: FindingCategory::InertOwnerLabel,
                        severity: Severity::Warning,
                        related: Vec::new(),
                        detail: format!(
                            "label '{OWNER_LABEL_KEY}' is set to '{value}', which resolves to '{resolved}'; that subject names no live resource, so ownership grants nobody."
                        ),
                    });
                }
            }
            "user" => {
                let Some(organization) = organization else {
                    continue;
                };
                if user_belongs_to_organization(memberships, org_bindings, &resolved, organization)
                    .await?
                {
                    continue;
                }
                findings.push(AuditFinding {
                    subject: subject(),
                    category: FindingCategory::InertOwnerLabel,
                    severity: Severity::Info,
                    related: Vec::new(),
                    detail: format!(
                        "label '{OWNER_LABEL_KEY}' is set to '{value}', naming subject '{resolved}', which has no live Group tie in Organization '{organization}' and is not a qualifying admin there; ownership currently grants nothing."
                    ),
                });
            }
            _ => {}
        }
    }
    Ok(findings)
}

// ---------------------------------------------------------------------------
// Row 8: SelectorMatchesNothing
// ---------------------------------------------------------------------------

/// How many live setters [`detect_selector_matches_nothing`] reads before
/// giving up on proving a selector matches nothing. Also the bound
/// [`owner_label_setters_by_organization`] applies to its single global scan.
///
/// Both detectors need this to stay exact rather than heuristic: reporting
/// "matches nothing" past this point would risk a false positive on a large
/// install, so a scan that hits the limit without a match is treated as
/// unknown and produces no finding instead.
const LABEL_SETTER_SCAN_LIMIT: i64 = 1000;

/// A binding's `labelSelector` that no resource in its scope can ever satisfy.
///
/// Exact and cheap without a subtree walk: a non-wildcard scope is first
/// checked through its own effective labels (nearest-wins inheritance already
/// covers every resource that inherits, rather than sets, the key), and only a
/// scope whose own root does not carry it falls back to
/// [`ResourceStore::list_label_setters`] to look for a setter at or below that
/// root. A wildcard scope has no root to check first, so only the setter scan
/// applies.
pub async fn detect_selector_matches_nothing(
    store: &dyn ResourceStore,
    bindings: &[BindingFact],
) -> Result<Vec<AuditFinding>, AuthorizationError> {
    let mut findings = Vec::new();
    for binding in bindings {
        let Some(selector) = &binding.selector else {
            continue;
        };
        if selector_matches_something(store, &binding.scope, selector).await? != Some(false) {
            continue;
        }
        let value_clause = match &selector.value {
            Some(value) => format!("value '{value}'"),
            None => "any value".to_owned(),
        };
        // A dynamic-subject binding exists to wait for labels: the seeded
        // `resource-owner` binding matches nothing on a fresh install by
        // design, and starts mattering the moment a resource is labelled. A
        // literal subject selecting on a key nobody sets is the shape worth a
        // warning.
        let severity = if binding.subject.is_dynamic() {
            Severity::Info
        } else {
            Severity::Warning
        };
        findings.push(AuditFinding {
            subject: binding_subject(binding),
            category: FindingCategory::SelectorMatchesNothing,
            severity,
            related: Vec::new(),
            detail: format!(
                "labelSelector key '{}' ({value_clause}) matches no resource in scope '{}'; the binding currently grants nothing.",
                selector.key, binding.scope,
            ),
        });
    }
    Ok(findings)
}

/// `Some(true)`/`Some(false)` when whether `selector` matches anything in
/// `scope` is decided; `None` when the scope does not resolve (row 3 already
/// reports that separately) or the setter scan hit [`LABEL_SETTER_SCAN_LIMIT`]
/// before a match turned up, leaving the true answer unknown.
async fn selector_matches_something(
    store: &dyn ResourceStore,
    scope: &Scope,
    selector: &LabelSelector,
) -> Result<Option<bool>, AuthorizationError> {
    let Some(resource_kind) = scope.resource_kind() else {
        return matches_via_setters(store, selector, None).await;
    };
    let Some(chain) = kind_chain(store, resource_kind.group(), resource_kind.kind()).await? else {
        // Unregistered kind: nothing to validate the scope against, matching
        // `detect_stale_scope`'s own skip for the same shape.
        return Ok(None);
    };
    let names: Vec<&str> = scope.names().collect();
    if names.len() != chain.len() {
        // Malformed scope: never this detector's concern either.
        return Ok(None);
    }
    let segments: Vec<PathSegment> = chain
        .into_iter()
        .zip(names)
        .map(|((kind, api_versions), name)| PathSegment::Name {
            api_versions,
            kind,
            name: name.to_owned(),
        })
        .collect();
    let root = match store.resolve_path(&segments).await {
        Ok(rows) if rows.iter().any(|row| row.deletion_timestamp.is_some()) => return Ok(None),
        Ok(rows) => rows
            .into_iter()
            .last()
            .expect("a non-empty scope resolves to a leaf row"),
        Err(StoreError::NotFound | StoreError::ParentNotFound) => return Ok(None),
        Err(other) => return Err(other.into()),
    };

    let ancestry = store.ancestors(root.uid).await?;
    let effective = ResourceTree::from_rows(&ancestry)?.effective_labels();
    if selector_matches_labels(&effective, selector) {
        return Ok(Some(true));
    }
    matches_via_setters(store, selector, Some(root.uid)).await
}

/// Whether some live resource that sets `selector.key` (and `selector.value`,
/// when given) lies at or below `scope_root` — or anywhere, for a wildcard
/// scope's `None`.
async fn matches_via_setters(
    store: &dyn ResourceStore,
    selector: &LabelSelector,
    scope_root: Option<Uuid>,
) -> Result<Option<bool>, AuthorizationError> {
    let setters = store
        .list_label_setters(&selector.key, LABEL_SETTER_SCAN_LIMIT)
        .await?;
    let exhausted = setters.len() as i64 == LABEL_SETTER_SCAN_LIMIT;
    for setter in &setters {
        if !selector_matches_labels(&setter.labels, selector) {
            continue;
        }
        let in_scope = match scope_root {
            None => true,
            Some(root_uid) => store
                .ancestors(setter.uid)
                .await?
                .iter()
                .any(|ancestor| ancestor.uid == root_uid),
        };
        if in_scope {
            return Ok(Some(true));
        }
    }
    Ok(if exhausted { None } else { Some(false) })
}

fn selector_matches_labels(labels: &BTreeMap<String, String>, selector: &LabelSelector) -> bool {
    match labels.get(selector.key.as_ref()) {
        Some(value) => selector
            .value
            .as_deref()
            .is_none_or(|expected| expected == value),
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Row 9: ShadowedAllow { by: Deny }
// ---------------------------------------------------------------------------

/// A binding's `PolicyDomain`: the same `(scope, selector)` pair
/// [`domain_covers`] and [`replaces`] compare over, borrowed straight from the
/// binding facts the audit already loaded.
fn binding_domain(binding: &BindingFact) -> PolicyDomain {
    PolicyDomain {
        scope: binding.scope.clone(),
        selector: binding.selector.clone(),
    }
}

/// A `binding`'s field values reduced to what [`replaces`] and
/// [`wildcard_allows_suppressed`](crate::policy::wildcard_allows_suppressed)
/// need — the subject, scope and selector its placement is judged by — without
/// cloning its (possibly large) statement list.
fn as_applicable(binding: &BindingFact) -> ApplicableBinding<()> {
    ApplicableBinding {
        provenance: (),
        subject: binding.subject.clone(),
        scope: binding.scope.clone(),
        selector: binding.selector.clone(),
        tier: binding.tier.clone(),
        statements: Vec::new(),
    }
}

/// An Allow statement in a live binding that a platform-tier Deny shadows.
///
/// Platform Denies only: an org Deny is escaped by that org's own admins
/// (ADR-0001 §5's exemption), so it never shadows an Allow for every caller
/// the way a platform Deny does. `Info`, not `Warning`: a platform ceiling
/// sitting over a narrower Allow is usually deliberate defense in depth, not a
/// mistake — unlike a dangling reference or a stale scope, nothing here is
/// obviously wrong.
pub fn detect_shadowed_by_deny(
    platform: &[BindingFact],
    bindings: &[BindingFact],
) -> Vec<AuditFinding> {
    let mut findings = Vec::new();
    for binding in bindings {
        let allows: Vec<&PolicyStatement> = binding
            .statements
            .iter()
            .filter(|statement| statement.effect == Effect::Allow)
            .collect();
        if allows.is_empty() {
            continue;
        }
        let domain = binding_domain(binding);
        for deny_binding in platform {
            if deny_binding.provenance.uid == binding.provenance.uid {
                continue;
            }
            let denies: Vec<&PolicyStatement> = deny_binding
                .statements
                .iter()
                .filter(|statement| statement.effect == Effect::Deny)
                .collect();
            if denies.is_empty() {
                continue;
            }
            if !subject_covers(&deny_binding.subject, &binding.subject) {
                continue;
            }
            if !domain_covers(&binding_domain(deny_binding), &domain) {
                continue;
            }
            let covered = allows
                .iter()
                .filter(|allow| denies.iter().any(|deny| deny_covers_allow(deny, allow)))
                .count();
            if covered == 0 {
                continue;
            }
            findings.push(AuditFinding {
                subject: binding_subject(binding),
                category: FindingCategory::ShadowedAllow { by: Shadow::Deny },
                severity: Severity::Info,
                related: vec![binding_subject(deny_binding)],
                detail: format!(
                    "{} '{}' denies {covered} of this binding's {} Allow statements; they grant nothing under it.",
                    deny_binding.provenance.kind,
                    deny_binding
                        .provenance
                        .name
                        .clone()
                        .unwrap_or_else(|| deny_binding.provenance.uid.to_string()),
                    allows.len(),
                ),
            });
        }
    }
    findings
}

// ---------------------------------------------------------------------------
// Row 10: ShadowedAllow { by: Replacement }
// ---------------------------------------------------------------------------

/// An Allow statement a wildcard-scoped platform binding loses to a more
/// specific binding through ADR-0001 §1's wildcard-replacement pairing.
///
/// Only platform bindings can carry `scope: "*"` at all — org containment
/// keeps an org binding's scope inside its own parent's subtree — so the
/// candidate that replaces it can be platform or org tier, but the wildcard
/// side is always read from `platform`. Always `Warning`: unlike a platform
/// Deny ceiling, this is a specific binding actively taking over from a
/// broader one the moment both are applicable, which is worth surfacing even
/// when intentional.
pub fn detect_shadowed_by_replacement(
    platform: &[BindingFact],
    bindings: &[BindingFact],
) -> Vec<AuditFinding> {
    let mut findings = Vec::new();
    for wildcard in platform
        .iter()
        .filter(|binding| binding.scope.is_wildcard())
    {
        if !wildcard
            .statements
            .iter()
            .any(|statement| statement.effect == Effect::Allow)
        {
            // Nothing for a more specific binding to drop.
            continue;
        }
        let wildcard_applicable = as_applicable(wildcard);
        for candidate in bindings {
            if candidate.provenance.uid == wildcard.provenance.uid {
                continue;
            }
            if !replaces(&as_applicable(candidate), &wildcard_applicable) {
                continue;
            }
            findings.push(AuditFinding {
                subject: binding_subject(wildcard),
                category: FindingCategory::ShadowedAllow {
                    by: Shadow::Replacement,
                },
                severity: Severity::Warning,
                related: vec![binding_subject(candidate)],
                detail: format!(
                    "on resources inside scope '{}', this binding's Allow statements are dropped by wildcard replacement; {} '{}' applies its own statements there instead.",
                    candidate.scope,
                    candidate.provenance.kind,
                    candidate
                        .provenance
                        .name
                        .clone()
                        .unwrap_or_else(|| candidate.provenance.uid.to_string()),
                ),
            });
        }
    }
    findings
}

// ---------------------------------------------------------------------------
// Row 11: OrganizationWithoutAdmin
// ---------------------------------------------------------------------------

/// An Organization with no qualifying binding naming a live, active admin.
pub async fn detect_organization_without_admin(
    store: &dyn ResourceStore,
    organization: &ResourceRow,
    org_bindings: &[BindingFact],
) -> Result<Vec<AuditFinding>, AuthorizationError> {
    if organization_has_live_admin(store, &organization.name, org_bindings).await? {
        return Ok(Vec::new());
    }
    Ok(vec![AuditFinding {
        subject: FindingSubject {
            uid: organization.uid,
            kind: ORGANIZATION_KIND.to_owned(),
            name: organization.name.clone(),
            organization: Some(organization.name.clone()),
        },
        category: FindingCategory::OrganizationWithoutAdmin,
        severity: Severity::Warning,
        related: Vec::new(),
        detail: format!(
            "Organization '{}' has no live, active admin: no qualifying binding names a live User directly or through a Group with a live, active member.",
            organization.name
        ),
    }])
}

/// Whether some qualifying binding names a live, active User, or a Group with
/// at least one live, active member.
async fn organization_has_live_admin(
    store: &dyn ResourceStore,
    organization: &str,
    org_bindings: &[BindingFact],
) -> Result<bool, AuthorizationError> {
    for binding in org_bindings
        .iter()
        .filter(|binding| qualifies_as_org_admin(binding, organization))
    {
        let Some(subject) = binding.subject.literal() else {
            continue;
        };
        let qualifies = match subject.kind() {
            "user" => match live_row_by_name(store, USER_KIND, subject.name(), None).await? {
                Some(row) => user_is_active(&row)?,
                None => false,
            },
            "group" => group_has_live_admin_member(store, subject).await?,
            _ => false,
        };
        if qualifies {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether a `group:<org>/<name>` subject names a live Group with at least one
/// `GroupMembership` naming a live, active User.
async fn group_has_live_admin_member(
    store: &dyn ResourceStore,
    group: &SubjectId,
) -> Result<bool, AuthorizationError> {
    let Some(organization) = group.organization() else {
        return Ok(false);
    };
    let Some(org_row) = live_row_by_name(store, ORGANIZATION_KIND, organization, None).await?
    else {
        return Ok(false);
    };
    let Some(group_row) =
        live_row_by_name(store, GROUP_KIND, group.name(), Some(org_row.uid)).await?
    else {
        return Ok(false);
    };
    let memberships = store
        .list(
            API_VERSION_V1ALPHA1,
            GROUP_MEMBERSHIP_KIND,
            Some(group_row.uid),
        )
        .await?;
    for membership in memberships.iter().filter(|row| is_live(row)) {
        if let Some(user_row) = live_row_by_name(store, USER_KIND, &membership.name, None).await? {
            if user_is_active(&user_row)? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// Row 12: NonQualifyingOrgAdminReference
// ---------------------------------------------------------------------------

/// A `PlatformRole/org-admin` reference that fails ADR-0001 §5's exact
/// org-root, scope-only shape for its own placement.
///
/// Every `PlatformRoleBinding` fails this unconditionally: the tier alone
/// disqualifies it, regardless of what its scope names.
pub fn detect_non_qualifying_org_admin_reference(bindings: &[BindingFact]) -> Vec<AuditFinding> {
    bindings
        .iter()
        .filter(|binding| {
            binding.provenance.role.kind == RoleRefKind::PlatformRole
                && binding.provenance.role.name == ORG_ADMIN_PLATFORM_ROLE
        })
        .filter(|binding| match &binding.tier {
            BindingTier::Organization(organization) => {
                !qualifies_as_org_admin(binding, organization)
            }
            BindingTier::Platform => true,
        })
        .map(|binding| AuditFinding {
            subject: binding_subject(binding),
            category: FindingCategory::NonQualifyingOrgAdminReference,
            severity: Severity::Warning,
            related: Vec::new(),
            detail: format!(
                "roleRef names PlatformRole/{ORG_ADMIN_PLATFORM_ROLE} but this binding is not an exact org-root, scope-only reference; it grants the baseline statements without conferring admin standing or the Deny exemption."
            ),
        })
        .collect()
}
