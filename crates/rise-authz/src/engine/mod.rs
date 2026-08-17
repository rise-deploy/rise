//! Tier 1 — the live evaluation engine.
//!
//! This is ADR-0001 §4's algorithm over facts read from the resource store:
//! membership expansion, structural org-admin classification, binding
//! collection against `effectiveLabels`, wildcard replacement, Deny filtering
//! by placement tier, and the token ceiling. It knows what a binding is; it
//! knows nothing about Deployments, HTTP, or SQL.
//!
//! Every request builds one [`AuthorizationSnapshot`] from current database
//! facts. Decisions may be memoized against that snapshot, never across
//! requests: tightening a cap or narrowing a Role takes effect on the next
//! request, with nothing carried in a token.

mod bindings;
mod membership;
mod principal;
mod tree;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rise_resource_api::{
    Effect, KindMatcher, PolicyStatement, ResourceStore, RoleRefKind, StoreError, SubjectId,
    SubjectMembership, SubresourceMatcher, ValidationError, VerbMatcher, API_GROUP,
    ORGANIZATION_KIND,
};
use uuid::Uuid;

use crate::policy::{
    evaluate, resolve_subject, statement_matches, wildcard_allows_suppressed, ApplicableBinding,
    BindingTier, Decision, PermissionTuple,
};

pub use bindings::{BindingKind, BindingProvenance, RoleReference};
pub use membership::{MembershipResolver, PrincipalMembership};
pub use principal::{AuthenticatedPrincipal, AuthorizationCap, CapEntry, CapPermission};
pub use tree::{ResourceNode, ResourceTree};

use bindings::{load_organization_bindings, load_platform_bindings, BindingFact};

/// The `PlatformRole` whose exact org-root binding confers org-admin standing.
///
/// The predicate is structural: it reads this reference, the binding's
/// placement, and its scope — never the Role's current statements. An operator
/// editing the Role changes what admins may do, not who they are.
pub const ORG_ADMIN_PLATFORM_ROLE: &str = "org-admin";

#[derive(Debug, thiserror::Error)]
pub enum AuthorizationError {
    #[error("resource store error")]
    Store(#[from] StoreError),
    #[error("invalid authorization input: {0}")]
    InvalidInput(String),
    #[error("invalid principal: {0}")]
    InvalidPrincipal(String),
    #[error("corrupt stored policy: {0}")]
    CorruptPolicy(String),
    #[error("membership resolver contract violated: {0}")]
    Membership(String),
    #[error("resource {0} does not exist")]
    UnknownResource(Uuid),
}

impl AuthorizationError {
    pub(crate) fn invalid_target(message: impl Into<String>) -> Self {
        Self::InvalidInput(message.into())
    }

    pub(crate) fn invalid_principal(message: impl Into<String>) -> Self {
        Self::InvalidPrincipal(message.into())
    }

    pub(crate) fn corrupt_policy(message: impl Into<String>) -> Self {
        Self::CorruptPolicy(message.into())
    }
}

impl From<ValidationError> for AuthorizationError {
    fn from(error: ValidationError) -> Self {
        Self::InvalidInput(error.to_string())
    }
}

/// Why a binding that targets this caller still contributed nothing.
///
/// Only bindings the caller actually matches are recorded. A binding aimed at
/// someone else is not a diagnostic, and reporting every one of them would bury
/// the cases that answer "why don't I have access?".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InertReason {
    /// A dynamic subject template produced no valid concrete subject for this
    /// resource — a malformed or unresolvable label value.
    UnresolvedSubject,
    /// The resolved subject is not a member of the binding's own organization,
    /// so the org's grant cannot reach it (ADR-0001 §1 recipient boundary).
    RecipientBoundary,
    /// `subjectMembership: ResourceOrganization` excluded the resolved subject
    /// from this resource's organization.
    ResourceOrganization,
}

/// A binding that applies to the caller by scope, selector, and subject, but
/// grants nothing because a membership boundary removed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InertBinding {
    pub provenance: BindingProvenance,
    pub tier: BindingTier,
    pub reason: InertReason,
}

/// Why a collected statement does or does not survive into the decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retention {
    Retained,
    /// Wildcard replacement: a more specific applicable binding for the same
    /// authored subject and selector key superseded this Allow (ADR-0001 §1).
    SupersededAllow,
    /// A Deny the caller's tier exempts them from. Deny content is never
    /// dropped by replacement, so this is the only way a Deny stops applying.
    ExemptDeny(DenyExemption),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyExemption {
    /// Operators ignore every Deny, from any tier or subject. Hardcoded here
    /// rather than read from a row, so no lost or altered binding can lock the
    /// one tier with no recovery authority above it out of the platform.
    Operator,
    /// A current admin of the resource's own organization ignores that org's
    /// own Denies, and remains subject to every platform Deny.
    OrganizationAdmin,
}

/// One statement a binding contributed, with its placement provenance intact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contribution {
    pub provenance: BindingProvenance,
    pub tier: BindingTier,
    pub statement: PolicyStatement,
    pub retention: Retention,
}

/// A caller's complete authority over one resource.
///
/// This is the `EffectivePolicy` ADR-0001 §5 compares before and after an
/// authorization-changing write: the net decision after membership expansion,
/// replacement, tier filtering, Deny-wins evaluation, and the token ceiling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectivePolicy {
    operator: bool,
    organization: Option<String>,
    organization_admin: bool,
    contributions: Vec<Contribution>,
    /// `None` is an unrestricted credential; `Some` is the union of the token's
    /// authorization details that cover this resource.
    ceiling: Option<Vec<PolicyStatement>>,
    inert: Vec<InertBinding>,
}

impl EffectivePolicy {
    pub fn is_operator(&self) -> bool {
        self.operator
    }

    /// Whether the caller is a current admin of this resource's organization.
    pub fn is_organization_admin(&self) -> bool {
        self.organization_admin
    }

    pub fn organization(&self) -> Option<&str> {
        self.organization.as_deref()
    }

    pub fn contributions(&self) -> &[Contribution] {
        &self.contributions
    }

    pub fn inert_bindings(&self) -> &[InertBinding] {
        &self.inert
    }

    fn retained_statements(&self) -> impl Iterator<Item = &PolicyStatement> {
        self.contributions
            .iter()
            .filter(|contribution| contribution.retention == Retention::Retained)
            .map(|contribution| &contribution.statement)
    }

    /// This caller's RBAC authority on the resource as a statement list, for
    /// the policy comparisons ADR-0001 §5's grant gate performs.
    ///
    /// An operator's list is the complete main-resource and subresource
    /// permission, not the bindings that happened to match them — reading the
    /// matched bindings instead would understate an operator's authority and
    /// wrongly block grants they are entitled to hand out.
    ///
    /// The token ceiling is deliberately *not* folded in: it intersects with
    /// this set, and an intersection is not expressible as a flat statement
    /// list. A comparison that must respect the caller's ceiling has to apply
    /// [`Self::ceiling`] alongside this.
    pub fn effective_statements(&self) -> Vec<PolicyStatement> {
        if self.operator {
            return universal_allow();
        }
        self.retained_statements().cloned().collect()
    }

    pub fn ceiling(&self) -> Option<&[PolicyStatement]> {
        self.ceiling.as_deref()
    }

    /// Permit iff an Allow matches, no retained Deny matches, and the token
    /// ceiling admits the tuple.
    pub fn decide(&self, request: &PermissionTuple) -> Decision {
        let granted = if self.operator {
            // An operator holds every main-resource and registered-subresource
            // permission unconditionally. Their own request ignores every Deny,
            // including a cap they authored: a cap is a Deny binding, and only
            // an operator can place an instance-wide one, so honouring it here
            // would let them lock out everyone with no tier above to undo it.
            Decision::Allow
        } else {
            evaluate(self.retained_statements(), request)
        };
        if granted != Decision::Allow {
            return Decision::Deny;
        }
        match &self.ceiling {
            None => Decision::Allow,
            Some(ceiling) => evaluate(ceiling.iter(), request),
        }
    }

    /// The decision plus every contribution bearing on it, including Denies
    /// this caller's tier ignored.
    pub fn explain(&self, request: &PermissionTuple) -> Explanation {
        Explanation {
            decision: self.decide(request),
            operator_override: self.operator,
            ceiling_admits: self
                .ceiling
                .as_ref()
                .is_none_or(|ceiling| evaluate(ceiling.iter(), request) == Decision::Allow),
            contributions: self
                .contributions
                .iter()
                .filter(|contribution| statement_matches(&contribution.statement, request))
                .cloned()
                .collect(),
            inert: self.inert.clone(),
        }
    }
}

/// Why one request was allowed or denied, in terms a human can audit.
///
/// This names binding UIDs, Roles, and placement tiers the caller may hold no
/// read access to, so it is diagnostic output for an authorized reader, not
/// something to return on an ordinary denial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Explanation {
    pub decision: Decision,
    /// The caller is an operator, so no Deny reduced their access.
    pub operator_override: bool,
    /// Whether the token's own ceiling admits this tuple, independent of RBAC.
    pub ceiling_admits: bool,
    /// Every collected statement matching the requested tuple, with its binding
    /// UID, placement tier, and retention.
    pub contributions: Vec<Contribution>,
    pub inert: Vec<InertBinding>,
}

/// One candidate item of a collection request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListCandidate {
    pub uid: Uuid,
    pub node: ResourceNode,
}

/// Per-item read granularity. `list` and `get` are independent verbs, so an
/// item can be visible by name and label while its stored content is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListDecision {
    pub uid: Uuid,
    pub listable: bool,
    pub readable: bool,
}

/// The caller's standing in one organization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OrganizationStanding {
    /// A current Group tie, an org-native identity of that org, or the direct
    /// org-admin bootstrap binding.
    affiliated: bool,
    admin: bool,
}

/// One request's immutable authorization facts, plus the loads made against them.
///
/// Facts are read once per request and reused; nothing here outlives the
/// request. A cross-request cache would need a transactionally incremented
/// authorization epoch, which does not exist yet.
pub struct AuthorizationSnapshot {
    principal: AuthenticatedPrincipal,
    membership: PrincipalMembership,
    platform_bindings: Mutex<Option<Arc<Vec<BindingFact>>>>,
    organization_bindings: Mutex<HashMap<String, Arc<Vec<BindingFact>>>>,
    standing: Mutex<HashMap<String, OrganizationStanding>>,
}

impl AuthorizationSnapshot {
    pub fn principal(&self) -> &AuthenticatedPrincipal {
        &self.principal
    }

    pub fn membership(&self) -> &PrincipalMembership {
        &self.membership
    }

    /// Whether a live Group tie or an org-native identity places this principal
    /// in the organization, before any admin bootstrap edge is considered.
    fn has_native_affiliation(&self, organization: &str) -> bool {
        self.principal.subject().organization() == Some(organization)
            || self
                .membership
                .groups
                .iter()
                .any(|group| group.organization() == Some(organization))
    }
}

/// The evaluator. One instance per process; state lives in the snapshot.
pub struct AuthorizationEngine {
    store: Arc<dyn ResourceStore>,
    memberships: Arc<dyn MembershipResolver>,
}

impl AuthorizationEngine {
    pub fn new(store: Arc<dyn ResourceStore>, memberships: Arc<dyn MembershipResolver>) -> Self {
        Self { store, memberships }
    }

    /// Resolve this request's live membership facts.
    pub async fn snapshot(
        &self,
        principal: AuthenticatedPrincipal,
    ) -> Result<AuthorizationSnapshot, AuthorizationError> {
        let membership = self.memberships.resolve(&principal).await?;
        // The seam is implemented outside this crate and its answers widen
        // access, so its contract is checked rather than assumed. A tie is a
        // Group; anything else — an `org:` predicate, a ServiceAccount — would
        // silently confer org affiliation. And both ties and operator status
        // belong to Users alone: a GroupMembership names a User, and an
        // operator is an active User with a matching UserIdentity, so
        // accepting either for a workload identity would hand a token holder
        // authority no identity resource could express.
        if let Some(subject) = membership
            .groups
            .iter()
            .find(|subject| subject.kind() != "group")
        {
            return Err(AuthorizationError::Membership(format!(
                "{subject} is not a Group subject and cannot be a membership tie"
            )));
        }
        if !principal.is_user() && (membership.is_operator || !membership.groups.is_empty()) {
            return Err(AuthorizationError::Membership(format!(
                "{} is not a User and can hold neither Group ties nor operator status",
                principal.subject()
            )));
        }
        Ok(AuthorizationSnapshot {
            principal,
            membership,
            platform_bindings: Mutex::new(None),
            organization_bindings: Mutex::new(HashMap::new()),
            standing: Mutex::new(HashMap::new()),
        })
    }

    /// Load an existing resource's evaluation target from its structural chain.
    ///
    /// Tombstoned rows are included, matching `ResourceStore::ancestors`:
    /// whether a resource being collected is still addressable is the API
    /// layer's decision, not an authorization one. Note that an Organization
    /// tombstone takes its own bindings with it, so org-tier Denies stop
    /// applying to what remains of its subtree while it drains.
    pub async fn resource_tree(&self, uid: Uuid) -> Result<ResourceTree, AuthorizationError> {
        let rows = self.store.ancestors(uid).await?;
        if rows.is_empty() {
            return Err(AuthorizationError::UnknownResource(uid));
        }
        ResourceTree::from_rows(&rows)
    }

    pub async fn authorize(
        &self,
        snapshot: &AuthorizationSnapshot,
        target: &ResourceTree,
        request: &PermissionTuple,
    ) -> Result<Decision, AuthorizationError> {
        Ok(self
            .effective_policy(snapshot, target)
            .await?
            .decide(request))
    }

    pub async fn explain(
        &self,
        snapshot: &AuthorizationSnapshot,
        target: &ResourceTree,
        request: &PermissionTuple,
    ) -> Result<Explanation, AuthorizationError> {
        Ok(self
            .effective_policy(snapshot, target)
            .await?
            .explain(request))
    }

    /// Filter a collection per item (ADR-0001 §4).
    ///
    /// Every item runs the complete algorithm against its own effective labels;
    /// a collection is never authorized scope-wide. Items sharing one parent
    /// share one ancestry, so this costs no additional tree reads.
    pub async fn filter_list(
        &self,
        snapshot: &AuthorizationSnapshot,
        ancestors: &[ResourceNode],
        candidates: &[ListCandidate],
    ) -> Result<Vec<ListDecision>, AuthorizationError> {
        let mut decisions = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            let target = ResourceTree::with_leaf(ancestors, candidate.node.clone());
            let policy = self.effective_policy(snapshot, &target).await?;
            let kind = candidate.node.kind.clone();
            decisions.push(ListDecision {
                uid: candidate.uid,
                listable: policy.decide(&PermissionTuple {
                    verb: rise_resource_api::Verb::List,
                    kind: kind.clone(),
                    subresource: None,
                }) == Decision::Allow,
                readable: policy.decide(&PermissionTuple {
                    verb: rise_resource_api::Verb::Get,
                    kind,
                    subresource: None,
                }) == Decision::Allow,
            });
        }
        Ok(decisions)
    }

    /// Steps 1–4 of ADR-0001 §4 for one resource: expand, collect, replace, and
    /// filter Denies by tier. The tuple only enters at [`EffectivePolicy::decide`].
    pub async fn effective_policy(
        &self,
        snapshot: &AuthorizationSnapshot,
        target: &ResourceTree,
    ) -> Result<EffectivePolicy, AuthorizationError> {
        let organization = target.organization().map(str::to_owned);
        let labels = target.effective_labels();

        // Both sides are snapshot-cached, so a collection request pays for this
        // load once and then evaluates every item against the same facts.
        let platform = self.platform_bindings(snapshot).await?;
        let organization_tier = match &organization {
            Some(organization) => Some(self.organization_bindings(snapshot, organization).await?),
            None => None,
        };
        let candidates = platform
            .iter()
            .chain(organization_tier.iter().flat_map(|tier| tier.iter()));

        let mut applicable: Vec<ApplicableBinding<BindingProvenance>> = Vec::new();
        let mut inert = Vec::new();
        for binding in candidates {
            if !target.covered_by(&binding.scope) {
                continue;
            }
            let selected_value = match &binding.selector {
                None => None,
                Some(selector) => {
                    let Some(value) = labels.get(selector.key.as_ref()) else {
                        continue;
                    };
                    if selector
                        .value
                        .as_ref()
                        .is_some_and(|wanted| wanted != value)
                    {
                        continue;
                    }
                    Some(value.as_str())
                }
            };

            // A template resolves against this resource's own organization, so
            // `group:platform` on an acme resource is acme's Group, never a
            // same-named Group elsewhere. Invalid values fail closed.
            let Ok(subject) =
                resolve_subject(&binding.subject, selected_value, organization.as_deref())
            else {
                inert.push(InertBinding {
                    provenance: binding.provenance.clone(),
                    tier: binding.tier.clone(),
                    reason: InertReason::UnresolvedSubject,
                });
                continue;
            };

            if !self.subject_matches_caller(snapshot, &subject).await? {
                continue;
            }
            if let Some(reason) = self
                .membership_boundary(snapshot, binding, &subject, organization.as_deref())
                .await?
            {
                inert.push(InertBinding {
                    provenance: binding.provenance.clone(),
                    tier: binding.tier.clone(),
                    reason,
                });
                continue;
            }

            applicable.push(ApplicableBinding {
                provenance: binding.provenance.clone(),
                subject: binding.subject.clone(),
                scope: binding.scope.clone(),
                selector: binding.selector.clone(),
                tier: binding.tier.clone(),
                statements: binding.statements.clone(),
            });
        }

        let operator = snapshot.membership.is_operator;
        let organization_admin = match &organization {
            Some(organization) => self.standing(snapshot, organization).await?.admin,
            None => false,
        };

        let suppressed = wildcard_allows_suppressed(&applicable);
        let mut contributions = Vec::new();
        for (binding, suppressed) in applicable.iter().zip(suppressed) {
            for statement in &binding.statements {
                let retention = match statement.effect {
                    Effect::Deny if operator => Retention::ExemptDeny(DenyExemption::Operator),
                    Effect::Deny
                        if organization_admin
                            && matches!(&binding.tier, BindingTier::Organization(org)
                                if Some(org.as_str()) == organization.as_deref()) =>
                    {
                        Retention::ExemptDeny(DenyExemption::OrganizationAdmin)
                    }
                    Effect::Allow if suppressed => Retention::SupersededAllow,
                    _ => Retention::Retained,
                };
                contributions.push(Contribution {
                    provenance: binding.provenance.clone(),
                    tier: binding.tier.clone(),
                    statement: statement.clone(),
                    retention,
                });
            }
        }

        Ok(EffectivePolicy {
            operator,
            organization,
            organization_admin,
            contributions,
            ceiling: snapshot.principal.authorization_cap().ceiling_for(target),
            inert,
        })
    }

    /// Membership expansion (§4 step 1): does this binding's resolved subject
    /// name the caller, one of their live Groups, or a virtual subject they
    /// currently satisfy?
    async fn subject_matches_caller(
        &self,
        snapshot: &AuthorizationSnapshot,
        subject: &SubjectId,
    ) -> Result<bool, AuthorizationError> {
        if subject == snapshot.principal.subject() {
            return Ok(true);
        }
        Ok(match subject.kind() {
            "group" => snapshot.membership.groups.contains(subject),
            "org" => self.standing(snapshot, subject.name()).await?.affiliated,
            "system" => match subject.name() {
                "authenticated" => true,
                "operators" => snapshot.membership.is_operator,
                _ => false,
            },
            _ => false,
        })
    }

    /// The org recipient boundary and `subjectMembership`, both of which turn a
    /// syntactically applicable binding into inert policy data.
    async fn membership_boundary(
        &self,
        snapshot: &AuthorizationSnapshot,
        binding: &BindingFact,
        subject: &SubjectId,
        resource_organization: Option<&str>,
    ) -> Result<Option<InertReason>, AuthorizationError> {
        if let BindingTier::Organization(binding_organization) = &binding.tier {
            if !self
                .subject_belongs_to(snapshot, subject, binding_organization)
                .await?
            {
                return Ok(Some(InertReason::RecipientBoundary));
            }
        }
        if binding.subject_membership == SubjectMembership::ResourceOrganization {
            // A Group, ServiceAccount, or `org:` subject already carries its
            // organization, and a root-scoped target lies in none, so the
            // constraint only bites on subjects that span organizations.
            let spans_organizations = matches!(subject.kind(), "user" | "controller")
                || subject.as_ref() == "system:authenticated";
            if let (Some(organization), true) = (resource_organization, spans_organizations) {
                if !self
                    .subject_belongs_to(snapshot, subject, organization)
                    .await?
                {
                    return Ok(Some(InertReason::ResourceOrganization));
                }
            }
        }
        Ok(None)
    }

    /// Whether the caller, reached through this subject, is a live member of
    /// the organization.
    ///
    /// The subject has already been matched to the caller, so a `user:` or
    /// `system:authenticated` subject is tested against the caller's own
    /// affiliation. A Controller belongs to no organization and never matches.
    async fn subject_belongs_to(
        &self,
        snapshot: &AuthorizationSnapshot,
        subject: &SubjectId,
        organization: &str,
    ) -> Result<bool, AuthorizationError> {
        Ok(match subject.kind() {
            "group" | "serviceaccount" | "org" => subject.organization() == Some(organization),
            "user" => self.standing(snapshot, organization).await?.affiliated,
            "system" if subject.name() == "authenticated" => {
                self.standing(snapshot, organization).await?.affiliated
            }
            _ => false,
        })
    }

    /// Org affiliation and org-admin classification, memoized per organization.
    async fn standing(
        &self,
        snapshot: &AuthorizationSnapshot,
        organization: &str,
    ) -> Result<OrganizationStanding, AuthorizationError> {
        if let Some(standing) = snapshot
            .standing
            .lock()
            .expect("snapshot standing cache")
            .get(organization)
        {
            return Ok(*standing);
        }

        let native = snapshot.has_native_affiliation(organization);
        let mut admin = false;
        let mut bootstrap = false;
        if snapshot.principal.is_user() {
            for binding in self
                .organization_bindings(snapshot, organization)
                .await?
                .iter()
                .filter(|binding| qualifies_as_org_admin(binding, organization))
            {
                let Some(subject) = binding.subject.literal() else {
                    continue;
                };
                if subject == snapshot.principal.subject() {
                    admin = true;
                    // The bootstrap edge: a direct admin binding is itself the
                    // first administrator's affiliation with a new org, so
                    // governing one never requires inventing a Group first.
                    bootstrap = true;
                } else if snapshot.membership.groups.contains(subject) {
                    admin = true;
                }
            }
        }

        let standing = OrganizationStanding {
            affiliated: native || bootstrap,
            admin,
        };
        snapshot
            .standing
            .lock()
            .expect("snapshot standing cache")
            .insert(organization.to_owned(), standing);
        Ok(standing)
    }

    async fn platform_bindings(
        &self,
        snapshot: &AuthorizationSnapshot,
    ) -> Result<Arc<Vec<BindingFact>>, AuthorizationError> {
        if let Some(bindings) = snapshot
            .platform_bindings
            .lock()
            .expect("snapshot platform binding cache")
            .clone()
        {
            return Ok(bindings);
        }
        let bindings = Arc::new(load_platform_bindings(self.store.as_ref()).await?);
        *snapshot
            .platform_bindings
            .lock()
            .expect("snapshot platform binding cache") = Some(bindings.clone());
        Ok(bindings)
    }

    async fn organization_bindings(
        &self,
        snapshot: &AuthorizationSnapshot,
        organization: &str,
    ) -> Result<Arc<Vec<BindingFact>>, AuthorizationError> {
        if let Some(bindings) = snapshot
            .organization_bindings
            .lock()
            .expect("snapshot organization binding cache")
            .get(organization)
        {
            return Ok(bindings.clone());
        }
        let bindings =
            Arc::new(load_organization_bindings(self.store.as_ref(), organization).await?);
        snapshot
            .organization_bindings
            .lock()
            .expect("snapshot organization binding cache")
            .insert(organization.to_owned(), bindings.clone());
        Ok(bindings)
    }
}

/// The policy ADR-0001 §1 guarantees every operator request, as data.
///
/// Two statements, because `kinds: "*"` covers the main resource only: a Role
/// that needs both writes both (§3), and `PlatformRole/system-admin` is defined
/// exactly this way.
fn universal_allow() -> Vec<PolicyStatement> {
    vec![
        PolicyStatement {
            effect: Effect::Allow,
            kinds: KindMatcher::All,
            verbs: VerbMatcher::All,
            subresources: None,
        },
        PolicyStatement {
            effect: Effect::Allow,
            kinds: KindMatcher::All,
            verbs: VerbMatcher::All,
            subresources: Some(SubresourceMatcher::All),
        },
    ]
}

/// ADR-0001 §5's structural org-admin predicate.
///
/// Exact org-root placement, no selector, and a `PlatformRole/org-admin`
/// reference. A label-selected binding can therefore never confer admin
/// standing, which is what keeps admin access independent of any
/// access-driving label an orphaned resource might carry.
fn qualifies_as_org_admin(binding: &BindingFact, organization: &str) -> bool {
    binding.selector.is_none()
        && binding.provenance.role.kind == RoleRefKind::PlatformRole
        && binding.provenance.role.name == ORG_ADMIN_PLATFORM_ROLE
        && matches!(&binding.tier, BindingTier::Organization(org) if org == organization)
        && binding.scope.as_ref() == format!("{API_GROUP}/{ORGANIZATION_KIND}/{organization}")
        && binding.subject.literal().is_some_and(|subject| {
            match subject.kind() {
                // The direct edge: §5's binding may name the User itself, and
                // that binding is also that first administrator's affiliation
                // with the org.
                "user" => true,
                // A Group carries its own organization, so §1's recipient
                // boundary settles this before membership is consulted: a
                // foreign Group is provably not a member of this org and its
                // binding grants nothing — admin standing included. Without
                // this, an admin could promote another org's Group by naming
                // it, and its members would inherit the Deny exemption that
                // admin standing carries.
                "group" => subject.organization() == Some(organization),
                _ => false,
            }
        })
}
