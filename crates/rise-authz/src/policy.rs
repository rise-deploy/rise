use std::collections::BTreeSet;

use rise_resource_api::{
    BindingSubject, Effect, KindMatcher, LabelSelector, PolicyStatement, ResourceKind,
    ResourceKindPattern, Scope, SubjectId, SubjectRef, SubresourceMatcher, SubresourceName, Verb,
    VerbMatcher,
};

/// One version-independent authorization operation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PermissionTuple {
    pub verb: Verb,
    pub kind: ResourceKind,
    /// `None` is the main resource. A named value is a distinct subresource.
    pub subresource: Option<SubresourceName>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
}

/// Evaluate an additive statement union. Default deny applies, and any matching
/// Deny overrides every matching Allow.
pub fn evaluate<'a>(
    statements: impl IntoIterator<Item = &'a PolicyStatement>,
    request: &PermissionTuple,
) -> Decision {
    let mut allowed = false;
    for statement in statements {
        if !statement_matches(statement, request) {
            continue;
        }
        match statement.effect {
            Effect::Allow => allowed = true,
            Effect::Deny => return Decision::Deny,
        }
    }
    if allowed {
        Decision::Allow
    } else {
        Decision::Deny
    }
}

pub fn statement_matches(statement: &PolicyStatement, request: &PermissionTuple) -> bool {
    kind_matches(&statement.kinds, &request.kind)
        && verb_matches(&statement.verbs, request.verb)
        && subresource_matches(
            statement.subresources.as_ref(),
            request.subresource.as_ref(),
        )
}

pub fn kind_matches(matcher: &KindMatcher, kind: &ResourceKind) -> bool {
    match matcher {
        KindMatcher::All => true,
        KindMatcher::Patterns(patterns) => patterns.iter().any(|pattern| match pattern {
            ResourceKindPattern::Exact(exact) => exact == kind,
            ResourceKindPattern::Group(group) => group == kind.group(),
        }),
    }
}

pub fn verb_matches(matcher: &VerbMatcher, verb: Verb) -> bool {
    match matcher {
        VerbMatcher::All => true,
        VerbMatcher::Verbs(verbs) => verbs.contains(&verb),
    }
}

pub fn subresource_matches(
    matcher: Option<&SubresourceMatcher>,
    subresource: Option<&SubresourceName>,
) -> bool {
    match (matcher, subresource) {
        (None, None) => true,
        (None, Some(_)) | (Some(_), None) => false,
        (Some(SubresourceMatcher::All), Some(_)) => true,
        (Some(SubresourceMatcher::Names(names)), Some(name)) => names.contains(name),
    }
}

/// Resolve one authored subject against the value selected from a resource.
/// Literal subjects ignore `selected_value`; templates fail closed without it.
pub fn resolve_subject(
    subject: &BindingSubject,
    selected_value: Option<&str>,
    resource_organization: Option<&str>,
) -> Result<SubjectId, rise_resource_api::ValidationError> {
    match subject {
        BindingSubject::Literal(subject) => Ok(subject.clone()),
        BindingSubject::UserNameTemplate => {
            let name = required_selected_value(selected_value)?;
            format!("user:{name}").parse()
        }
        BindingSubject::GroupNameTemplate => {
            let name = required_selected_value(selected_value)?;
            let organization = resource_organization.ok_or_else(|| {
                rise_resource_api::ValidationError::new(
                    "relative group subject requires an organization",
                )
            })?;
            format!("group:{organization}/{name}").parse()
        }
        BindingSubject::SubjectRefTemplate => {
            let subject_ref: SubjectRef = required_selected_value(selected_value)?.parse()?;
            subject_ref.resolve(resource_organization)
        }
    }
}

fn required_selected_value(
    value: Option<&str>,
) -> Result<&str, rise_resource_api::ValidationError> {
    value.ok_or_else(|| {
        rise_resource_api::ValidationError::new("dynamic subject requires a selected label value")
    })
}

/// The placement tier that supplied a statement. Tier-1 decides which Denies
/// survive for a live caller; Tier-0 carries this fact without interpreting
/// membership or administrator status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingTier {
    Platform,
    Organization(String),
}

/// A binding already found applicable to the resource being evaluated.
///
/// `apply_wildcard_replacement` deliberately consumes already-applicable
/// bindings. This makes value-specific selectors compete only on resources
/// whose current effective label matched them, as ADR-0001 requires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicableBinding<P> {
    pub provenance: P,
    pub subject: BindingSubject,
    pub scope: Scope,
    pub selector: Option<LabelSelector>,
    pub tier: BindingTier,
    pub statements: Vec<PolicyStatement>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementContribution<P> {
    pub provenance: P,
    pub tier: BindingTier,
    pub statement: PolicyStatement,
}

/// Replace wildcard Allows when an applicable specific binding has the same
/// authored subject and selector key. Denies are never removed, and every
/// surviving statement retains binding and placement provenance.
pub fn apply_wildcard_replacement<P: Clone>(
    bindings: &[ApplicableBinding<P>],
) -> Vec<StatementContribution<P>> {
    let suppressed = wildcard_allows_suppressed(bindings);
    let mut contributions = Vec::new();
    for (binding, suppressed) in bindings.iter().zip(suppressed) {
        for statement in &binding.statements {
            if suppressed && statement.effect == Effect::Allow {
                continue;
            }
            contributions.push(StatementContribution {
                provenance: binding.provenance.clone(),
                tier: binding.tier.clone(),
                statement: statement.clone(),
            });
        }
    }
    contributions
}

/// Per-binding wildcard-replacement verdict, positionally aligned with
/// `bindings`: `true` where a more specific applicable binding supersedes that
/// binding's Allow content.
///
/// [`apply_wildcard_replacement`] is this predicate applied; callers that must
/// report *why* an Allow contributed nothing read it directly rather than
/// re-deriving the comparison.
pub fn wildcard_allows_suppressed<P>(bindings: &[ApplicableBinding<P>]) -> Vec<bool> {
    bindings
        .iter()
        .map(|binding| {
            bindings
                .iter()
                .any(|candidate| replaces(candidate, binding))
        })
        .collect()
}

fn selector_key(selector: Option<&LabelSelector>) -> Option<&rise_resource_api::LabelKey> {
    selector.map(|selector| &selector.key)
}

/// The ADR-0001 §1 wildcard-replacement pairing: `specific` drops
/// `wildcard`'s Allow content on every resource where both are applicable —
/// same authored subject, same selector key, `wildcard`'s scope is `*` and
/// `specific`'s is not.
///
/// Extracted from [`wildcard_allows_suppressed`], which is this predicate
/// applied pairwise; its behavior is unchanged by the extraction.
pub fn replaces<P>(specific: &ApplicableBinding<P>, wildcard: &ApplicableBinding<P>) -> bool {
    wildcard.scope.is_wildcard()
        && !specific.scope.is_wildcard()
        && specific.subject == wildcard.subject
        && selector_key(specific.selector.as_ref()) == selector_key(wildcard.selector.as_ref())
}

/// Whether `deny` matches every tuple `allow` does.
///
/// Implemented by turning `deny`'s matchers into an Allow and asking whether
/// it dominates `allow`'s tuples through the same representative-based subset
/// machinery [`policy_is_subset`] uses, so the answer agrees with [`evaluate`]
/// by construction rather than by a second, possibly-diverging comparison.
/// Either statement having the wrong effect for its position (a `Deny` asked
/// to cover, or an `Allow` asked to be covered) answers `false`.
pub fn deny_covers_allow(deny: &PolicyStatement, allow: &PolicyStatement) -> bool {
    if deny.effect != Effect::Deny || allow.effect != Effect::Allow {
        return false;
    }
    let deny_as_allow = PolicyStatement {
        effect: Effect::Allow,
        kinds: deny.kinds.clone(),
        verbs: deny.verbs.clone(),
        subresources: deny.subresources.clone(),
    };
    policy_is_subset(
        std::slice::from_ref(allow),
        std::slice::from_ref(&deny_as_allow),
    )
}

/// Whether every principal `covered` can match is one `covering` also
/// matches, decided from the two subjects' authored forms alone.
///
/// Only two shapes are decidable this way, both definitional in ADR-0001 §1:
/// `system:authenticated` reaches every subject, and `org:<o>` reaches every
/// org-native (`group:`/`serviceaccount:`) subject naming `o`. A `user:`
/// subject is only ever *contingently* a member of an org — deciding that
/// needs a live membership lookup this function deliberately has no access
/// to — so `org:<o>` never covers one here. A dynamic template is authored
/// per binding and is covered by, or covers, nothing but an identical
/// template.
pub fn subject_covers(covering: &BindingSubject, covered: &BindingSubject) -> bool {
    if covering == covered {
        return true;
    }
    let Some(covering) = covering.literal() else {
        return false;
    };
    if covering.as_ref() == "system:authenticated" {
        return true;
    }
    if covering.kind() != "org" {
        return false;
    }
    let Some(covered) = covered.literal() else {
        return false;
    };
    matches!(covered.kind(), "group" | "serviceaccount")
        && covered.organization() == Some(covering.name())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyDomain {
    pub scope: Scope,
    pub selector: Option<LabelSelector>,
}

/// Whether `covering` contains every resource domain represented by `covered`.
///
/// Scope comparison is deliberately fail-closed without registry facts:
/// wildcard covers every scope, while concrete scopes cover only the identical
/// canonical path. Selector order is none > key-exists > key-equals-value;
/// different keys are never assumed disjoint.
pub fn domain_covers(covering: &PolicyDomain, covered: &PolicyDomain) -> bool {
    domain_covers_with(covering, covered, &|left, right| left == right)
}

/// Domain containment with registry-resolved concrete-scope ancestry supplied
/// by the caller. The callback is consulted only for two non-wildcard scopes;
/// Tier-0 remains independent of the resource registry and store.
pub fn domain_covers_with<F>(
    covering: &PolicyDomain,
    covered: &PolicyDomain,
    concrete_scope_covers: &F,
) -> bool
where
    F: Fn(&Scope, &Scope) -> bool + ?Sized,
{
    let scope_covers = if covering.scope.is_wildcard() {
        true
    } else if covered.scope.is_wildcard() {
        false
    } else {
        concrete_scope_covers(&covering.scope, &covered.scope)
    };
    scope_covers && selector_covers(covering.selector.as_ref(), covered.selector.as_ref())
}

pub fn selector_covers(covering: Option<&LabelSelector>, covered: Option<&LabelSelector>) -> bool {
    match (covering, covered) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(left), Some(right)) if left.key != right.key => false,
        (Some(LabelSelector { value: None, .. }), Some(_)) => true,
        (Some(left), Some(right)) => left.value == right.value,
    }
}

/// Whether two domains provably share no resource.
///
/// This is the complement the grant gate needs on the *restricting* side: an
/// Allow only counts when its domain demonstrably covers the target, while a
/// Deny counts unless its domain demonstrably misses it. Both directions
/// therefore have to be positively provable, and everything unproven is treated
/// as overlapping — dropping a Deny that might apply would turn an unprovable
/// scope relationship into extra authority.
///
/// Only two facts prove disjointness: same-key selectors pinned to different
/// values, and two concrete scopes neither of which covers the other. The
/// callback answers the latter for the caller's registry, exactly as in
/// [`domain_covers_with`]; a wildcard scope is never disjoint from anything.
pub fn domains_provably_disjoint_with<F>(
    left: &PolicyDomain,
    right: &PolicyDomain,
    concrete_scope_covers: &F,
) -> bool
where
    F: Fn(&Scope, &Scope) -> bool + ?Sized,
{
    if selectors_provably_disjoint(left.selector.as_ref(), right.selector.as_ref()) {
        return true;
    }
    if left.scope.is_wildcard() || right.scope.is_wildcard() {
        return false;
    }
    !concrete_scope_covers(&left.scope, &right.scope)
        && !concrete_scope_covers(&right.scope, &left.scope)
}

/// Two selectors that provably never match the same resource.
///
/// A resource holds one effective value per key, so the same key pinned to two
/// different values cannot both match. Different keys are independent and are
/// never assumed disjoint, matching [`selector_covers`]'s fail-closed ordering.
fn selectors_provably_disjoint(
    left: Option<&LabelSelector>,
    right: Option<&LabelSelector>,
) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => {
            left.key == right.key
                && matches!((&left.value, &right.value),
                    (Some(left), Some(right)) if left != right)
        }
        _ => false,
    }
}

/// Test the tuple-level implication `(after - before) ⊆ writer` exactly over
/// the closed matcher grammar. The implementation uses one representative for
/// every equivalence class induced by exact/group/global kind patterns and
/// named/wildcard subresource patterns; it does not enumerate stored resources.
pub fn newly_allowed_is_subset(
    before: &[PolicyStatement],
    after: &[PolicyStatement],
    writer: &[PolicyStatement],
) -> bool {
    unjustified_new_tuples(before, after, writer).is_empty()
}

/// The tuples `after` newly allows that neither `before` already allowed nor
/// `writer` holds — the witnesses of a failed grant-gate check.
///
/// [`newly_allowed_is_subset`] is this emptied of its evidence. The gate reports
/// these back so a rejection can name the authority the writer is missing
/// instead of only that the write was refused. One representative per
/// equivalence class means the list identifies every distinct class that
/// failed, not every concrete kind the platform happens to have registered.
pub fn unjustified_new_tuples(
    before: &[PolicyStatement],
    after: &[PolicyStatement],
    writer: &[PolicyStatement],
) -> Vec<PermissionTuple> {
    unjustified_new_tuples_under_ceiling(before, after, writer, None)
}

/// The delta check with the writer's credential ceiling applied.
///
/// A ceiling intersects with the writer's RBAC authority rather than adding to
/// it (ADR-0001 §7), and an intersection is not expressible as a flat statement
/// list — so it is applied here, tuple-wise, where the comparison already
/// enumerates one representative per equivalence class. `None` is an
/// unrestricted credential.
///
/// Omitting the ceiling from a grant comparison would let a capped writer
/// delegate authority their own token cannot exercise, so every gate comparison
/// goes through this.
pub fn unjustified_new_tuples_under_ceiling(
    before: &[PolicyStatement],
    after: &[PolicyStatement],
    writer: &[PolicyStatement],
    ceiling: Option<&[PolicyStatement]>,
) -> Vec<PermissionTuple> {
    let policies = [before, after, writer, ceiling.unwrap_or(&[])];
    tuple_representatives(policies)
        .into_iter()
        .filter(|tuple| {
            let writer_holds = evaluate(writer, tuple) == Decision::Allow
                && ceiling.is_none_or(|ceiling| evaluate(ceiling, tuple) == Decision::Allow);
            evaluate(after, tuple) == Decision::Allow
                && evaluate(before, tuple) != Decision::Allow
                && !writer_holds
        })
        .collect()
}

pub fn policy_is_subset(candidate: &[PolicyStatement], covering: &[PolicyStatement]) -> bool {
    newly_allowed_is_subset(&[], candidate, covering)
}

/// Combine domain movement with the Deny-aware tuple delta check.
///
/// `None` before represents creation. Callers must always supply the fully
/// resolved aggregate after-policy over the affected domain, including for a
/// binding deletion: removing a Deny may expose another binding's Allow and is
/// therefore a grant. If the old domain covers the new domain, only the
/// tuple-level policy delta is new. Otherwise the move/expansion may reach
/// resources that had no prior policy, so the writer must cover the complete
/// after policy and domain.
pub fn scoped_newly_allowed_is_subset(
    before: Option<(&[PolicyStatement], &PolicyDomain)>,
    after: (&[PolicyStatement], &PolicyDomain),
    writer: &[PolicyStatement],
    writer_domain: &PolicyDomain,
) -> bool {
    scoped_newly_allowed_is_subset_with(before, after, writer, writer_domain, &|left, right| {
        left == right
    })
}

/// Complete scoped delta check using caller-supplied concrete-scope ancestry.
pub fn scoped_newly_allowed_is_subset_with<F>(
    before: Option<(&[PolicyStatement], &PolicyDomain)>,
    after: (&[PolicyStatement], &PolicyDomain),
    writer: &[PolicyStatement],
    writer_domain: &PolicyDomain,
    concrete_scope_covers: &F,
) -> bool
where
    F: Fn(&Scope, &Scope) -> bool + ?Sized,
{
    let (after_policy, after_domain) = after;
    if policy_is_subset(after_policy, &[]) {
        return true;
    }

    let Some((before_policy, before_domain)) = before else {
        return domain_covers_with(writer_domain, after_domain, concrete_scope_covers)
            && policy_is_subset(after_policy, writer);
    };
    if !domain_covers_with(before_domain, after_domain, concrete_scope_covers) {
        return domain_covers_with(writer_domain, after_domain, concrete_scope_covers)
            && policy_is_subset(after_policy, writer);
    }
    if newly_allowed_is_subset(before_policy, after_policy, &[]) {
        return true;
    }
    domain_covers_with(writer_domain, after_domain, concrete_scope_covers)
        && newly_allowed_is_subset(before_policy, after_policy, writer)
}

fn tuple_representatives<'a>(
    policies: impl IntoIterator<Item = &'a [PolicyStatement]>,
) -> Vec<PermissionTuple> {
    let policies: Vec<&[PolicyStatement]> = policies.into_iter().collect();
    let mut kinds = BTreeSet::new();
    let mut groups = BTreeSet::new();
    let mut subresources = BTreeSet::new();

    for statement in policies.iter().flat_map(|policy| policy.iter()) {
        if let KindMatcher::Patterns(patterns) = &statement.kinds {
            for pattern in patterns {
                groups.insert(pattern.group().to_owned());
                if let ResourceKindPattern::Exact(kind) = pattern {
                    kinds.insert(kind.clone());
                }
            }
        }
        if let Some(SubresourceMatcher::Names(names)) = &statement.subresources {
            subresources.extend(names.iter().cloned());
        }
    }

    for group in groups {
        kinds.insert(probe_kind(&group, &kinds));
    }
    let unknown_group = probe_group(&kinds);
    kinds.insert(ResourceKind::new(&unknown_group, "PolicySubsetProbe").expect("valid probe kind"));
    subresources.insert(probe_subresource(&subresources));

    const VERBS: [Verb; 6] = [
        Verb::Get,
        Verb::List,
        Verb::Create,
        Verb::Update,
        Verb::Delete,
        Verb::Use,
    ];
    let mut tuples = Vec::new();
    for verb in VERBS {
        for kind in &kinds {
            tuples.push(PermissionTuple {
                verb,
                kind: kind.clone(),
                subresource: None,
            });
            for subresource in &subresources {
                tuples.push(PermissionTuple {
                    verb,
                    kind: kind.clone(),
                    subresource: Some(subresource.clone()),
                });
            }
        }
    }
    tuples
}

fn probe_kind(group: &str, existing: &BTreeSet<ResourceKind>) -> ResourceKind {
    for suffix in 0.. {
        let kind = if suffix == 0 {
            "PolicySubsetProbe".to_owned()
        } else {
            format!("PolicySubsetProbe{suffix}")
        };
        let candidate = ResourceKind::new(group, &kind).expect("validated source group");
        if !existing.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

fn probe_group(existing: &BTreeSet<ResourceKind>) -> String {
    for suffix in 0.. {
        let group = if suffix == 0 {
            "policy-subset-probe.invalid".to_owned()
        } else {
            format!("policy-subset-probe-{suffix}.invalid")
        };
        if existing.iter().all(|kind| kind.group() != group) {
            return group;
        }
    }
    unreachable!()
}

fn probe_subresource(existing: &BTreeSet<SubresourceName>) -> SubresourceName {
    for suffix in 0.. {
        let name = if suffix == 0 {
            "policy-subset-probe".to_owned()
        } else {
            format!("policy-subset-probe-{suffix}")
        };
        let candidate: SubresourceName = name.parse().expect("valid probe subresource");
        if !existing.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

#[cfg(test)]
mod shadowing_tests {
    use super::*;
    use rise_resource_api::ResourceKindPattern;

    fn kind(group: &str, kind: &str) -> ResourceKind {
        ResourceKind::new(group, kind).expect("valid kind")
    }

    fn kinds(patterns: &[ResourceKind]) -> KindMatcher {
        KindMatcher::Patterns(
            patterns
                .iter()
                .cloned()
                .map(ResourceKindPattern::Exact)
                .collect(),
        )
    }

    fn verbs(vs: &[Verb]) -> VerbMatcher {
        VerbMatcher::Verbs(vs.iter().copied().collect())
    }

    fn allow_statement(kinds: KindMatcher, verbs: VerbMatcher) -> PolicyStatement {
        PolicyStatement {
            effect: Effect::Allow,
            kinds,
            verbs,
            subresources: None,
        }
    }

    fn deny_statement(kinds: KindMatcher, verbs: VerbMatcher) -> PolicyStatement {
        PolicyStatement {
            effect: Effect::Deny,
            kinds,
            verbs,
            subresources: None,
        }
    }

    #[test]
    fn deny_covers_allow_when_deny_is_wildcard() {
        let deny = deny_statement(KindMatcher::All, VerbMatcher::All);
        let allow = allow_statement(kinds(&[kind("rise.dev", "Project")]), verbs(&[Verb::Get]));
        assert!(deny_covers_allow(&deny, &allow));
    }

    #[test]
    fn deny_covers_allow_exact_match() {
        let matcher = kinds(&[kind("rise.dev", "Project")]);
        let deny = deny_statement(matcher.clone(), verbs(&[Verb::Get, Verb::List]));
        let allow = allow_statement(matcher, verbs(&[Verb::Get]));
        assert!(deny_covers_allow(&deny, &allow));
    }

    #[test]
    fn deny_covers_allow_false_when_deny_is_narrower_on_verbs() {
        let deny = deny_statement(KindMatcher::All, verbs(&[Verb::Get]));
        let allow = allow_statement(KindMatcher::All, verbs(&[Verb::Get, Verb::List]));
        assert!(!deny_covers_allow(&deny, &allow));
    }

    #[test]
    fn deny_covers_allow_false_when_deny_is_narrower_on_kinds() {
        let deny = deny_statement(kinds(&[kind("rise.dev", "Project")]), VerbMatcher::All);
        let allow = allow_statement(KindMatcher::All, VerbMatcher::All);
        assert!(!deny_covers_allow(&deny, &allow));
    }

    #[test]
    fn deny_covers_allow_requires_the_expected_effect_on_each_side() {
        let statement = allow_statement(KindMatcher::All, VerbMatcher::All);
        // Passing an Allow where a Deny is expected never covers, regardless
        // of how permissive it is.
        assert!(!deny_covers_allow(&statement, &statement));
    }

    fn literal(subject: &str) -> BindingSubject {
        BindingSubject::Literal(subject.parse().expect("valid subject"))
    }

    #[test]
    fn subject_covers_identical_subjects() {
        let subject = literal("user:alice");
        assert!(subject_covers(&subject, &subject));
        assert!(subject_covers(
            &BindingSubject::UserNameTemplate,
            &BindingSubject::UserNameTemplate
        ));
    }

    #[test]
    fn subject_covers_system_authenticated_covers_every_subject() {
        let authenticated = literal("system:authenticated");
        assert!(subject_covers(&authenticated, &literal("user:alice")));
        assert!(subject_covers(
            &authenticated,
            &literal("group:acme/platform")
        ));
        assert!(subject_covers(
            &authenticated,
            &literal("controller:reconciler")
        ));
        assert!(subject_covers(
            &authenticated,
            &BindingSubject::SubjectRefTemplate
        ));
    }

    #[test]
    fn subject_covers_org_covers_its_own_group_and_service_account() {
        let org = literal("org:acme");
        assert!(subject_covers(&org, &literal("group:acme/platform")));
        assert!(subject_covers(&org, &literal("serviceaccount:acme/ci")));
    }

    #[test]
    fn subject_covers_org_does_not_cover_a_user() {
        // A `user:` subject is only ever contingently a member of an org, so
        // `org:<o>` never covers one from the authored forms alone.
        let org = literal("org:acme");
        assert!(!subject_covers(&org, &literal("user:alice")));
    }

    #[test]
    fn subject_covers_org_does_not_cover_a_different_org() {
        let org = literal("org:acme");
        assert!(!subject_covers(&org, &literal("group:other-org/platform")));
    }

    #[test]
    fn subject_covers_a_template_covers_nothing_but_itself() {
        assert!(!subject_covers(
            &BindingSubject::UserNameTemplate,
            &literal("user:alice")
        ));
        assert!(!subject_covers(
            &BindingSubject::UserNameTemplate,
            &BindingSubject::GroupNameTemplate
        ));
    }

    fn applicable_binding(
        subject: BindingSubject,
        scope: &str,
        selector: Option<LabelSelector>,
    ) -> ApplicableBinding<()> {
        ApplicableBinding {
            provenance: (),
            subject,
            scope: scope.parse().expect("valid scope"),
            selector,
            tier: BindingTier::Platform,
            statements: Vec::new(),
        }
    }

    fn selector(key: &str) -> LabelSelector {
        LabelSelector {
            key: key.parse().expect("valid label key"),
            value: None,
        }
    }

    #[test]
    fn replaces_same_subject_and_selector_key() {
        let subject = literal("user:alice");
        let wildcard = applicable_binding(subject.clone(), "*", Some(selector("rise.dev/team")));
        let specific = applicable_binding(
            subject,
            "rise.dev/Project/acme/app",
            Some(selector("rise.dev/team")),
        );
        assert!(replaces(&specific, &wildcard));
    }

    #[test]
    fn replaces_true_with_no_selector_on_either_side() {
        let subject = literal("user:alice");
        let wildcard = applicable_binding(subject.clone(), "*", None);
        let specific = applicable_binding(subject, "rise.dev/Project/acme/app", None);
        assert!(replaces(&specific, &wildcard));
    }

    #[test]
    fn replaces_false_on_different_selector_keys() {
        let subject = literal("user:alice");
        let wildcard = applicable_binding(subject.clone(), "*", Some(selector("rise.dev/team")));
        let specific = applicable_binding(
            subject,
            "rise.dev/Project/acme/app",
            Some(selector("rise.dev/other")),
        );
        assert!(!replaces(&specific, &wildcard));
    }

    #[test]
    fn replaces_false_on_different_subjects() {
        let wildcard =
            applicable_binding(literal("user:alice"), "*", Some(selector("rise.dev/team")));
        let specific = applicable_binding(
            literal("user:bob"),
            "rise.dev/Project/acme/app",
            Some(selector("rise.dev/team")),
        );
        assert!(!replaces(&specific, &wildcard));
    }

    #[test]
    fn replaces_false_when_wildcard_argument_is_not_wildcard_scoped() {
        let subject = literal("user:alice");
        let not_wildcard = applicable_binding(subject.clone(), "rise.dev/Project/acme/app", None);
        let specific = applicable_binding(subject, "rise.dev/Project/acme/app2", None);
        assert!(!replaces(&specific, &not_wildcard));
    }

    #[test]
    fn replaces_false_when_specific_argument_is_also_wildcard_scoped() {
        let subject = literal("user:alice");
        let wildcard = applicable_binding(subject.clone(), "*", None);
        let also_wildcard = applicable_binding(subject, "*", None);
        assert!(!replaces(&also_wildcard, &wildcard));
    }
}
