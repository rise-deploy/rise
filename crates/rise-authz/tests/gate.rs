//! ADR-0001 §5/§6.6 write-time grant gate, against fakes.
//!
//! Covers the appendix's grant-gate and ownership scenarios that are decidable
//! without a database: 29–32, 34, and 39–43. Scenario 33 (serializable with
//! revocation) is a property of the transaction the gate runs inside, not of the
//! gate, and lands with the choke point that opens one.

mod support;

use std::sync::Arc;

use rise_authz::engine::{
    AuthenticatedPrincipal, AuthorizationCap, AuthorizationChange, AuthorizationEngine,
    AuthorizationSnapshot, BindingChange, BindingKind, BindingState, CapEntry, CapPermission,
    GateOutcome, GroupMembershipChange, IdentityMappingChange, LabelChange, RejectionReason,
    ResourceNode, ResourceTree, RoleBodyChange, RoleReference, ORG_ADMIN_PLATFORM_ROLE,
};
use rise_resource_api::{
    BindingSubject, KindMatcher, PolicyStatement, ResourceStore, RoleRefKind, SubjectMembership,
    Verb, VerbMatcher,
};
use serde_json::json;
use support::{FakeMemberships, FakeStore, StoreBuilder};
use uuid::Uuid;

const ORGANIZATION: &str = "Organization";
const PROJECT: &str = "Project";
const ENVIRONMENT: &str = "Environment";
const ROLE: &str = "Role";
const ROLE_BINDING: &str = "RoleBinding";
const PLATFORM_ROLE: &str = "PlatformRole";
const PLATFORM_ROLE_BINDING: &str = "PlatformRoleBinding";
const OWNER_KEY: &str = "rise.dev/owner";

// -----------------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------------

fn engine(store: Arc<FakeStore>, memberships: Arc<FakeMemberships>) -> AuthorizationEngine {
    AuthorizationEngine::new(store as Arc<dyn ResourceStore>, memberships)
}

fn principal(subject: &str) -> AuthenticatedPrincipal {
    AuthenticatedPrincipal::new(
        subject.parse().expect("valid subject"),
        Uuid::new_v4(),
        AuthorizationCap::Unrestricted,
    )
    .expect("principal kind")
}

/// A credential whose ceiling admits only `verbs`, over every kind. Keeping the
/// kind matcher wide isolates the verb narrowing, which is what the ceiling test
/// is about.
fn capped_principal(subject: &str, scope: &str, verbs: &[&str]) -> AuthenticatedPrincipal {
    AuthenticatedPrincipal::new(
        subject.parse().expect("valid subject"),
        Uuid::new_v4(),
        AuthorizationCap::Restricted(vec![CapEntry {
            scope: scope.parse().expect("valid scope"),
            permissions: vec![CapPermission {
                verbs: VerbMatcher::Verbs(
                    verbs
                        .iter()
                        .map(|verb| serde_json::from_value(json!(verb)).expect("valid verb"))
                        .collect(),
                ),
                kinds: KindMatcher::All,
                subresources: None,
            }],
        }]),
    )
    .expect("principal kind")
}

async fn snapshot(
    engine: &AuthorizationEngine,
    principal: AuthenticatedPrincipal,
) -> AuthorizationSnapshot {
    engine.snapshot(principal).await.expect("snapshot builds")
}

async fn gate(
    engine: &AuthorizationEngine,
    snapshot: &AuthorizationSnapshot,
    change: AuthorizationChange,
) -> GateOutcome {
    engine
        .evaluate_grant(snapshot, &change)
        .await
        .expect("gate evaluates")
}

fn statements(value: serde_json::Value) -> Vec<PolicyStatement> {
    serde_json::from_value(value).expect("valid statements")
}

fn allow_all() -> serde_json::Value {
    json!([
        { "effect": "Allow", "kinds": "*", "verbs": "*" },
        { "effect": "Allow", "kinds": "*", "verbs": "*", "subresources": "*" }
    ])
}

/// An org `RoleBinding` satisfying §5's exact structural admin predicate.
fn org_admin_spec(subject: &str, organization: &str) -> serde_json::Value {
    json!({
        "subject": subject,
        "scope": format!("rise.dev/Organization/{organization}"),
        "roleRef": { "kind": "PlatformRole", "name": ORG_ADMIN_PLATFORM_ROLE }
    })
}

fn org_admin_state(subject: &str, organization: &str) -> BindingState {
    BindingState {
        uid: Uuid::new_v4(),
        kind: BindingKind::RoleBinding,
        organization: Some(organization.to_owned()),
        subject: subject.parse().expect("valid subject"),
        subject_membership: SubjectMembership::Any,
        scope: format!("rise.dev/Organization/{organization}")
            .parse()
            .expect("valid scope"),
        selector: None,
        role: RoleReference {
            kind: RoleRefKind::PlatformRole,
            name: ORG_ADMIN_PLATFORM_ROLE.to_owned(),
        },
    }
}

fn missing_verbs(outcome: &GateOutcome) -> Vec<String> {
    let mut verbs: Vec<String> = outcome
        .rejections
        .iter()
        .flat_map(|rejection| match &rejection.reason {
            RejectionReason::InsufficientAuthority(tuples) => tuples
                .iter()
                .map(|tuple| {
                    serde_json::to_value(tuple.verb)
                        .expect("verb serializes")
                        .as_str()
                        .expect("verb is a string")
                        .to_owned()
                })
                .collect::<Vec<_>>(),
            RejectionReason::OperatorStandingRequired => vec!["operator".to_owned()],
        })
        .collect();
    verbs.sort();
    verbs.dedup();
    verbs
}

// -----------------------------------------------------------------------------
// Scenario 29 — net delta respects platform Deny
// -----------------------------------------------------------------------------

/// A capped admin may appoint another admin covered by the same platform Deny.
///
/// The denied tuple appears in neither effective policy, so it is never
/// delegated — and the appointment is not blocked by a cap the writer shares
/// with the appointee.
#[tokio::test]
async fn capped_admin_may_appoint_an_equally_capped_admin() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    builder.role(PLATFORM_ROLE, ORG_ADMIN_PLATFORM_ROLE, None, allow_all());
    builder.role(
        PLATFORM_ROLE,
        "no-project-delete",
        None,
        json!([{ "effect": "Deny", "kinds": ["rise.dev/Project"], "verbs": ["delete"] }]),
    );
    // The platform ceiling on everyone affiliated with acme, admins included.
    builder.binding(
        PLATFORM_ROLE_BINDING,
        "acme-cap",
        None,
        json!({
            "subject": "org:acme",
            "subjectMembership": "Any",
            "scope": "rise.dev/Organization/acme",
            "roleRef": { "kind": "PlatformRole", "name": "no-project-delete" }
        }),
    );
    builder.binding(
        ROLE_BINDING,
        "alice-admin",
        Some(acme),
        org_admin_spec("user:alice", "acme"),
    );
    let store = builder.build();
    let engine = engine(store, FakeMemberships::none());
    let alice = snapshot(&engine, principal("user:alice")).await;

    let outcome = gate(
        &engine,
        &alice,
        AuthorizationChange::Binding(BindingChange {
            before: None,
            after: Some(Box::new(org_admin_state("user:bob", "acme"))),
        }),
    )
    .await;

    assert!(
        outcome.permitted(),
        "an admin under a platform Deny may appoint an admin under the same Deny, got {:?}",
        outcome.rejections
    );
    // The appointment really was gated — a claim was compared, not skipped.
    assert_eq!(outcome.claims.len(), 1, "one recipient, one domain");
}

/// The same appointment is refused when the writer's own authority is narrower
/// than what admin standing confers.
#[tokio::test]
async fn a_narrow_writer_cannot_appoint_an_admin() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    builder.role(PLATFORM_ROLE, ORG_ADMIN_PLATFORM_ROLE, None, allow_all());
    builder.role(
        ROLE,
        "project-reader",
        Some(acme),
        json!([{ "effect": "Allow", "kinds": ["rise.dev/Project"], "verbs": ["get"] }]),
    );
    builder.binding(
        ROLE_BINDING,
        "carol-reader",
        Some(acme),
        json!({
            "subject": "user:carol",
            "scope": "rise.dev/Organization/acme",
            "roleRef": { "kind": "Role", "name": "project-reader" }
        }),
    );
    let store = builder.build();
    let engine = engine(store, FakeMemberships::none());
    let carol = snapshot(&engine, principal("user:carol")).await;

    let outcome = gate(
        &engine,
        &carol,
        AuthorizationChange::Binding(BindingChange {
            before: None,
            after: Some(Box::new(org_admin_state("user:bob", "acme"))),
        }),
    )
    .await;

    assert!(
        !outcome.permitted(),
        "a reader cannot mint an administrator"
    );
    assert!(
        missing_verbs(&outcome).contains(&"delete".to_owned()),
        "the rejection names authority the writer lacks, got {:?}",
        missing_verbs(&outcome)
    );
}

// -----------------------------------------------------------------------------
// Scenario 30 — Role edits span bindings; an unbound Role grants nothing
// -----------------------------------------------------------------------------

/// Widening an unbound Role creates no authority, so it needs none.
#[tokio::test]
async fn widening_an_unbound_role_is_ungated() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    builder.role(ROLE, "orphan", Some(acme), json!([]));
    let store = builder.build();
    let engine = engine(store, FakeMemberships::none());
    let nobody = snapshot(&engine, principal("user:nobody")).await;

    let outcome = gate(
        &engine,
        &nobody,
        AuthorizationChange::RoleBody(RoleBodyChange {
            kind: RoleRefKind::Role,
            name: "orphan".to_owned(),
            organization: Some("acme".to_owned()),
            before: Vec::new(),
            after: statements(allow_all()),
        }),
    )
    .await;

    assert!(outcome.permitted());
    assert!(
        outcome.claims.is_empty(),
        "no binding delivers the body, so there is nothing to compare"
    );
}

/// Widening a *bound* Role is gated once per recipient and domain its bindings
/// deliver it to.
#[tokio::test]
async fn widening_a_bound_role_is_gated_per_binding() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    builder.resource(PROJECT, "web", Some(acme));
    builder.role(
        ROLE,
        "viewer",
        Some(acme),
        json!([{ "effect": "Allow", "kinds": ["rise.dev/Project"], "verbs": ["get"] }]),
    );
    builder.binding(
        ROLE_BINDING,
        "devs-viewer",
        Some(acme),
        json!({
            "subject": "group:acme/devs",
            "scope": "rise.dev/Organization/acme",
            "roleRef": { "kind": "Role", "name": "viewer" }
        }),
    );
    builder.binding(
        ROLE_BINDING,
        "ops-viewer",
        Some(acme),
        json!({
            "subject": "group:acme/ops",
            "scope": "rise.dev/Project/acme/web",
            "roleRef": { "kind": "Role", "name": "viewer" }
        }),
    );
    let store = builder.build();
    let engine = engine(store, FakeMemberships::none());
    let nobody = snapshot(&engine, principal("user:nobody")).await;

    let outcome = gate(
        &engine,
        &nobody,
        AuthorizationChange::RoleBody(RoleBodyChange {
            kind: RoleRefKind::Role,
            name: "viewer".to_owned(),
            organization: Some("acme".to_owned()),
            before: statements(
                json!([{ "effect": "Allow", "kinds": ["rise.dev/Project"], "verbs": ["get"] }]),
            ),
            after: statements(json!([
                { "effect": "Allow", "kinds": ["rise.dev/Project"], "verbs": ["get", "delete"] }
            ])),
        }),
    )
    .await;

    assert_eq!(
        outcome.claims.len(),
        2,
        "both bindings' recipients and domains are compared, got {:?}",
        outcome.claims
    );
    assert!(!outcome.permitted());
    assert_eq!(
        outcome.rejections.len(),
        2,
        "a writer holding nothing fails both"
    );
}

// -----------------------------------------------------------------------------
// Scenario 31 — deleting a Deny is a grant
// -----------------------------------------------------------------------------

#[tokio::test]
async fn deleting_a_deny_binding_is_gated_as_a_grant() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    builder.role(ROLE, "project-admin", Some(acme), allow_all());
    builder.role(
        ROLE,
        "no-delete",
        Some(acme),
        json!([{ "effect": "Deny", "kinds": ["rise.dev/Project"], "verbs": ["delete"] }]),
    );
    builder.binding(
        ROLE_BINDING,
        "devs-admin",
        Some(acme),
        json!({
            "subject": "group:acme/devs",
            "scope": "rise.dev/Organization/acme",
            "roleRef": { "kind": "Role", "name": "project-admin" }
        }),
    );
    let cap_uid = builder.binding(
        ROLE_BINDING,
        "devs-cap",
        Some(acme),
        json!({
            "subject": "group:acme/devs",
            "scope": "rise.dev/Organization/acme",
            "roleRef": { "kind": "Role", "name": "no-delete" }
        }),
    );
    let store = builder.build();
    let engine = engine(store, FakeMemberships::none());
    let nobody = snapshot(&engine, principal("user:nobody")).await;

    let cap_state = BindingState {
        uid: cap_uid,
        kind: BindingKind::RoleBinding,
        organization: Some("acme".to_owned()),
        subject: "group:acme/devs".parse().expect("valid subject"),
        subject_membership: SubjectMembership::Any,
        scope: "rise.dev/Organization/acme".parse().expect("valid scope"),
        selector: None,
        role: RoleReference {
            kind: RoleRefKind::Role,
            name: "no-delete".to_owned(),
        },
    };
    let outcome = gate(
        &engine,
        &nobody,
        AuthorizationChange::Binding(BindingChange {
            before: Some(Box::new(cap_state)),
            after: None,
        }),
    )
    .await;

    assert!(
        !outcome.permitted(),
        "removing the Deny exposes the admin Role's delete and must be gated"
    );
    assert!(
        missing_verbs(&outcome).contains(&"delete".to_owned()),
        "the exposed verb is named, got {:?}",
        missing_verbs(&outcome)
    );
}

// -----------------------------------------------------------------------------
// Scenario 32 — scope and selector containment are exact
// -----------------------------------------------------------------------------

/// Authority over one Project cannot justify a grant across the Organization.
#[tokio::test]
async fn narrow_scope_cannot_justify_a_broader_grant() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    builder.resource(PROJECT, "web", Some(acme));
    builder.role(ROLE, "project-admin", Some(acme), allow_all());
    builder.binding(
        ROLE_BINDING,
        "dana-web-admin",
        Some(acme),
        json!({
            "subject": "user:dana",
            "scope": "rise.dev/Project/acme/web",
            "roleRef": { "kind": "Role", "name": "project-admin" }
        }),
    );
    let store = builder.build();
    let engine = engine(store, FakeMemberships::none());
    let dana = snapshot(&engine, principal("user:dana")).await;

    // Granting the same Role at the Organization reaches every Project, which
    // Dana's single-Project authority cannot cover.
    let org_wide = BindingState {
        uid: Uuid::new_v4(),
        kind: BindingKind::RoleBinding,
        organization: Some("acme".to_owned()),
        subject: "user:erin".parse().expect("valid subject"),
        subject_membership: SubjectMembership::Any,
        scope: "rise.dev/Organization/acme".parse().expect("valid scope"),
        selector: None,
        role: RoleReference {
            kind: RoleRefKind::Role,
            name: "project-admin".to_owned(),
        },
    };
    let widened = gate(
        &engine,
        &dana,
        AuthorizationChange::Binding(BindingChange {
            before: None,
            after: Some(Box::new(org_wide.clone())),
        }),
    )
    .await;
    assert!(
        !widened.permitted(),
        "one Project's admin cannot grant across the whole Organization"
    );

    // The same grant confined to the Project Dana administers is within her
    // authority, which is what makes the containment check exact rather than
    // merely restrictive.
    let project_wide = BindingState {
        scope: "rise.dev/Project/acme/web".parse().expect("valid scope"),
        ..org_wide
    };
    let confined = gate(
        &engine,
        &dana,
        AuthorizationChange::Binding(BindingChange {
            before: None,
            after: Some(Box::new(project_wide)),
        }),
    )
    .await;
    assert!(
        confined.permitted(),
        "delegation inside her own scope is permitted, got {:?}",
        confined.rejections
    );
}

/// A Project-scoped writer covers a grant on a resource *below* that Project:
/// containment follows the registered parent chain, not string equality.
#[tokio::test]
async fn scope_containment_follows_the_registered_parent_chain() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    let web = builder.resource(PROJECT, "web", Some(acme));
    builder.resource(ENVIRONMENT, "prod", Some(web));
    builder.role(ROLE, "project-admin", Some(acme), allow_all());
    builder.binding(
        ROLE_BINDING,
        "dana-web-admin",
        Some(acme),
        json!({
            "subject": "user:dana",
            "scope": "rise.dev/Project/acme/web",
            "roleRef": { "kind": "Role", "name": "project-admin" }
        }),
    );
    let store = builder.build();
    let engine = engine(store, FakeMemberships::none());
    let dana = snapshot(&engine, principal("user:dana")).await;

    let environment_scoped = BindingState {
        uid: Uuid::new_v4(),
        kind: BindingKind::RoleBinding,
        organization: Some("acme".to_owned()),
        subject: "user:erin".parse().expect("valid subject"),
        subject_membership: SubjectMembership::Any,
        scope: "rise.dev/Environment/acme/web/prod"
            .parse()
            .expect("valid scope"),
        selector: None,
        role: RoleReference {
            kind: RoleRefKind::Role,
            name: "project-admin".to_owned(),
        },
    };
    let outcome = gate(
        &engine,
        &dana,
        AuthorizationChange::Binding(BindingChange {
            before: None,
            after: Some(Box::new(environment_scoped)),
        }),
    )
    .await;
    assert!(
        outcome.permitted(),
        "Project/acme/web contains Environment/acme/web/prod, got {:?}",
        outcome.rejections
    );
}

/// Relaxing `subjectMembership` from `ResourceOrganization` to `Any` widens the
/// binding's reach and is therefore gated, even with the Role untouched.
#[tokio::test]
async fn relaxing_the_membership_clamp_is_a_grant() {
    let mut builder = StoreBuilder::new();
    builder.resource(ORGANIZATION, "acme", None);
    builder.role(PLATFORM_ROLE, "reader", None, allow_all());
    let binding_uid = builder.binding(
        PLATFORM_ROLE_BINDING,
        "erin-reader",
        None,
        json!({
            "subject": "user:erin",
            "subjectMembership": "ResourceOrganization",
            "scope": "rise.dev/Organization/acme",
            "roleRef": { "kind": "PlatformRole", "name": "reader" }
        }),
    );
    let store = builder.build();
    let engine = engine(store, FakeMemberships::none());
    let nobody = snapshot(&engine, principal("user:nobody")).await;

    let clamped = BindingState {
        uid: binding_uid,
        kind: BindingKind::PlatformRoleBinding,
        organization: None,
        subject: "user:erin".parse().expect("valid subject"),
        subject_membership: SubjectMembership::ResourceOrganization,
        scope: "rise.dev/Organization/acme".parse().expect("valid scope"),
        selector: None,
        role: RoleReference {
            kind: RoleRefKind::PlatformRole,
            name: "reader".to_owned(),
        },
    };
    let relaxed = BindingState {
        subject_membership: SubjectMembership::Any,
        ..clamped.clone()
    };

    let outcome = gate(
        &engine,
        &nobody,
        AuthorizationChange::Binding(BindingChange {
            before: Some(Box::new(clamped)),
            after: Some(Box::new(relaxed)),
        }),
    )
    .await;
    assert!(
        !outcome.permitted(),
        "the clamped before-policy does not cover the unclamped after-policy"
    );
}

/// An org Role body change that omits its Organization is refused, not silently
/// ungated. Loading no org tier would match no binding and produce no claims,
/// which is the one direction a gate must never fail in.
#[tokio::test]
async fn an_org_role_change_without_its_organization_fails_closed() {
    let store = StoreBuilder::new().build();
    let engine = engine(store, FakeMemberships::none());
    let nobody = snapshot(&engine, principal("user:nobody")).await;

    let error = engine
        .evaluate_grant(
            &nobody,
            &AuthorizationChange::RoleBody(RoleBodyChange {
                kind: RoleRefKind::Role,
                name: "viewer".to_owned(),
                organization: None,
                before: Vec::new(),
                after: statements(allow_all()),
            }),
        )
        .await
        .expect_err("a Role change with no Organization cannot be evaluated");
    assert!(
        error.to_string().contains("Organization"),
        "the error names what is missing, got {error}"
    );
}

/// A Deny whose scope names an unregistered kind is retained, not dropped.
///
/// `covers` answers "no" both for a scope that genuinely misses another and for
/// one the registry cannot resolve, and only the first is evidence of
/// disjointness. Treating the second as disjoint would silently remove a
/// restriction from the writer — and the state is reachable, since deleting a
/// `ResourceDefinition` does not delete bindings whose scope named that kind.
#[tokio::test]
async fn a_deny_scoped_to_an_unregistered_kind_still_limits_the_writer() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    builder.role(ROLE, "project-admin", Some(acme), allow_all());
    builder.role(
        ROLE,
        "no-delete",
        Some(acme),
        json!([{ "effect": "Deny", "kinds": "*", "verbs": ["delete"] }]),
    );
    builder.binding(
        ROLE_BINDING,
        "tess-admin",
        Some(acme),
        json!({
            "subject": "user:tess",
            "scope": "rise.dev/Organization/acme",
            "roleRef": { "kind": "Role", "name": "project-admin" }
        }),
    );
    // `Gadget` is not a registered kind, so this scope resolves to nothing.
    builder.binding(
        ROLE_BINDING,
        "tess-cap",
        Some(acme),
        json!({
            "subject": "user:tess",
            "scope": "rise.dev/Gadget/acme/thing",
            "roleRef": { "kind": "Role", "name": "no-delete" }
        }),
    );
    let store = builder.build();
    let engine = engine(store, FakeMemberships::none());
    let tess = snapshot(&engine, principal("user:tess")).await;

    let outcome = gate(
        &engine,
        &tess,
        AuthorizationChange::Binding(BindingChange {
            before: None,
            after: Some(Box::new(BindingState {
                uid: Uuid::new_v4(),
                kind: BindingKind::RoleBinding,
                organization: Some("acme".to_owned()),
                subject: "user:umar".parse().expect("valid subject"),
                subject_membership: SubjectMembership::Any,
                scope: "rise.dev/Organization/acme".parse().expect("valid scope"),
                selector: None,
                role: RoleReference {
                    kind: RoleRefKind::Role,
                    name: "project-admin".to_owned(),
                },
            })),
        }),
    )
    .await;

    assert!(
        !outcome.permitted(),
        "the unresolvable-scope Deny must still count against the writer"
    );
    assert_eq!(missing_verbs(&outcome), vec!["delete".to_owned()]);
}

// -----------------------------------------------------------------------------
// Scenario 34 — identity mapping is a grant
// -----------------------------------------------------------------------------

/// A new credential path to a User makes that User's whole policy reachable,
/// Group-derived authority included.
#[tokio::test]
async fn creating_an_identity_mapping_requires_the_targets_whole_policy() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    builder.role(ROLE, "project-admin", Some(acme), allow_all());
    builder.binding(
        ROLE_BINDING,
        "devs-admin",
        Some(acme),
        json!({
            "subject": "group:acme/devs",
            "scope": "rise.dev/Organization/acme",
            "roleRef": { "kind": "Role", "name": "project-admin" }
        }),
    );
    let store = builder.build();
    let memberships = FakeMemberships::none().with_user_groups("user:frank", &["group:acme/devs"]);
    let engine = engine(store, memberships);
    let nobody = snapshot(&engine, principal("user:nobody")).await;

    let outcome = gate(
        &engine,
        &nobody,
        AuthorizationChange::IdentityMapping(IdentityMappingChange {
            identity: "user:frank".parse().expect("valid subject"),
            confers_operator: false,
        }),
    )
    .await;

    assert!(
        !outcome.permitted(),
        "Frank's Group grants are reachable through the new mapping and must be held by the writer"
    );
    assert!(
        outcome.claims.iter().any(|claim| claim.recipient
            == BindingSubject::Literal("group:acme/devs".parse().expect("valid subject"))),
        "the Group tie is part of what the mapping reaches, got {:?}",
        outcome.claims
    );
}

/// Only an operator may create a mapping that confers operator standing: there
/// is no tier above the recovery tier to undo the mistake.
#[tokio::test]
async fn an_operator_conferring_mapping_requires_an_operator() {
    let store = StoreBuilder::new().build();
    let engine = engine(store, FakeMemberships::none());
    let nobody = snapshot(&engine, principal("user:nobody")).await;

    let outcome = gate(
        &engine,
        &nobody,
        AuthorizationChange::IdentityMapping(IdentityMappingChange {
            identity: "user:frank".parse().expect("valid subject"),
            confers_operator: true,
        }),
    )
    .await;
    assert_eq!(
        outcome
            .rejections
            .iter()
            .map(|rejection| rejection.reason.clone())
            .collect::<Vec<_>>(),
        vec![RejectionReason::OperatorStandingRequired]
    );
}

#[tokio::test]
async fn an_operator_passes_every_gate() {
    let store = StoreBuilder::new().build();
    let engine = engine(store, FakeMemberships::operator());
    let operator = snapshot(&engine, principal("user:root")).await;

    let outcome = gate(
        &engine,
        &operator,
        AuthorizationChange::IdentityMapping(IdentityMappingChange {
            identity: "user:frank".parse().expect("valid subject"),
            confers_operator: true,
        }),
    )
    .await;
    assert!(outcome.permitted());
}

// -----------------------------------------------------------------------------
// GroupMembership
// -----------------------------------------------------------------------------

#[tokio::test]
async fn adding_a_group_membership_delegates_the_groups_authority() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    builder.role(
        ROLE,
        "project-deleter",
        Some(acme),
        json!([{ "effect": "Allow", "kinds": ["rise.dev/Project"], "verbs": ["delete"] }]),
    );
    builder.role(
        ROLE,
        "project-reader",
        Some(acme),
        json!([{ "effect": "Allow", "kinds": ["rise.dev/Project"], "verbs": ["get"] }]),
    );
    builder.binding(
        ROLE_BINDING,
        "devs-deleter",
        Some(acme),
        json!({
            "subject": "group:acme/devs",
            "scope": "rise.dev/Organization/acme",
            "roleRef": { "kind": "Role", "name": "project-deleter" }
        }),
    );
    builder.binding(
        ROLE_BINDING,
        "gail-reader",
        Some(acme),
        json!({
            "subject": "user:gail",
            "scope": "rise.dev/Organization/acme",
            "roleRef": { "kind": "Role", "name": "project-reader" }
        }),
    );
    let store = builder.build();
    let engine = engine(store, FakeMemberships::none());
    let gail = snapshot(&engine, principal("user:gail")).await;

    let outcome = gate(
        &engine,
        &gail,
        AuthorizationChange::GroupMembership(GroupMembershipChange {
            user: "user:henry".parse().expect("valid subject"),
            group: "group:acme/devs".parse().expect("valid subject"),
        }),
    )
    .await;

    assert!(
        !outcome.permitted(),
        "a reader cannot add someone to a Group that can delete Projects"
    );
    assert_eq!(missing_verbs(&outcome), vec!["delete".to_owned()]);
}

/// Scenario 42's last clause: a membership write that *activates* a dynamic
/// ownership grant passes the ordinary effective-delta gate.
///
/// The grant is not delivered by any binding naming the Group — it comes from the
/// seeded `${ref.subject}` binding, whose authored subject is a template. Looking
/// only at literal-subject bindings would wave this through.
#[tokio::test]
async fn joining_a_group_that_owns_resources_is_gated() {
    let mut builder = StoreBuilder::new();
    seeded_ownership(&mut builder);
    let acme = builder.resource(ORGANIZATION, "acme", None);
    builder.labeled(PROJECT, "web", Some(acme), &[(OWNER_KEY, "group:platform")]);
    let store = builder.build();
    // Mallory is not in the owning Group, so she holds none of what joining it
    // would confer.
    let engine = engine(store, FakeMemberships::groups(&["group:acme/bystanders"]));
    let mallory = snapshot(&engine, principal("user:mallory")).await;

    let outcome = gate(
        &engine,
        &mallory,
        AuthorizationChange::GroupMembership(GroupMembershipChange {
            user: "user:victim".parse().expect("valid subject"),
            group: "group:acme/platform".parse().expect("valid subject"),
        }),
    )
    .await;

    assert!(
        !outcome.permitted(),
        "joining a Group that owns resources delegates resource-owner over them"
    );
    let mut verbs = missing_verbs(&outcome);
    verbs.sort();
    assert_eq!(
        verbs,
        vec![
            "delete".to_owned(),
            "get".to_owned(),
            "list".to_owned(),
            "update".to_owned()
        ],
        "the whole of resource-owner is what the write hands over"
    );
}

/// A current member of the owning Group may add another member: they already hold
/// exactly what joining confers, so the subset check passes.
#[tokio::test]
async fn a_member_of_an_owning_group_may_add_another_member() {
    let mut builder = StoreBuilder::new();
    seeded_ownership(&mut builder);
    let acme = builder.resource(ORGANIZATION, "acme", None);
    builder.labeled(PROJECT, "web", Some(acme), &[(OWNER_KEY, "group:platform")]);
    let store = builder.build();
    let engine = engine(store, FakeMemberships::groups(&["group:acme/platform"]));
    let owner = snapshot(&engine, principal("user:olive")).await;

    let outcome = gate(
        &engine,
        &owner,
        AuthorizationChange::GroupMembership(GroupMembershipChange {
            user: "user:newcomer".parse().expect("valid subject"),
            group: "group:acme/platform".parse().expect("valid subject"),
        }),
    )
    .await;
    assert!(
        outcome.permitted(),
        "a current member holds what they are handing on, got {:?}",
        outcome.rejections
    );
}

// -----------------------------------------------------------------------------
// Token ceiling
// -----------------------------------------------------------------------------

/// A writer whose credential withholds a verb cannot delegate it, even though
/// their live RBAC policy allows it.
#[tokio::test]
async fn a_credential_ceiling_limits_what_a_writer_may_delegate() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    builder.role(ROLE, "project-admin", Some(acme), allow_all());
    builder.binding(
        ROLE_BINDING,
        "iris-admin",
        Some(acme),
        json!({
            "subject": "user:iris",
            "scope": "rise.dev/Organization/acme",
            "roleRef": { "kind": "Role", "name": "project-admin" }
        }),
    );
    let store = builder.build();
    let engine = engine(store, FakeMemberships::none());

    let delegation = |uid: Uuid| {
        AuthorizationChange::Binding(BindingChange {
            before: None,
            after: Some(Box::new(BindingState {
                uid,
                kind: BindingKind::RoleBinding,
                organization: Some("acme".to_owned()),
                subject: "user:jack".parse().expect("valid subject"),
                subject_membership: SubjectMembership::Any,
                scope: "rise.dev/Organization/acme".parse().expect("valid scope"),
                selector: None,
                role: RoleReference {
                    kind: RoleRefKind::Role,
                    name: "project-admin".to_owned(),
                },
            })),
        })
    };

    let unrestricted = snapshot(&engine, principal("user:iris")).await;
    assert!(
        gate(&engine, &unrestricted, delegation(Uuid::new_v4()))
            .await
            .permitted(),
        "an uncapped admin may delegate their own Role"
    );

    let capped = snapshot(
        &engine,
        capped_principal("user:iris", "rise.dev/Organization/acme", &["get"]),
    )
    .await;
    let outcome = gate(&engine, &capped, delegation(Uuid::new_v4())).await;
    assert!(
        !outcome.permitted(),
        "a get-only credential cannot hand out an admin Role"
    );
    // The ceiling admits `get` on the main resource and nothing else, so that
    // exact tuple must not appear among the missing ones — `get` on a
    // *subresource* legitimately does, since the entry omits `subresources`.
    let missing_main_get = outcome.rejections.iter().any(|rejection| {
        matches!(&rejection.reason, RejectionReason::InsufficientAuthority(tuples)
            if tuples
                .iter()
                .any(|tuple| tuple.verb == Verb::Get && tuple.subresource.is_none()))
    });
    assert!(
        !missing_main_get,
        "the tuple the ceiling admits is not reported missing, got {:?}",
        outcome.rejections
    );
}

// -----------------------------------------------------------------------------
// Scenarios 39–43 — ownership labels
// -----------------------------------------------------------------------------

/// The seeded ownership binding: whoever `rise.dev/owner` names gets
/// `resource-owner` over the resource.
fn seeded_ownership(builder: &mut StoreBuilder) {
    builder.role(
        PLATFORM_ROLE,
        "resource-owner",
        None,
        json!([{ "effect": "Allow", "kinds": "*", "verbs": ["get", "list", "update", "delete"] }]),
    );
    builder.binding(
        PLATFORM_ROLE_BINDING,
        "resource-owner",
        None,
        json!({
            "subject": "${ref.subject}",
            "subjectMembership": "ResourceOrganization",
            "scope": "*",
            "labelSelector": { "key": OWNER_KEY },
            "roleRef": { "kind": "PlatformRole", "name": "resource-owner" }
        }),
    );
}

fn tree(store: &FakeStore, chain: &[Uuid]) -> ResourceTree {
    ResourceTree::new(chain.iter().map(|uid| store.node(*uid)).collect()).expect("non-empty tree")
}

/// Scenario 41 — an editor cannot redirect ownership to their own Group, even
/// though the resource never loses access.
#[tokio::test]
async fn an_unauthorized_owner_redirect_is_refused() {
    let mut builder = StoreBuilder::new();
    seeded_ownership(&mut builder);
    let acme = builder.resource(ORGANIZATION, "acme", None);
    let web = builder.labeled(PROJECT, "web", Some(acme), &[(OWNER_KEY, "group:platform")]);
    let store = builder.build();
    let engine = engine(
        store.clone(),
        FakeMemberships::groups(&["group:acme/interlopers"]),
    );
    let mallory = snapshot(&engine, principal("user:mallory")).await;

    let outcome = gate(
        &engine,
        &mallory,
        AuthorizationChange::Label(LabelChange {
            target: tree(&store, &[acme, web]),
            target_uid: Some(web),
            key: OWNER_KEY.parse().expect("valid label key"),
            after_value: Some("group:interlopers".to_owned()),
            creation: false,
        }),
    )
    .await;

    assert!(
        !outcome.permitted(),
        "redirecting ownership is a grant to the new owner and needs the writer to hold it"
    );
    assert_eq!(
        outcome.claims.len(),
        1,
        "one newly-named recipient on one resource"
    );
    assert_eq!(
        outcome.claims[0].recipient,
        BindingSubject::Literal("group:acme/interlopers".parse().expect("valid subject")),
        "the relative Group form resolves against the resource's own Organization"
    );
}

/// The resource's current owner may hand ownership on: they already hold exactly
/// what is being delegated, so the subset check passes trivially.
#[tokio::test]
async fn the_current_owner_may_transfer_ownership() {
    let mut builder = StoreBuilder::new();
    seeded_ownership(&mut builder);
    let acme = builder.resource(ORGANIZATION, "acme", None);
    let web = builder.labeled(PROJECT, "web", Some(acme), &[(OWNER_KEY, "group:platform")]);
    let store = builder.build();
    let engine = engine(
        store.clone(),
        FakeMemberships::groups(&["group:acme/platform", "group:acme/successors"]),
    );
    let owner = snapshot(&engine, principal("user:olive")).await;

    let outcome = gate(
        &engine,
        &owner,
        AuthorizationChange::Label(LabelChange {
            target: tree(&store, &[acme, web]),
            target_uid: Some(web),
            key: OWNER_KEY.parse().expect("valid label key"),
            after_value: Some("group:successors".to_owned()),
            creation: false,
        }),
    )
    .await;

    assert!(
        outcome.permitted(),
        "the owner holds resource-owner already, got {:?}",
        outcome.rejections
    );
}

/// Scenario 39/40 — removing a child's own owner label re-exposes the ancestor's
/// value, and the diff spans every descendant that inherits it.
///
/// This is §6.6's named trap: dropping a label reads as `victim → absent` if you
/// diff the resource's own stored value, and as `victim → the writer's group` if
/// you diff the *effective* value. Only the second sees the escalation.
#[tokio::test]
async fn removing_an_owner_label_is_gated_across_the_inheriting_subtree() {
    let mut builder = StoreBuilder::new();
    seeded_ownership(&mut builder);
    // The ancestor names the writer's own Group, so removal redirects ownership
    // to them.
    let acme = builder.labeled(ORGANIZATION, "acme", None, &[(OWNER_KEY, "group:platform")]);
    let web = builder.labeled(PROJECT, "web", Some(acme), &[(OWNER_KEY, "group:victims")]);
    // Inherits `web`'s value today and the Organization's after the removal.
    builder.resource(ENVIRONMENT, "prod", Some(web));
    // Sets its own value, so the removal cannot reach it.
    builder.labeled(
        ENVIRONMENT,
        "staging",
        Some(web),
        &[(OWNER_KEY, "group:victims")],
    );
    let store = builder.build();
    // Olive is in the Group the ancestor names but owns nothing here.
    let engine = engine(
        store.clone(),
        FakeMemberships::groups(&["group:acme/platform"]),
    );
    let olive = snapshot(&engine, principal("user:olive")).await;

    let outcome = gate(
        &engine,
        &olive,
        AuthorizationChange::Label(LabelChange {
            target: tree(&store, &[acme, web]),
            target_uid: Some(web),
            key: OWNER_KEY.parse().expect("valid label key"),
            after_value: None,
            creation: false,
        }),
    )
    .await;

    let scopes: Vec<&str> = outcome
        .claims
        .iter()
        .map(|claim| claim.domain.domain.scope.as_ref())
        .collect();
    assert!(
        !outcome.permitted(),
        "the removal redirects ownership of web and prod to the writer's own Group"
    );
    assert_eq!(
        outcome.claims.len(),
        2,
        "the written resource and its one inheriting descendant, got {scopes:?}"
    );
    assert!(scopes.contains(&"rise.dev/Project/acme/web"));
    assert!(scopes.contains(&"rise.dev/Environment/acme/web/prod"));
    assert!(
        !scopes.iter().any(|scope| scope.contains("staging")),
        "a descendant with its own value is not affected, got {scopes:?}"
    );
    for claim in &outcome.claims {
        assert_eq!(
            claim.recipient,
            BindingSubject::Literal("group:acme/platform".parse().expect("valid subject")),
            "the re-exposed ancestor value is the recipient"
        );
    }
}

/// The resource's current owner may drop its label even though that redirects
/// ownership upwards: they already hold everything `resource-owner` confers here,
/// so the subset check passes.
#[tokio::test]
async fn the_owner_may_drop_their_own_label() {
    let mut builder = StoreBuilder::new();
    seeded_ownership(&mut builder);
    let acme = builder.labeled(ORGANIZATION, "acme", None, &[(OWNER_KEY, "group:elders")]);
    let web = builder.labeled(PROJECT, "web", Some(acme), &[(OWNER_KEY, "group:platform")]);
    let store = builder.build();
    let engine = engine(
        store.clone(),
        FakeMemberships::groups(&["group:acme/platform"]),
    );
    let owner = snapshot(&engine, principal("user:olive")).await;

    let outcome = gate(
        &engine,
        &owner,
        AuthorizationChange::Label(LabelChange {
            target: tree(&store, &[acme, web]),
            target_uid: Some(web),
            key: OWNER_KEY.parse().expect("valid label key"),
            after_value: None,
            creation: false,
        }),
    )
    .await;
    assert!(
        outcome.permitted(),
        "the owner may hand on exactly what they hold, got {:?}",
        outcome.rejections
    );
}

/// Removing the only owner label in a chain grants nobody: the written resource
/// and every inheritor end up with no effective value, so the binding names
/// nobody there. Losing access is never a grant.
#[tokio::test]
async fn removing_the_last_owner_label_grants_nobody() {
    let mut builder = StoreBuilder::new();
    seeded_ownership(&mut builder);
    // No value above `web`, so the removal leaves nothing to inherit.
    let acme = builder.resource(ORGANIZATION, "acme", None);
    let web = builder.labeled(PROJECT, "web", Some(acme), &[(OWNER_KEY, "group:platform")]);
    builder.resource(ENVIRONMENT, "prod", Some(web));
    let store = builder.build();
    let engine = engine(store.clone(), FakeMemberships::none());
    // A writer with no authority at all: if anything were granted here, they
    // could not justify it.
    let nobody = snapshot(&engine, principal("user:nobody")).await;

    let outcome = gate(
        &engine,
        &nobody,
        AuthorizationChange::Label(LabelChange {
            target: tree(&store, &[acme, web]),
            target_uid: Some(web),
            key: OWNER_KEY.parse().expect("valid label key"),
            after_value: None,
            creation: false,
        }),
    )
    .await;

    assert!(
        outcome.permitted(),
        "nothing is delegated, got {:?}",
        outcome.rejections
    );
    assert!(
        outcome.claims.is_empty(),
        "no resource gains an owner, got {:?}",
        outcome
            .claims
            .iter()
            .map(|claim| (
                claim.recipient.to_string(),
                claim.domain.domain.scope.as_ref()
            ))
            .collect::<Vec<_>>()
    );
}

/// A binding scoped *below* the written resource still makes the key gated.
///
/// Relabelling an Organization changes what a Project-scoped ownership binding
/// resolves to on that Project, so applicability cannot be tested against the
/// written resource alone — the write reaches the binding through inheritance,
/// not through covering it.
#[tokio::test]
async fn a_binding_scoped_below_the_written_resource_still_gates_the_key() {
    let mut builder = StoreBuilder::new();
    // Deliberately no wildcard-scoped ownership binding: the only selector on the
    // key sits on a Project, strictly below the Organization being relabelled.
    builder.role(
        PLATFORM_ROLE,
        "resource-owner",
        None,
        json!([{ "effect": "Allow", "kinds": "*", "verbs": ["get", "list", "update", "delete"] }]),
    );
    let acme = builder.labeled(ORGANIZATION, "acme", None, &[(OWNER_KEY, "group:victims")]);
    builder.resource(PROJECT, "web", Some(acme));
    builder.binding(
        PLATFORM_ROLE_BINDING,
        "web-owner",
        None,
        json!({
            "subject": "${ref.subject}",
            "subjectMembership": "ResourceOrganization",
            "scope": "rise.dev/Project/acme/web",
            "labelSelector": { "key": OWNER_KEY },
            "roleRef": { "kind": "PlatformRole", "name": "resource-owner" }
        }),
    );
    let store = builder.build();
    let engine = engine(
        store.clone(),
        FakeMemberships::groups(&["group:acme/interlopers"]),
    );
    let mallory = snapshot(&engine, principal("user:mallory")).await;

    let outcome = gate(
        &engine,
        &mallory,
        AuthorizationChange::Label(LabelChange {
            target: tree(&store, &[acme]),
            target_uid: Some(acme),
            key: OWNER_KEY.parse().expect("valid label key"),
            after_value: Some("group:interlopers".to_owned()),
            creation: false,
        }),
    )
    .await;

    assert!(
        !outcome.permitted(),
        "the Project-scoped binding hands `web` to the writer's own Group"
    );
    let scopes: Vec<&str> = outcome
        .claims
        .iter()
        .map(|claim| claim.domain.domain.scope.as_ref())
        .collect();
    assert_eq!(
        scopes,
        vec!["rise.dev/Project/acme/web"],
        "only the resource the binding actually reaches is claimed"
    );
}

/// Step 2 — a label no binding selects on is ungated however it changes.
#[tokio::test]
async fn a_label_no_binding_selects_on_is_ungated() {
    let mut builder = StoreBuilder::new();
    seeded_ownership(&mut builder);
    let acme = builder.resource(ORGANIZATION, "acme", None);
    let web = builder.resource(PROJECT, "web", Some(acme));
    let store = builder.build();
    let engine = engine(store.clone(), FakeMemberships::none());
    let anyone = snapshot(&engine, principal("user:anyone")).await;

    let outcome = gate(
        &engine,
        &anyone,
        AuthorizationChange::Label(LabelChange {
            target: tree(&store, &[acme, web]),
            target_uid: Some(web),
            key: "rise.dev/team".parse().expect("valid label key"),
            after_value: Some("group:whatever".to_owned()),
            creation: false,
        }),
    )
    .await;
    assert!(outcome.permitted());
    assert!(outcome.claims.is_empty());
    assert!(!outcome.creation_exception);
}

/// Step 1 — restating a value the resource already inherits changes no effective
/// value and needs only ordinary `update`.
#[tokio::test]
async fn restating_an_inherited_value_is_ungated() {
    let mut builder = StoreBuilder::new();
    seeded_ownership(&mut builder);
    let acme = builder.labeled(ORGANIZATION, "acme", None, &[(OWNER_KEY, "group:platform")]);
    let web = builder.resource(PROJECT, "web", Some(acme));
    let store = builder.build();
    let engine = engine(store.clone(), FakeMemberships::none());
    let anyone = snapshot(&engine, principal("user:anyone")).await;

    let outcome = gate(
        &engine,
        &anyone,
        AuthorizationChange::Label(LabelChange {
            target: tree(&store, &[acme, web]),
            target_uid: Some(web),
            key: OWNER_KEY.parse().expect("valid label key"),
            after_value: Some("group:platform".to_owned()),
            creation: false,
        }),
    )
    .await;
    assert!(outcome.permitted());
    assert!(outcome.claims.is_empty(), "no effective value changed");
}

/// Scenario 42 — the creation exception covers a creator naming themselves or a
/// same-org Group they belong to, and nothing else.
#[tokio::test]
async fn the_creation_exception_is_narrow_and_org_clamped() {
    let mut builder = StoreBuilder::new();
    seeded_ownership(&mut builder);
    let acme = builder.resource(ORGANIZATION, "acme", None);
    let store = builder.build();
    let engine = engine(
        store.clone(),
        FakeMemberships::groups(&["group:acme/platform", "group:other/elsewhere"]),
    );
    let creator = snapshot(&engine, principal("user:paula")).await;

    let proposed = |value: &str| {
        AuthorizationChange::Label(LabelChange {
            target: ResourceTree::with_leaf(
                &[store.node(acme)],
                ResourceNode::new("rise.dev/Project".parse().expect("valid kind"), "fresh")
                    .with_labels([(OWNER_KEY, value)]),
            ),
            target_uid: None,
            key: OWNER_KEY.parse().expect("valid label key"),
            after_value: Some(value.to_owned()),
            creation: true,
        })
    };

    let own = gate(&engine, &creator, proposed("user:paula")).await;
    assert!(
        own.permitted() && own.creation_exception,
        "naming themselves"
    );

    let own_group = gate(&engine, &creator, proposed("group:platform")).await;
    assert!(
        own_group.permitted() && own_group.creation_exception,
        "naming a same-org Group they belong to"
    );

    // A Group in another organization resolves relative to *this* resource's org,
    // so `group:elsewhere` is `group:acme/elsewhere` — a Group the creator is not
    // in. The general rule governs, and the creator holds nothing.
    let foreign = gate(&engine, &creator, proposed("group:elsewhere")).await;
    assert!(
        !foreign.permitted() && !foreign.creation_exception,
        "a Group the creator does not belong to falls back to the subset rule"
    );

    let other_user = gate(&engine, &creator, proposed("user:quinn")).await;
    assert!(
        !other_user.permitted() && !other_user.creation_exception,
        "naming another User falls back to the subset rule"
    );
}

/// The exception applies once: the very next update to the same label is
/// unconditionally subject to the general rule.
#[tokio::test]
async fn the_creation_exception_does_not_survive_into_an_update() {
    let mut builder = StoreBuilder::new();
    seeded_ownership(&mut builder);
    let acme = builder.resource(ORGANIZATION, "acme", None);
    let web = builder.labeled(PROJECT, "web", Some(acme), &[(OWNER_KEY, "user:paula")]);
    let store = builder.build();
    let engine = engine(
        store.clone(),
        FakeMemberships::groups(&["group:acme/platform"]),
    );
    // Rob is in no group and owns nothing, but holds ordinary update access.
    let rob = snapshot(&engine, principal("user:rob")).await;

    let outcome = gate(
        &engine,
        &rob,
        AuthorizationChange::Label(LabelChange {
            target: tree(&store, &[acme, web]),
            target_uid: Some(web),
            key: OWNER_KEY.parse().expect("valid label key"),
            after_value: Some("user:rob".to_owned()),
            // An existing identity is never a creation, whatever the caller wants.
            creation: false,
        }),
    )
    .await;
    assert!(
        !outcome.permitted() && !outcome.creation_exception,
        "taking ownership of an existing resource is the general rule's business"
    );
}

/// Scenario 43 — org-admin standing derives from a scope-only binding, so no
/// ownership-label write can reach it. The gate treats an admin's relabel as
/// trivially covered because their access is label-independent.
#[tokio::test]
async fn admin_access_is_independent_of_ownership_labels() {
    let mut builder = StoreBuilder::new();
    seeded_ownership(&mut builder);
    builder.role(PLATFORM_ROLE, ORG_ADMIN_PLATFORM_ROLE, None, allow_all());
    let acme = builder.resource(ORGANIZATION, "acme", None);
    let web = builder.labeled(PROJECT, "web", Some(acme), &[(OWNER_KEY, "group:platform")]);
    builder.binding(
        ROLE_BINDING,
        "sam-admin",
        Some(acme),
        org_admin_spec("user:sam", "acme"),
    );
    let store = builder.build();
    let engine = engine(store.clone(), FakeMemberships::none());
    let sam = snapshot(&engine, principal("user:sam")).await;

    let outcome = gate(
        &engine,
        &sam,
        AuthorizationChange::Label(LabelChange {
            target: tree(&store, &[acme, web]),
            target_uid: Some(web),
            key: OWNER_KEY.parse().expect("valid label key"),
            after_value: Some("group:anything".to_owned()),
            creation: false,
        }),
    )
    .await;
    assert!(
        outcome.permitted(),
        "an org admin holds resource-owner's verbs org-wide already, got {:?}",
        outcome.rejections
    );
}
