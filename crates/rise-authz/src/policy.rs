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
            binding.scope.is_wildcard()
                && bindings.iter().any(|candidate| {
                    !candidate.scope.is_wildcard()
                        && candidate.subject == binding.subject
                        && selector_key(candidate.selector.as_ref())
                            == selector_key(binding.selector.as_ref())
                })
        })
        .collect()
}

fn selector_key(selector: Option<&LabelSelector>) -> Option<&rise_resource_api::LabelKey> {
    selector.map(|selector| &selector.key)
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

/// Test the tuple-level implication `(after - before) ⊆ writer` exactly over
/// the closed matcher grammar. The implementation uses one representative for
/// every equivalence class induced by exact/group/global kind patterns and
/// named/wildcard subresource patterns; it does not enumerate stored resources.
pub fn newly_allowed_is_subset(
    before: &[PolicyStatement],
    after: &[PolicyStatement],
    writer: &[PolicyStatement],
) -> bool {
    let representatives = tuple_representatives([before, after, writer]);
    representatives.into_iter().all(|tuple| {
        evaluate(after, &tuple) != Decision::Allow
            || evaluate(before, &tuple) == Decision::Allow
            || evaluate(writer, &tuple) == Decision::Allow
    })
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
