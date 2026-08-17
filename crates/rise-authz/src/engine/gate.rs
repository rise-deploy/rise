//! ADR-0001 §5's write-time grant gate, and §6.6's label-write gate.
//!
//! Every authorization-changing write answers one question:
//!
//! ```text
//! newly_implied = EffectivePolicy(recipient, after) − EffectivePolicy(recipient, before)
//! require newly_implied ⊆ EffectivePolicy(writer, before)
//! ```
//!
//! The gate expresses that as a set of [`GrantClaim`]s — one recipient, one
//! policy domain, a before policy and an after policy — and checks each against
//! the writer's own authority over the same domain. What differs between a Role
//! edit, a binding move, a membership edge, an identity mapping, and a label
//! write is how claims are produced; the comparison never differs.
//!
//! Three properties are load-bearing throughout:
//!
//! * **Recipients are authored subjects, not expanded identities.** A claim's
//!   recipient is the `subject` a binding literally carries. Expanding a
//!   recipient's Groups could only reveal authority they *already* hold, which
//!   shrinks the delta — and a smaller delta is the unsafe direction on a gate.
//!   The one exception is an identity mapping, where the arrow points the other
//!   way and [`MembershipResolver::groups_for_user`] is consulted.
//! * **Unproven relationships resolve against the writer.** An Allow counts
//!   towards the writer's authority only when its domain demonstrably covers the
//!   claim's; a Deny counts against it unless its domain demonstrably misses it.
//! * **The binding universe must cover every tier that can reach a claim.**
//!   Under-loading an organization's tier would drop its Denies, and a missing
//!   Deny makes the before-policy look *larger* — the one direction that shrinks
//!   a delta. [`OrganizationScope`] is why loads fan out rather than guess.
//!
//! Nothing here writes. The caller runs it inside the same serializable
//! transaction as the mutation (§5), which is what makes the simulated delta and
//! the committed row agree, and — for a label write — ahead of §6.7's
//! referential-integrity check, so a caller the gate refuses never learns
//! whether the subject they named exists.
//!
//! [`MembershipResolver::groups_for_user`]: crate::engine::MembershipResolver::groups_for_user

use std::collections::{BTreeMap, BTreeSet, HashMap};

use rise_resource_api::{
    BindingSubject, Effect, LabelKey, LabelSelector, PolicyStatement, ResourceKind, ResourceRow,
    ResourceStore, RoleRefKind, Scope, SubjectId, SubjectMembership, API_GROUP,
    API_VERSION_V1ALPHA1, MAX_PARENT_CHAIN_DEPTH, ORGANIZATION_KIND, PLATFORM_ROLE_KIND, ROLE_KIND,
};
use uuid::Uuid;

use crate::engine::bindings::{
    load_organization_bindings, load_platform_bindings, parse_role_statements, BindingFact,
};
use crate::engine::{
    qualifies_as_org_admin, AuthorizationEngine, AuthorizationError, AuthorizationSnapshot,
    BindingKind, BindingProvenance, ResourceNode, ResourceTree, RoleReference,
};
use crate::policy::{
    domain_covers_with, domains_provably_disjoint_with, resolve_subject,
    unjustified_new_tuples_under_ceiling, wildcard_allows_suppressed, ApplicableBinding,
    BindingTier, PermissionTuple, PolicyDomain,
};

// -----------------------------------------------------------------------------
// Change description
// -----------------------------------------------------------------------------

/// A binding as the gate sees it before or after a write.
///
/// This is the authored spec plus its placement — everything needed to know
/// which resources the binding reaches and who receives its authority. The
/// statements come from resolving `role` against the store, so a caller cannot
/// misdeclare what a binding grants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingState {
    /// The binding's UID. A create that has no row yet may use any UID not
    /// already stored; it identifies which stored row the after-universe
    /// replaces, nothing more.
    pub uid: Uuid,
    pub kind: BindingKind,
    /// The Organization the binding is parented under: always `Some` for a
    /// `RoleBinding` and always `None` for a `PlatformRoleBinding`, matching
    /// ADR-0001 §3's exact placement.
    pub organization: Option<String>,
    pub subject: BindingSubject,
    pub subject_membership: SubjectMembership,
    pub scope: Scope,
    pub selector: Option<LabelSelector>,
    pub role: RoleReference,
}

impl BindingState {
    fn tier(&self) -> Result<BindingTier, AuthorizationError> {
        match (self.kind, &self.organization) {
            (BindingKind::PlatformRoleBinding, None) => Ok(BindingTier::Platform),
            (BindingKind::RoleBinding, Some(organization)) => {
                Ok(BindingTier::Organization(organization.clone()))
            }
            (kind, Some(organization)) => Err(AuthorizationError::invalid_input(format!(
                "{kind} cannot be parented under Organization '{organization}'"
            ))),
            (kind, None) => Err(AuthorizationError::invalid_input(format!(
                "{kind} cannot be parented at the root"
            ))),
        }
    }

    fn domain(&self) -> GateDomain {
        GateDomain::new(
            self.scope.clone(),
            self.selector.clone(),
            self.subject_membership,
        )
    }

    async fn to_fact(
        &self,
        engine: &AuthorizationEngine,
    ) -> Result<BindingFact, AuthorizationError> {
        Ok(BindingFact {
            provenance: BindingProvenance {
                uid: self.uid,
                kind: self.kind,
                role: self.role.clone(),
            },
            tier: self.tier()?,
            subject: self.subject.clone(),
            subject_membership: self.subject_membership,
            scope: self.scope.clone(),
            selector: self.selector.clone(),
            statements: engine
                .role_statements(&self.role, self.organization.as_deref())
                .await?,
        })
    }
}

/// A `Role` or `PlatformRole` body write.
///
/// `before` and `after` are statement lists; an empty list stands in for a body
/// that does not exist, so a create is `before: []` and a delete is `after: []`.
/// `organization` is `Some` for an org `Role` and `None` for a `PlatformRole`.
#[derive(Debug, Clone)]
pub struct RoleBodyChange {
    pub kind: RoleRefKind,
    pub name: String,
    pub organization: Option<String>,
    pub before: Vec<PolicyStatement>,
    pub after: Vec<PolicyStatement>,
}

/// A binding create, edit, move, or delete.
///
/// Deleting a binding is a grant like any other: removing it can expose another
/// binding's Allow that its Deny was suppressing (ADR-0001 §5, scenario 31).
#[derive(Debug, Clone)]
pub struct BindingChange {
    pub before: Option<Box<BindingState>>,
    pub after: Option<Box<BindingState>>,
}

/// A `GroupMembership` edge being created.
///
/// Deleting one is governed by ADR-0001 §4's governance-boundary rule instead:
/// losing a tie removes authority, and removal introduces none.
#[derive(Debug, Clone)]
pub struct GroupMembershipChange {
    /// Canonical `user:<name>` subject the membership names.
    pub user: SubjectId,
    /// Canonical `group:<org>/<name>` subject of the Group it is parented under.
    pub group: SubjectId,
}

/// A `UserIdentity`, `ControllerTrustPolicy`, or `ServiceAccountTrustPolicy`
/// being created or retargeted.
///
/// ADR-0001 §5 treats the parent identity's complete effective policy as newly
/// reachable through the new credential path, so the writer must already hold all
/// of it. Tightening or deleting a mapping introduces no authority and does not
/// reach this gate.
#[derive(Debug, Clone)]
pub struct IdentityMappingChange {
    /// Canonical subject of the identity the mapping attaches to.
    pub identity: SubjectId,
    /// Whether the mapping matches the process's restart-loaded operator selector
    /// set, and would therefore confer operator standing.
    ///
    /// The gate cannot derive this: `operatorIdentities` is configuration — the
    /// external bootstrap authority ADR-0001 §1 deliberately places outside the
    /// store. The caller fills it from the same selector set backing
    /// `MembershipResolver::resolve`. Only an operator may create such a mapping;
    /// no amount of ordinary authority delegates the recovery tier, because there
    /// is no tier above it to undo the mistake.
    pub confers_operator: bool,
}

/// An access-driving label write (ADR-0001 §6.6).
#[derive(Debug, Clone)]
pub struct LabelChange {
    /// The resource being written, with its stored ancestry and current labels.
    /// For a creation this is the parent ancestry plus the proposed leaf.
    pub target: ResourceTree,
    /// The target's UID, or `None` when it does not exist yet — a resource with
    /// no row has no inheriting subtree to diff.
    pub target_uid: Option<Uuid>,
    pub key: LabelKey,
    /// The proposed own value for `key`. `None` removes it, re-exposing whatever
    /// an ancestor supplies.
    pub after_value: Option<String>,
    /// Whether this write brings a genuinely new, previously nonexistent resource
    /// identity into being — the only case §6.6's narrow creation exception can
    /// apply to.
    ///
    /// Restoring a soft-deleted resource and an upsert whose target already
    /// exists are **not** creations: the exception's safety rests on there being
    /// no prior owner to displace. The caller must resolve existence first.
    pub creation: bool,
}

/// One authorization-changing write, in the terms the gate diffs.
#[derive(Debug, Clone)]
pub enum AuthorizationChange {
    RoleBody(RoleBodyChange),
    Binding(BindingChange),
    GroupMembership(GroupMembershipChange),
    IdentityMapping(IdentityMappingChange),
    Label(LabelChange),
}

// -----------------------------------------------------------------------------
// Verdict
// -----------------------------------------------------------------------------

/// A binding's reachable domain, with `subjectMembership` folded in.
///
/// `ResourceOrganization` restricts a binding to resources inside the recipient's
/// own organization, which narrows *which resources it reaches* rather than *what
/// it permits*. Modelling it as part of the domain is what makes relaxing the
/// constraint to `Any` register as the grant ADR-0001 §5 says it is: an unclamped
/// domain covers its clamped twin and never the reverse, so a clamped
/// before-policy stops covering the unclamped after-policy and the writer has to
/// justify the whole thing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateDomain {
    pub domain: PolicyDomain,
    pub organization_clamped: bool,
}

impl GateDomain {
    fn new(scope: Scope, selector: Option<LabelSelector>, membership: SubjectMembership) -> Self {
        Self {
            domain: PolicyDomain { scope, selector },
            organization_clamped: membership == SubjectMembership::ResourceOrganization,
        }
    }
}

/// One recipient's authority over one domain, before and after the write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantClaim {
    /// The authored subject receiving the authority.
    pub recipient: BindingSubject,
    /// The domain the delta is computed over.
    pub domain: GateDomain,
    /// The domain the *writer's* own authority is measured over.
    ///
    /// Usually identical to `domain`. It differs for a label write, where the
    /// delta is computed over the domain the new value creates while ADR-0001
    /// §5 measures the writer against `EffectivePolicy(writer, before)` — the
    /// world as it stands, with the label still naming whoever it named. Pinning
    /// the writer's side to the *old* value is what lets a resource's current
    /// owner hand ownership on: their own access arrives through that value.
    pub writer_domain: GateDomain,
    pub before: Vec<PolicyStatement>,
    pub after: Vec<PolicyStatement>,
}

/// Why a write was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectionReason {
    /// The writer does not hold authority the write would newly confer. The
    /// tuples name what is missing.
    InsufficientAuthority(Vec<PermissionTuple>),
    /// The write would confer operator standing, which only an operator may do.
    OperatorStandingRequired,
}

/// One refused claim, in terms that name the missing authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateRejection {
    pub recipient: BindingSubject,
    pub domain: Option<GateDomain>,
    pub reason: RejectionReason,
}

/// The gate's verdict, with the claims it compared retained for auditing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateOutcome {
    pub claims: Vec<GrantClaim>,
    pub rejections: Vec<GateRejection>,
    /// Set when §6.6's creation exception carried the write: every selecting
    /// binding resolved the proposed value to the creator or one of their own
    /// same-org Groups, so nothing was delegated at all.
    pub creation_exception: bool,
}

impl GateOutcome {
    pub fn permitted(&self) -> bool {
        self.rejections.is_empty()
    }

    fn permitted_outcome() -> Self {
        Self {
            claims: Vec::new(),
            rejections: Vec::new(),
            creation_exception: false,
        }
    }
}

/// Which organizations' policy tiers a comparison has to load.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OrganizationScope {
    /// Every Organization. Required whenever a change can reach an unbounded set
    /// of them: a `PlatformRole` body edit, a wildcard-scoped platform binding,
    /// or an identity mapping.
    All,
    Named(BTreeSet<String>),
}

impl OrganizationScope {
    fn named(names: impl IntoIterator<Item = String>) -> Self {
        Self::Named(names.into_iter().collect())
    }
}

// -----------------------------------------------------------------------------
// Entry point
// -----------------------------------------------------------------------------

impl AuthorizationEngine {
    /// Run ADR-0001 §5's write-time grant gate for one authorization-changing
    /// write.
    ///
    /// The caller must already hold ordinary write access to the resource being
    /// changed; this answers the separate question of whether the writer may
    /// *delegate* the authority the change confers.
    pub async fn evaluate_grant(
        &self,
        snapshot: &AuthorizationSnapshot,
        change: &AuthorizationChange,
    ) -> Result<GateOutcome, AuthorizationError> {
        // An operator holds every permission over every domain and ignores every
        // Deny, so no delta can escape their own authority. Short-circuiting
        // keeps the recovery tier working even when stored policy is in exactly
        // the broken state they are there to repair.
        if snapshot.membership.is_operator {
            return Ok(GateOutcome::permitted_outcome());
        }

        if let AuthorizationChange::IdentityMapping(mapping) = change {
            if mapping.confers_operator {
                return Ok(GateOutcome {
                    claims: Vec::new(),
                    rejections: vec![GateRejection {
                        recipient: BindingSubject::Literal(mapping.identity.clone()),
                        domain: None,
                        reason: RejectionReason::OperatorStandingRequired,
                    }],
                    creation_exception: false,
                });
            }
        }

        let (claims, creation_exception) = match change {
            AuthorizationChange::RoleBody(role) => (self.role_body_claims(role).await?, false),
            AuthorizationChange::Binding(binding) => (self.binding_claims(binding).await?, false),
            AuthorizationChange::GroupMembership(membership) => {
                (self.group_membership_claims(membership).await?, false)
            }
            AuthorizationChange::IdentityMapping(mapping) => {
                (self.identity_mapping_claims(mapping).await?, false)
            }
            AuthorizationChange::Label(label) => self.label_claims(snapshot, label).await?,
        };

        // Load the writer's side once for the whole call rather than once per
        // claim. A §6.6 subtree diff produces one claim per inheriting
        // descendant, and each measures the writer over its own domain, so a
        // per-claim reload would multiply a cold path by the size of the subtree.
        // The facts are still read fresh for every request — nothing here
        // outlives the call.
        let writer_facts = self.writer_facts(&claims).await?;

        let mut rejections = Vec::new();
        for claim in &claims {
            let (writer, ceiling) = self
                .writer_authority(snapshot, &claim.writer_domain, &writer_facts)
                .await?;
            let missing = unjustified_new_tuples_under_ceiling(
                &claim.before,
                &claim.after,
                &writer,
                ceiling.as_deref(),
            );
            if !missing.is_empty() {
                rejections.push(GateRejection {
                    recipient: claim.recipient.clone(),
                    domain: Some(claim.domain.clone()),
                    reason: RejectionReason::InsufficientAuthority(missing),
                });
            }
        }

        Ok(GateOutcome {
            claims,
            rejections,
            creation_exception,
        })
    }

    // -------------------------------------------------------------------------
    // Claim construction
    // -------------------------------------------------------------------------

    /// A Role body edit spans every live binding of that Role (ADR-0001 §5): the
    /// body confers nothing by itself, so the delta is computed once per
    /// `(recipient, domain)` its bindings deliver it to. An unbound Role produces
    /// no claims and therefore creates no authority — scenario 30.
    ///
    /// A `PlatformRole` can be referenced from any organization's bindings, so
    /// editing one fans out across every tier. That is a cold path, and paying
    /// for it is the only way the edit's real reach is measured.
    async fn role_body_claims(
        &self,
        change: &RoleBodyChange,
    ) -> Result<Vec<GrantClaim>, AuthorizationError> {
        let organizations = match change.kind {
            RoleRefKind::PlatformRole => OrganizationScope::All,
            RoleRefKind::Role => OrganizationScope::named(change.organization.clone()),
        };
        let universe = self.binding_universe(&organizations).await?;
        let lattice = self.scope_lattice(&universe, &[]).await?;

        let referencing: BTreeSet<Uuid> = universe
            .iter()
            .filter(|binding| {
                binding.provenance.role.kind == change.kind
                    && binding.provenance.role.name == change.name
                    && role_placement_matches(binding, change)
            })
            .map(|binding| binding.provenance.uid)
            .collect();

        let overlay = |statements: &[PolicyStatement]| -> Vec<BindingFact> {
            universe
                .iter()
                .map(|binding| {
                    let mut binding = binding.clone();
                    if referencing.contains(&binding.provenance.uid) {
                        binding.statements = statements.to_vec();
                    }
                    binding
                })
                .collect()
        };
        let before_universe = overlay(&change.before);
        let after_universe = overlay(&change.after);

        let claims = universe
            .iter()
            .filter(|binding| referencing.contains(&binding.provenance.uid))
            .map(|binding| {
                claim_over(
                    &before_universe,
                    &after_universe,
                    &binding.subject,
                    fact_domain(binding),
                    &lattice,
                )
            })
            .collect();
        Ok(dedupe_claims(claims))
    }

    /// A binding write is diffed over both the domain it leaves and the domain it
    /// arrives at, so a move is gated on its new reach and a delete on its old.
    async fn binding_claims(
        &self,
        change: &BindingChange,
    ) -> Result<Vec<GrantClaim>, AuthorizationError> {
        let states: Vec<&BindingState> = change
            .before
            .iter()
            .chain(change.after.iter())
            .map(Box::as_ref)
            .collect();
        if states.is_empty() {
            return Ok(Vec::new());
        }

        let mut organizations = BTreeSet::new();
        let mut spans_every_organization = false;
        for state in &states {
            organizations.extend(state.organization.clone());
            // A platform binding's scope can reach into an organization it is not
            // parented under, and a wildcard scope reaches all of them. Both
            // tiers have to be loaded or that organization's Denies go missing.
            match self.scope_organization(&state.scope).await? {
                Some(organization) => {
                    organizations.insert(organization);
                }
                None if !state.scope.is_wildcard() => {}
                None => spans_every_organization = true,
            }
        }
        let organizations = if spans_every_organization {
            OrganizationScope::All
        } else {
            OrganizationScope::Named(organizations)
        };

        let stored = self.binding_universe(&organizations).await?;
        let touched: BTreeSet<Uuid> = states.iter().map(|state| state.uid).collect();
        let untouched: Vec<BindingFact> = stored
            .iter()
            .filter(|binding| !touched.contains(&binding.provenance.uid))
            .cloned()
            .collect();

        let mut before_universe = untouched.clone();
        if let Some(before) = &change.before {
            before_universe.push(before.to_fact(self).await?);
        }
        let mut after_universe = untouched;
        if let Some(after) = &change.after {
            after_universe.push(after.to_fact(self).await?);
        }

        let lattice = self
            .scope_lattice(
                &stored,
                &states
                    .iter()
                    .map(|state| state.scope.clone())
                    .collect::<Vec<_>>(),
            )
            .await?;
        let claims = states
            .iter()
            .map(|state| {
                claim_over(
                    &before_universe,
                    &after_universe,
                    &state.subject,
                    state.domain(),
                    &lattice,
                )
            })
            .collect();
        Ok(dedupe_claims(claims))
    }

    /// Adding a User to a Group hands that User everything the Group holds.
    ///
    /// Per domain: the after policy is what the User and the Group hold together,
    /// the before policy what the User held alone.
    async fn group_membership_claims(
        &self,
        change: &GroupMembershipChange,
    ) -> Result<Vec<GrantClaim>, AuthorizationError> {
        require_subject_kind(&change.user, "user", "a GroupMembership's User")?;
        require_subject_kind(&change.group, "group", "a GroupMembership's Group")?;
        let organizations =
            OrganizationScope::named(change.group.organization().map(str::to_owned));
        let universe = self.binding_universe(&organizations).await?;
        let lattice = self.scope_lattice(&universe, &[]).await?;

        let recipient = BindingSubject::Literal(change.user.clone());
        let group = BindingSubject::Literal(change.group.clone());
        let claims = authored_domains(&universe, &group)
            .into_iter()
            .map(|domain| {
                let before = aggregate(&universe, &recipient, &domain, &lattice);
                let after =
                    merge_statements(&before, &aggregate(&universe, &group, &domain, &lattice));
                GrantClaim {
                    recipient: recipient.clone(),
                    writer_domain: domain.clone(),
                    domain,
                    before,
                    after,
                }
            })
            .collect();
        Ok(dedupe_claims(claims))
    }

    /// A new credential path to an existing identity makes that identity's whole
    /// effective policy reachable, so the before side is empty everywhere.
    ///
    /// This is the one place a recipient's Groups are expanded: leaving them out
    /// would understate what the mapping reaches, and understating a delta is the
    /// unsafe direction.
    async fn identity_mapping_claims(
        &self,
        change: &IdentityMappingChange,
    ) -> Result<Vec<GrantClaim>, AuthorizationError> {
        let mut reachable = BTreeSet::new();
        reachable.insert(BindingSubject::Literal(change.identity.clone()));
        if change.identity.kind() == "user" {
            for group in self.memberships.groups_for_user(&change.identity).await? {
                require_subject_kind(&group, "group", "a resolved Group tie")?;
                reachable.insert(BindingSubject::Literal(group));
            }
        }

        // The identity's authority can be delivered from any organization's tier,
        // including one it holds no membership in — an operator-authored binding
        // naming it directly. Loading every tier is the only complete answer.
        let universe = self.binding_universe(&OrganizationScope::All).await?;
        let lattice = self.scope_lattice(&universe, &[]).await?;

        let mut claims = Vec::new();
        for recipient in &reachable {
            for domain in authored_domains(&universe, recipient) {
                let after = aggregate(&universe, recipient, &domain, &lattice);
                claims.push(GrantClaim {
                    recipient: recipient.clone(),
                    writer_domain: domain.clone(),
                    domain,
                    before: Vec::new(),
                    after,
                });
            }
        }
        Ok(dedupe_claims(claims))
    }

    /// ADR-0001 §6.6's three steps for one access-driving label write.
    ///
    /// Returns no claims when the write is ungated — an unchanged effective value
    /// (step 1) or a key no applicable binding selects on (step 2) — and the
    /// creation-exception flag when every selecting binding resolves the proposed
    /// value to the creator or one of their own same-org Groups.
    async fn label_claims(
        &self,
        snapshot: &AuthorizationSnapshot,
        change: &LabelChange,
    ) -> Result<(Vec<GrantClaim>, bool), AuthorizationError> {
        let key = change.key.as_ref();

        // Step 1: an unchanged *effective* value is an ordinary update, so
        // re-stating a value the resource already inherits is equally ungated.
        //
        // A creation has no stored leaf, so its before-value is whatever its
        // ancestry supplies — deriving it here rather than trusting the caller to
        // have left the proposed value out of `target` keeps the comparison from
        // silently becoming "unchanged" and skipping the gate entirely.
        let before_effective = if change.creation {
            inherited_value(change.target.nodes(), key)
        } else {
            change.target.effective_labels().get(key).cloned()
        };
        let after_effective = match &change.after_value {
            Some(value) => Some(value.clone()),
            // Removing the resource's own value re-exposes the nearest ancestor's
            // — exactly the escalation a diff over the stored label alone would
            // misread as a de-escalation.
            None => inherited_value(change.target.nodes(), key),
        };
        if before_effective == after_effective {
            return Ok((Vec::new(), false));
        }

        // Step 2: applicability is a property of the bindings, not of the
        // resource's present labels. A key becomes gated the moment some binding
        // placed over this location selects on it, whether or not that binding
        // currently matches anything.
        let organizations =
            OrganizationScope::named(change.target.organization().map(str::to_owned));
        let universe = self.binding_universe(&organizations).await?;
        let selecting: Vec<&BindingFact> = universe
            .iter()
            .filter(|binding| {
                binding
                    .selector
                    .as_ref()
                    .is_some_and(|selector| selector.key.as_ref() == key)
            })
            .collect();
        if selecting.is_empty()
            || !selecting
                .iter()
                .any(|binding| change.target.covered_by(&binding.scope))
        {
            return Ok((Vec::new(), false));
        }

        // Step 3: the diff spans the written resource and its K-inheriting
        // subtree, because relabelling one node moves the effective value on
        // every descendant that had no value of its own.
        let mut affected = vec![change.target.clone()];
        if let Some(uid) = change.target_uid {
            affected.extend(self.inheriting_subtree(&change.target, uid, key).await?);
        }

        let mut scopes = Vec::new();
        for tree in &affected {
            scopes.push(resource_scope(tree)?);
        }
        let lattice = self.scope_lattice(&universe, &scopes).await?;

        let mut claims = Vec::new();
        let mut claimable = true;
        for (tree, scope) in affected.iter().zip(&scopes) {
            let before_value = if change.creation {
                inherited_value(tree.nodes(), key)
            } else {
                tree.effective_labels().get(key).cloned()
            };
            let after_value = affected_after_value(after_effective.as_deref());

            for binding in &selecting {
                if !tree.covered_by(&binding.scope) {
                    continue;
                }
                let resource_organization = tree.organization();
                let before_subject =
                    selected_subject(binding, before_value.as_deref(), resource_organization);
                let after_subject =
                    selected_subject(binding, after_value.as_deref(), resource_organization);
                if before_subject == after_subject {
                    continue;
                }
                // Only the newly-named subject gains anything. Whoever the value
                // named before loses access, which needs no delegation.
                let Some(recipient) = after_subject else {
                    continue;
                };

                if !self
                    .claimable_by_creator(snapshot, &recipient, resource_organization)
                    .await?
                {
                    claimable = false;
                }

                // The claim's domain is this resource's own subtree pinned to the
                // value that makes the binding match — the exact reach of what is
                // being handed over, rather than the binding's whole declared
                // domain.
                let domain = GateDomain::new(
                    scope.clone(),
                    Some(LabelSelector {
                        key: change.key.clone(),
                        value: after_value.clone(),
                    }),
                    binding.subject_membership,
                );
                // The writer is measured against the world as it stands, so their
                // side pins the *old* value. That is what lets the resource's
                // current owner transfer ownership: their own `resource-owner`
                // access arrives through the very label being replaced, and a
                // domain pinned to the new value would credit them with none.
                let writer_domain = GateDomain::new(
                    scope.clone(),
                    Some(LabelSelector {
                        key: change.key.clone(),
                        value: before_value.clone(),
                    }),
                    binding.subject_membership,
                );
                let recipient = BindingSubject::Literal(recipient);
                let before = aggregate(&universe, &recipient, &domain, &lattice);
                claims.push(GrantClaim {
                    after: merge_statements(&before, &binding.statements),
                    recipient,
                    domain,
                    writer_domain,
                    before,
                });
            }
        }

        // The exception is creation-only and all-or-nothing: one unclaimable
        // resolved subject sends the whole write back to the general rule.
        if change.creation && claimable && !claims.is_empty() {
            return Ok((Vec::new(), true));
        }
        Ok((dedupe_claims(claims), false))
    }

    /// Whether §6.6's creation exception can carry this resolved subject: it must
    /// be the creator's own canonical User subject, or a Group they currently
    /// belong to in the resource's own organization.
    async fn claimable_by_creator(
        &self,
        snapshot: &AuthorizationSnapshot,
        recipient: &SubjectId,
        resource_organization: Option<&str>,
    ) -> Result<bool, AuthorizationError> {
        if recipient == snapshot.principal.subject() {
            return Ok(true);
        }
        if recipient.kind() != "group" || !snapshot.membership.groups.contains(recipient) {
            return Ok(false);
        }
        // Naming a Group the creator belongs to in *another* organization would
        // hand access across a boundary the creator has no standing to cross.
        Ok(match (recipient.organization(), resource_organization) {
            (Some(group_organization), Some(organization)) => group_organization == organization,
            _ => false,
        })
    }

    /// Rebuild each K-inheriting descendant's own evaluation target.
    ///
    /// The store returns rows parent-before-child, so each one's ancestry is its
    /// parent's chain plus itself. Stitching them here keeps `covered_by` and
    /// organization resolution exact per descendant instead of assuming they all
    /// share the written resource's position in the tree.
    async fn inheriting_subtree(
        &self,
        target: &ResourceTree,
        uid: Uuid,
        key: &str,
    ) -> Result<Vec<ResourceTree>, AuthorizationError> {
        let rows = self.store.label_inheriting_descendants(uid, key).await?;
        let mut chains: BTreeMap<Uuid, Vec<ResourceNode>> = BTreeMap::new();
        chains.insert(uid, target.nodes().to_vec());
        let mut trees = Vec::new();
        for row in &rows {
            let Some(parent_uid) = row.parent_uid else {
                continue;
            };
            // A row whose parent was pruned cannot inherit the written value, so
            // the store never returns one; skipping keeps this correct rather
            // than inventing an ancestry if that ever stops holding.
            let Some(parent_chain) = chains.get(&parent_uid).cloned() else {
                continue;
            };
            let mut chain = parent_chain;
            chain.push(node_from_row(row)?);
            chains.insert(row.uid, chain.clone());
            trees.push(ResourceTree::new(chain)?);
        }
        Ok(trees)
    }

    // -------------------------------------------------------------------------
    // Writer authority
    // -------------------------------------------------------------------------

    /// The writer's own authority over one domain, with their credential ceiling
    /// returned alongside it.
    ///
    /// This is `EffectivePolicy(writer, before)` restricted to a domain rather
    /// than to a single resource. Two asymmetries make it fail-closed:
    ///
    /// * Only bindings whose domain **covers** this one contribute Allows. A
    ///   binding reaching part of a domain does not establish authority over all
    ///   of it, and §5 is explicit that a narrow Scope cannot justify a broader
    ///   grant.
    /// * Every binding not **provably disjoint** from it contributes its Denies,
    ///   dynamic-subject bindings included. A Deny that might bite somewhere in
    ///   the domain means the writer does not hold that tuple throughout it.
    ///
    /// Dynamic-subject bindings never contribute Allows: a template grants over
    /// whichever resources a label happens to name, which is not a whole domain.
    async fn writer_authority(
        &self,
        snapshot: &AuthorizationSnapshot,
        domain: &GateDomain,
        facts: &WriterFacts,
    ) -> Result<(Vec<PolicyStatement>, Option<Vec<PolicyStatement>>), AuthorizationError> {
        // An operator is short-circuited before any claim is built, so reaching
        // here means ordinary RBAC decides.
        let organization = facts.organization(&domain.domain.scope);
        let universe = &facts.universe;
        let lattice = &facts.lattice;

        let organization_admin = match &organization {
            Some(organization) => self.standing(snapshot, organization).await?.admin,
            None => false,
        };

        let mut applicable: Vec<ApplicableBinding<BindingProvenance>> = Vec::new();
        let mut denies = Vec::new();
        for binding in universe {
            let binding_domain = fact_domain(binding);
            let covers = gate_domain_covers(&binding_domain, domain, lattice);
            let overlaps = !gate_domains_disjoint(&binding_domain, domain, lattice);

            // A template only names somebody once a label value is in hand. A
            // domain that pins one — every §6.6 claim does — resolves it exactly;
            // a domain that does not cannot show the template names the caller
            // across the whole thing, so it contributes no Allow.
            let resolved = match binding.subject.literal() {
                Some(subject) => Some(subject.clone()),
                None => resolve_subject(
                    &binding.subject,
                    domain.domain.selector.as_ref().and_then(|selector| {
                        (selector.key == binding.selector.as_ref()?.key)
                            .then_some(selector.value.as_deref()?)
                    }),
                    organization.as_deref(),
                )
                .ok(),
            };
            let subject_match = match &resolved {
                Some(subject) => self.subject_matches_caller(snapshot, subject).await?,
                None => false,
            };

            // A Deny whose subject cannot be resolved here might still bite
            // somewhere inside the domain, so it is retained: unproven
            // relationships resolve against the writer.
            if overlaps && (subject_match || resolved.is_none()) {
                let exempt = organization_admin
                    && matches!(&binding.tier, BindingTier::Organization(org)
                        if Some(org.as_str()) == organization.as_deref());
                if !exempt {
                    denies.extend(allows_or_denies(binding, Effect::Deny));
                }
            }
            if covers && subject_match {
                applicable.push(applicable_binding(binding));
            }
        }

        let mut statements = surviving_allows(&applicable);
        statements.extend(denies);
        let ceiling = self.ceiling_over_domain(snapshot, domain, lattice);
        Ok((statements, ceiling))
    }

    /// Resolve every fact the writer side of this call needs, once.
    ///
    /// The claims' domains are already known by the time this runs, so the
    /// organizations they lie in, the binding tiers that can restrict the writer
    /// inside them, and the registry chains their scopes compare through are all
    /// read together. A wildcard domain spans every organization, so any org tier
    /// can restrict the writer there and the load fans out.
    async fn writer_facts(&self, claims: &[GrantClaim]) -> Result<WriterFacts, AuthorizationError> {
        // An ungated write — an unchanged effective label, a key no binding
        // selects on, an unbound Role — produces no claims, and is the common
        // case. It should cost no reads at all.
        if claims.is_empty() {
            return Ok(WriterFacts::default());
        }
        let mut organizations = HashMap::new();
        let mut named = BTreeSet::new();
        let mut spans_every_organization = false;
        let mut scopes = Vec::new();
        for claim in claims {
            let scope = &claim.writer_domain.domain.scope;
            scopes.push(scope.clone());
            if organizations.contains_key(scope.as_ref()) {
                continue;
            }
            let organization = self.scope_organization(scope).await?;
            match &organization {
                Some(organization) => {
                    named.insert(organization.clone());
                }
                None if scope.is_wildcard() => spans_every_organization = true,
                None => {}
            }
            organizations.insert(scope.as_ref().to_owned(), organization);
        }

        let universe = self
            .binding_universe(&if spans_every_organization {
                OrganizationScope::All
            } else {
                OrganizationScope::Named(named)
            })
            .await?;
        let lattice = self.scope_lattice(&universe, &scopes).await?;
        Ok(WriterFacts {
            universe,
            lattice,
            organizations,
        })
    }

    /// The ceiling entries that cover a whole domain.
    ///
    /// An entry covering only part of it does not license the writer across the
    /// domain, so it is dropped — which means a restricted credential with no
    /// covering entry holds nothing there at all.
    fn ceiling_over_domain(
        &self,
        snapshot: &AuthorizationSnapshot,
        domain: &GateDomain,
        lattice: &ScopeLattice,
    ) -> Option<Vec<PolicyStatement>> {
        snapshot
            .principal
            .authorization_cap()
            .statements_covering(&domain.domain.scope, &|covering, covered| {
                lattice.covers(covering, covered)
            })
    }

    // -------------------------------------------------------------------------
    // Shared fact loading
    // -------------------------------------------------------------------------

    /// Every live binding that can reach the requested tiers: the complete
    /// platform tier plus each named organization's own tier.
    ///
    /// Write-time containment keeps an org binding's scope inside its parent
    /// Organization's subtree, so this pair is exhaustive for any resource in
    /// those organizations — the same reasoning that lets the evaluator skip a
    /// scope index.
    async fn binding_universe(
        &self,
        organizations: &OrganizationScope,
    ) -> Result<Vec<BindingFact>, AuthorizationError> {
        let mut universe = load_platform_bindings(self.store.as_ref()).await?;
        let names = match organizations {
            OrganizationScope::All => self.organization_names().await?,
            OrganizationScope::Named(names) => names.clone(),
        };
        for name in names {
            universe.extend(load_organization_bindings(self.store.as_ref(), &name).await?);
        }
        Ok(universe)
    }

    async fn organization_names(&self) -> Result<BTreeSet<String>, AuthorizationError> {
        Ok(self
            .store
            .list(API_VERSION_V1ALPHA1, ORGANIZATION_KIND, None)
            .await?
            .into_iter()
            .filter(|row| row.deletion_timestamp.is_none())
            .map(|row| row.name)
            .collect())
    }

    /// Which Organization a scope lies in, if it lies in exactly one.
    ///
    /// A wildcard scope spans every organization and has none of its own. A
    /// deeper scope names its Organization first only if that root really is one,
    /// which is confirmed against the store so a same-named resource of another
    /// root kind cannot borrow an organization's policy tier.
    async fn scope_organization(
        &self,
        scope: &Scope,
    ) -> Result<Option<String>, AuthorizationError> {
        let Some(root) = scope.names().next().map(str::to_owned) else {
            return Ok(None);
        };
        let names_organization = scope
            .resource_kind()
            .is_some_and(|kind| kind.group() == API_GROUP && kind.kind() == ORGANIZATION_KIND)
            && scope.names().len() == 1;
        if names_organization {
            return Ok(Some(root));
        }
        Ok(self
            .store
            .get_by_name(API_VERSION_V1ALPHA1, ORGANIZATION_KIND, &root, None)
            .await?
            .filter(|row| row.deletion_timestamp.is_none())
            .map(|row| row.name))
    }

    /// Resolve a `roleRef` to its statements at the placement its kind implies.
    /// A dangling reference resolves to no statements, exactly as in evaluation.
    async fn role_statements(
        &self,
        role: &RoleReference,
        organization: Option<&str>,
    ) -> Result<Vec<PolicyStatement>, AuthorizationError> {
        let (kind, parent_uid) = match role.kind {
            RoleRefKind::PlatformRole => (PLATFORM_ROLE_KIND, None),
            RoleRefKind::Role => {
                let organization = organization.ok_or_else(|| {
                    AuthorizationError::invalid_input(
                        "an org Role reference requires the binding's Organization",
                    )
                })?;
                let parent = self
                    .store
                    .get_by_name(API_VERSION_V1ALPHA1, ORGANIZATION_KIND, organization, None)
                    .await?
                    .filter(|row| row.deletion_timestamp.is_none());
                match parent {
                    Some(parent) => (ROLE_KIND, Some(parent.uid)),
                    None => return Ok(Vec::new()),
                }
            }
        };
        Ok(
            match self
                .store
                .get_by_name(API_VERSION_V1ALPHA1, kind, &role.name, parent_uid)
                .await?
                .filter(|row| row.deletion_timestamp.is_none())
            {
                Some(row) => parse_role_statements(&row)?,
                None => Vec::new(),
            },
        )
    }

    /// Resolve every concrete scope one comparison will compare, so the
    /// containment predicate itself stays a pure function.
    async fn scope_lattice(
        &self,
        universe: &[BindingFact],
        extra: &[Scope],
    ) -> Result<ScopeLattice, AuthorizationError> {
        ScopeLattice::load(
            self.store.as_ref(),
            universe
                .iter()
                .map(|binding| &binding.scope)
                .chain(extra.iter()),
        )
        .await
    }
}

// -----------------------------------------------------------------------------
// Scope containment
// -----------------------------------------------------------------------------

/// The writer-side facts one `evaluate_grant` call reads, shared by every claim.
#[derive(Default)]
struct WriterFacts {
    universe: Vec<BindingFact>,
    lattice: ScopeLattice,
    /// Which Organization each claim's writer-domain scope lies in, if any.
    organizations: HashMap<String, Option<String>>,
}

impl WriterFacts {
    fn organization(&self, scope: &Scope) -> Option<String> {
        self.organizations.get(scope.as_ref()).cloned().flatten()
    }
}

/// Registry-resolved kind chains for the scopes one gate run compares.
///
/// A `Scope` spells out its leaf kind and its root-first names but not the kinds
/// of the nodes in between, so deciding whether `rise.dev/Organization/acme`
/// covers `rise.dev/Project/acme/web` needs the registry's parent chain for
/// `Project`. Resolving every scope once up front keeps the containment
/// predicate a pure function, which is what lets the aggregation below stay
/// synchronous — and lets it be fuzzed without a store.
///
/// An unresolvable scope maps to no chain and is treated as covering nothing.
/// Fail-closed matters here specifically: a scope naming a kind that is not
/// registered, or whose depth disagrees with its kind's chain, describes an
/// empty domain, and crediting an empty domain with coverage would let a writer
/// hold authority "over" resources they can never reach.
#[derive(Default)]
struct ScopeLattice {
    chains: HashMap<String, Vec<ResourceKind>>,
}

impl ScopeLattice {
    async fn load<'a>(
        store: &dyn ResourceStore,
        scopes: impl Iterator<Item = &'a Scope>,
    ) -> Result<Self, AuthorizationError> {
        let mut chains = HashMap::new();
        let mut resolved: HashMap<ResourceKind, Option<Vec<ResourceKind>>> = HashMap::new();
        for scope in scopes {
            let Some(kind) = scope.resource_kind() else {
                continue;
            };
            if chains.contains_key(scope.as_ref()) {
                continue;
            }
            if !resolved.contains_key(kind) {
                let chain = kind_chain(store, kind).await?;
                resolved.insert(kind.clone(), chain);
            }
            if let Some(chain) = resolved.get(kind).and_then(Option::as_ref) {
                chains.insert(scope.as_ref().to_owned(), chain.clone());
            }
        }
        Ok(Self { chains })
    }

    /// Whether one concrete scope's subtree contains another's.
    ///
    /// `covering`'s names must be a root-first prefix of `covered`'s, and
    /// `covering`'s own kind must be the kind that sits at that position in
    /// `covered`'s registered chain. Both scopes must be well-formed — chain
    /// depth equal to name count — or the answer is no.
    fn covers(&self, covering: &Scope, covered: &Scope) -> bool {
        let (Some(covering_chain), Some(covered_chain)) = (
            self.chains.get(covering.as_ref()),
            self.chains.get(covered.as_ref()),
        ) else {
            return false;
        };
        let covering_names: Vec<&str> = covering.names().collect();
        let covered_names: Vec<&str> = covered.names().collect();
        if covering_chain.len() != covering_names.len()
            || covered_chain.len() != covered_names.len()
            || covering_names.is_empty()
            || covering_names.len() > covered_names.len()
        {
            return false;
        }
        covered_names[..covering_names.len()] == covering_names[..]
            && covered_chain
                .get(covering_names.len() - 1)
                .is_some_and(|kind| Some(kind) == covering.resource_kind())
    }
}

/// The root-first registered kind chain for one leaf kind, or `None` when the
/// kind is unregistered or its chain exceeds the store's bound.
async fn kind_chain(
    store: &dyn ResourceStore,
    kind: &ResourceKind,
) -> Result<Option<Vec<ResourceKind>>, AuthorizationError> {
    let mut chain = Vec::new();
    let mut cursor = Some((kind.group().to_owned(), kind.kind().to_owned()));
    while let Some((group, name)) = cursor {
        if chain.len() > MAX_PARENT_CHAIN_DEPTH {
            return Ok(None);
        }
        let Some(info) = store.resolve_collection_by_kind(&group, &name).await? else {
            return Ok(None);
        };
        chain.push(ResourceKind::new(&group, &info.kind)?);
        cursor = info.parent.map(|parent| {
            (
                parent
                    .api_version
                    .split_once('/')
                    .map_or(parent.api_version.clone(), |(group, _)| group.to_owned()),
                parent.kind,
            )
        });
    }
    chain.reverse();
    Ok(Some(chain))
}

fn gate_domain_covers(covering: &GateDomain, covered: &GateDomain, lattice: &ScopeLattice) -> bool {
    // An unclamped domain covers its clamped twin; the reverse would credit a
    // membership-limited grant with reach it does not have.
    (!covering.organization_clamped || covered.organization_clamped)
        && domain_covers_with(&covering.domain, &covered.domain, &|covering, covered| {
            lattice.covers(covering, covered)
        })
}

fn gate_domains_disjoint(left: &GateDomain, right: &GateDomain, lattice: &ScopeLattice) -> bool {
    // A clamp narrows a domain but never proves it misses another one, so
    // disjointness is decided on scope and selector alone.
    domains_provably_disjoint_with(&left.domain, &right.domain, &|left, right| {
        lattice.covers(left, right)
    })
}

// -----------------------------------------------------------------------------
// Aggregation
// -----------------------------------------------------------------------------

/// One authored recipient's policy over one domain, drawn from a binding universe.
///
/// Allows come only from bindings whose domain covers this one; Denies come from
/// every binding not provably disjoint from it. Wildcard replacement runs over
/// the covering set, and an org-tier Deny is dropped when the recipient's own
/// standing in that organization exempts them — computed structurally from the
/// same universe, so a promotion in the after-set correctly exposes the authority
/// the before-set's Denies were hiding (ADR-0001 §5).
fn aggregate(
    universe: &[BindingFact],
    recipient: &BindingSubject,
    domain: &GateDomain,
    lattice: &ScopeLattice,
) -> Vec<PolicyStatement> {
    let reaching = reaching_subjects(universe, recipient);
    let mut applicable = Vec::new();
    let mut denies = Vec::new();
    for binding in universe
        .iter()
        .filter(|binding| reaching.contains(&binding.subject))
    {
        let binding_domain = fact_domain(binding);
        if !gate_domains_disjoint(&binding_domain, domain, lattice) {
            let exempt = matches!(&binding.tier, BindingTier::Organization(org)
                if recipient_is_organization_admin(universe, recipient, org));
            if !exempt {
                denies.extend(allows_or_denies(binding, Effect::Deny));
            }
        }
        if gate_domain_covers(&binding_domain, domain, lattice) {
            applicable.push(applicable_binding(binding));
        }
    }

    let mut statements = surviving_allows(&applicable);
    statements.extend(denies);
    statements
}

/// Every authored subject that provably reaches whatever identities `recipient`
/// reaches.
///
/// Authored-subject equality alone is not enough, and ADR-0001 §5's scenario 29
/// is why: a capped admin must be able to appoint another admin covered by the
/// same platform Deny, which requires that Deny — delivered through `org:acme`,
/// not through the new admin's own name — to appear in the recipient's policy.
/// Aggregating only exact-subject bindings would leave the denied tuple inside
/// the delta and refuse the appointment.
///
/// Only provable reach is included, so nothing here credits the recipient with
/// authority they may not have:
///
/// * `system:authenticated` reaches every principal, by definition.
/// * A `group:<O>/…` or `serviceaccount:<O>/…` recipient carries its own
///   organization, so `org:<O>` reaches everyone it reaches.
/// * A `user:` recipient's affiliation is proven only by §5's bootstrap edge — a
///   direct qualifying org-admin binding naming them. Group ties are not
///   consulted, deliberately: they would have to come from live membership,
///   which is the fact the gate refuses to depend on.
///
/// The set is computed per universe, so a promotion contained in the write
/// itself changes which Denies reach the recipient afterwards — exactly §5's
/// "promotion to admin may remove org-tier Denies, so that newly exposed
/// authority is part of the delta".
///
/// Mis-modelling a Deny that is present in *both* universes cannot manufacture a
/// grant: it suppresses the tuple on both sides, so the tuple is either absent
/// from the delta or absent from the recipient's real authority too. A Deny the
/// write itself adds or removes belongs to a changed binding, whose authored
/// subject is the claim's recipient by construction.
fn reaching_subjects(
    universe: &[BindingFact],
    recipient: &BindingSubject,
) -> BTreeSet<BindingSubject> {
    let mut reaching = BTreeSet::new();
    reaching.insert(recipient.clone());
    if let Ok(authenticated) = "system:authenticated".parse::<SubjectId>() {
        reaching.insert(BindingSubject::Literal(authenticated));
    }
    for organization in provable_affiliations(universe, recipient) {
        if let Ok(subject) = format!("org:{organization}").parse::<SubjectId>() {
            reaching.insert(BindingSubject::Literal(subject));
        }
    }
    reaching
}

/// The organizations every identity behind an authored subject provably belongs
/// to. See [`reaching_subjects`] for why only these three sources count.
fn provable_affiliations(universe: &[BindingFact], recipient: &BindingSubject) -> BTreeSet<String> {
    let Some(subject) = recipient.literal() else {
        return BTreeSet::new();
    };
    match subject.kind() {
        "group" | "serviceaccount" | "org" => subject
            .organization()
            .map(str::to_owned)
            .into_iter()
            .collect(),
        "user" => universe
            .iter()
            .filter(|binding| &binding.subject == recipient)
            .filter_map(|binding| match &binding.tier {
                BindingTier::Organization(organization) => {
                    qualifies_as_org_admin(binding, organization).then(|| organization.clone())
                }
                BindingTier::Platform => None,
            })
            .collect(),
        _ => BTreeSet::new(),
    }
}

/// ADR-0001 §5's structural admin predicate, applied to an authored recipient.
///
/// Admin standing is never read from a Role's contents, and here it is not read
/// from live membership either: a qualifying binding naming this exact authored
/// subject is the whole test. That keeps the classification a pure function of
/// the binding universe being diffed, which is what lets a promotion inside the
/// same write register as the Deny exemption it is.
fn recipient_is_organization_admin(
    universe: &[BindingFact],
    recipient: &BindingSubject,
    organization: &str,
) -> bool {
    universe
        .iter()
        .filter(|binding| &binding.subject == recipient)
        .any(|binding| qualifies_as_org_admin(binding, organization))
}

/// Every distinct domain over which some binding delivers authority to this
/// authored subject.
fn authored_domains(universe: &[BindingFact], recipient: &BindingSubject) -> Vec<GateDomain> {
    let mut domains: Vec<GateDomain> = Vec::new();
    for binding in universe
        .iter()
        .filter(|binding| &binding.subject == recipient)
    {
        let domain = fact_domain(binding);
        if !domains.contains(&domain) {
            domains.push(domain);
        }
    }
    domains
}

fn claim_over(
    before_universe: &[BindingFact],
    after_universe: &[BindingFact],
    recipient: &BindingSubject,
    domain: GateDomain,
    lattice: &ScopeLattice,
) -> GrantClaim {
    GrantClaim {
        recipient: recipient.clone(),
        before: aggregate(before_universe, recipient, &domain, lattice),
        after: aggregate(after_universe, recipient, &domain, lattice),
        writer_domain: domain.clone(),
        domain,
    }
}

fn fact_domain(binding: &BindingFact) -> GateDomain {
    GateDomain::new(
        binding.scope.clone(),
        binding.selector.clone(),
        binding.subject_membership,
    )
}

fn applicable_binding(binding: &BindingFact) -> ApplicableBinding<BindingProvenance> {
    ApplicableBinding {
        provenance: binding.provenance.clone(),
        subject: binding.subject.clone(),
        scope: binding.scope.clone(),
        selector: binding.selector.clone(),
        tier: binding.tier.clone(),
        statements: binding.statements.clone(),
    }
}

fn allows_or_denies(binding: &BindingFact, effect: Effect) -> Vec<PolicyStatement> {
    binding
        .statements
        .iter()
        .filter(|statement| statement.effect == effect)
        .cloned()
        .collect()
}

/// The Allows that survive wildcard replacement across an applicable set.
fn surviving_allows(applicable: &[ApplicableBinding<BindingProvenance>]) -> Vec<PolicyStatement> {
    let suppressed = wildcard_allows_suppressed(applicable);
    applicable
        .iter()
        .zip(suppressed)
        .filter(|(_, suppressed)| !suppressed)
        .flat_map(|(binding, _)| {
            binding
                .statements
                .iter()
                .filter(|statement| statement.effect == Effect::Allow)
                .cloned()
        })
        .collect()
}

fn merge_statements(left: &[PolicyStatement], right: &[PolicyStatement]) -> Vec<PolicyStatement> {
    let mut merged = left.to_vec();
    for statement in right {
        if !merged.contains(statement) {
            merged.push(statement.clone());
        }
    }
    merged
}

// -----------------------------------------------------------------------------
// Label helpers
// -----------------------------------------------------------------------------

/// Resolve one binding's subject against a selected label value, discarding a
/// value that does not parse. `None` therefore means "this binding names nobody
/// here", which is what makes an unresolvable value contribute no claim.
fn selected_subject(
    binding: &BindingFact,
    value: Option<&str>,
    organization: Option<&str>,
) -> Option<SubjectId> {
    let selector = binding.selector.as_ref()?;
    let value = value?;
    if selector
        .value
        .as_ref()
        .is_some_and(|wanted| wanted != value)
    {
        return None;
    }
    resolve_subject(&binding.subject, Some(value), organization).ok()
}

/// The value a chain supplies for a key without the leaf's own contribution —
/// what the leaf falls back to when its own label is removed.
fn inherited_value(nodes: &[ResourceNode], key: &str) -> Option<String> {
    nodes
        .iter()
        .take(nodes.len().saturating_sub(1))
        .filter_map(|node| node.labels.get(key))
        .next_back()
        .cloned()
}

/// What one affected resource's effective value for the key becomes.
///
/// The written resource's post-write effective value propagates verbatim: every
/// other affected resource is a K-inheriting descendant, which by definition sets
/// no value of its own and has no node between it and the written resource that
/// does — that is exactly the pruning condition
/// [`ResourceStore::label_inheriting_descendants`] enforces. So nearest-wins
/// resolves to the same value for all of them, including when the write removes
/// the key and leaves no value at all.
fn affected_after_value(after_effective: Option<&str>) -> Option<String> {
    after_effective.map(str::to_owned)
}

/// One resource's own scope: its leaf kind over its root-first name chain.
fn resource_scope(tree: &ResourceTree) -> Result<Scope, AuthorizationError> {
    let leaf = tree.leaf();
    let names: Vec<&str> = tree.nodes().iter().map(|node| node.name.as_str()).collect();
    format!(
        "{}/{}/{}",
        leaf.kind.group(),
        leaf.kind.kind(),
        names.join("/")
    )
    .parse()
    .map_err(AuthorizationError::from)
}

fn node_from_row(row: &ResourceRow) -> Result<ResourceNode, AuthorizationError> {
    Ok(ResourceNode {
        kind: row.resource_kind()?,
        name: row.name.clone(),
        labels: row.labels.clone(),
    })
}

fn require_subject_kind(
    subject: &SubjectId,
    kind: &str,
    what: &str,
) -> Result<(), AuthorizationError> {
    if subject.kind() != kind {
        return Err(AuthorizationError::invalid_input(format!(
            "{what} must be a {kind} subject, got {subject}"
        )));
    }
    Ok(())
}

fn role_placement_matches(binding: &BindingFact, change: &RoleBodyChange) -> bool {
    match change.kind {
        // A `PlatformRole` name always resolves at the root, from any tier.
        RoleRefKind::PlatformRole => true,
        // A bare `Role` name means the referencing binding's own Organization's
        // Role and never crosses that line (ADR-0001 §4).
        RoleRefKind::Role => matches!(
            (&binding.tier, &change.organization),
            (BindingTier::Organization(binding_org), Some(role_org)) if binding_org == role_org
        ),
    }
}

fn dedupe_claims(claims: Vec<GrantClaim>) -> Vec<GrantClaim> {
    let mut unique: Vec<GrantClaim> = Vec::new();
    for claim in claims {
        if !unique.contains(&claim) {
            unique.push(claim);
        }
    }
    unique
}
