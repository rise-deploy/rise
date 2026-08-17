mod support;

use std::sync::Arc;

use rise_authz::engine::{
    AuthenticatedPrincipal, AuthorizationCap, AuthorizationEngine, AuthorizationError,
    AuthorizationSnapshot, CapEntry, CapPermission, DenyExemption, InertReason, ListCandidate,
    ResourceNode, ResourceTree, Retention, ORG_ADMIN_PLATFORM_ROLE,
};
use rise_authz::policy::{Decision, PermissionTuple};
use rise_resource_api::{ResourceStore, Verb};
use serde_json::json;
use support::{FakeMemberships, FakeStore, StoreBuilder};
use uuid::Uuid;

const ORGANIZATION: &str = "Organization";
const PROJECT: &str = "Project";
const DEPLOYMENT: &str = "Deployment";
const ENVIRONMENT: &str = "Environment";
const ROLE: &str = "Role";
const ROLE_BINDING: &str = "RoleBinding";
const PLATFORM_ROLE: &str = "PlatformRole";
const PLATFORM_ROLE_BINDING: &str = "PlatformRoleBinding";

fn principal(subject: &str) -> AuthenticatedPrincipal {
    AuthenticatedPrincipal::new(
        subject.parse().expect("valid subject"),
        Uuid::new_v4(),
        AuthorizationCap::Unrestricted,
    )
    .expect("principal kind")
}

fn tuple(verb: Verb, kind: &str) -> PermissionTuple {
    PermissionTuple {
        verb,
        kind: kind.parse().expect("valid kind"),
        subresource: None,
    }
}

fn allow_all() -> serde_json::Value {
    json!([
        { "effect": "Allow", "kinds": "*", "verbs": "*" },
        { "effect": "Allow", "kinds": "*", "verbs": "*", "subresources": "*" }
    ])
}

/// An org `RoleBinding` that satisfies §5's exact structural admin predicate.
fn org_admin_binding(subject: &str, organization: &str) -> serde_json::Value {
    json!({
        "subject": subject,
        "scope": format!("rise.dev/Organization/{organization}"),
        "roleRef": { "kind": "PlatformRole", "name": ORG_ADMIN_PLATFORM_ROLE }
    })
}

async fn snapshot_for(engine: &AuthorizationEngine, subject: &str) -> AuthorizationSnapshot {
    engine
        .snapshot(principal(subject))
        .await
        .expect("snapshot builds")
}

async fn decide(
    engine: &AuthorizationEngine,
    snapshot: &AuthorizationSnapshot,
    uid: Uuid,
    request: &PermissionTuple,
) -> Decision {
    let target = engine.resource_tree(uid).await.expect("resource tree");
    engine
        .authorize(snapshot, &target, request)
        .await
        .expect("evaluation succeeds")
}

fn engine(store: Arc<FakeStore>, memberships: Arc<FakeMemberships>) -> AuthorizationEngine {
    AuthorizationEngine::new(store as Arc<dyn ResourceStore>, memberships)
}

#[tokio::test]
async fn allows_union_and_a_retained_deny_wins() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    let app = builder.resource(PROJECT, "app", Some(acme));
    let deployment = builder.resource(DEPLOYMENT, "foo", Some(app));
    builder.role(
        ROLE,
        "deployment-reader",
        Some(acme),
        json!([{ "effect": "Allow", "kinds": ["rise.dev/Deployment"], "verbs": ["get"] }]),
    );
    builder.role(
        ROLE,
        "deployment-editor",
        Some(acme),
        json!([{ "effect": "Allow", "kinds": ["rise.dev/Deployment"], "verbs": ["update", "delete"] }]),
    );
    builder.role(
        ROLE,
        "no-delete",
        Some(acme),
        json!([{ "effect": "Deny", "kinds": ["rise.dev/Deployment"], "verbs": ["delete"] }]),
    );
    for (name, role) in [
        ("reader", "deployment-reader"),
        ("editor", "deployment-editor"),
        ("cap", "no-delete"),
    ] {
        builder.binding(
            ROLE_BINDING,
            name,
            Some(acme),
            json!({
                "subject": "group:acme/platform",
                "scope": "rise.dev/Organization/acme",
                "roleRef": { "kind": "Role", "name": role }
            }),
        );
    }
    let store = builder.build();
    let engine = engine(
        store.clone(),
        FakeMemberships::groups(&["group:acme/platform"]),
    );
    let snapshot = snapshot_for(&engine, "user:u-alice").await;

    for (verb, expected) in [
        (Verb::Get, Decision::Allow),
        (Verb::Update, Decision::Allow),
        (Verb::Delete, Decision::Deny),
        (Verb::Create, Decision::Deny),
    ] {
        assert_eq!(
            decide(
                &engine,
                &snapshot,
                deployment,
                &tuple(verb, "rise.dev/Deployment")
            )
            .await,
            expected,
            "{verb:?}"
        );
    }
}

#[tokio::test]
async fn a_platform_deny_reaches_an_org_admin() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    let app = builder.resource(PROJECT, "app", Some(acme));
    let deployment = builder.resource(DEPLOYMENT, "foo", Some(app));
    builder.role(PLATFORM_ROLE, ORG_ADMIN_PLATFORM_ROLE, None, allow_all());
    builder.role(
        PLATFORM_ROLE,
        "acme-ceiling",
        None,
        json!([{ "effect": "Deny", "kinds": ["rise.dev/Deployment"], "verbs": ["delete"] }]),
    );
    builder.binding(
        ROLE_BINDING,
        "admin",
        Some(acme),
        org_admin_binding("user:u-alice", "acme"),
    );
    builder.binding(
        PLATFORM_ROLE_BINDING,
        "acme-ceiling",
        None,
        json!({
            "subject": "system:authenticated",
            "subjectMembership": "Any",
            "scope": "rise.dev/Organization/acme",
            "roleRef": { "kind": "PlatformRole", "name": "acme-ceiling" }
        }),
    );
    let store = builder.build();
    let engine = engine(store.clone(), FakeMemberships::none());
    let snapshot = snapshot_for(&engine, "user:u-alice").await;

    let target = engine.resource_tree(deployment).await.unwrap();
    let policy = engine.effective_policy(&snapshot, &target).await.unwrap();
    assert!(policy.is_organization_admin());
    assert_eq!(
        policy.decide(&tuple(Verb::Update, "rise.dev/Deployment")),
        Decision::Allow
    );
    assert_eq!(
        policy.decide(&tuple(Verb::Delete, "rise.dev/Deployment")),
        Decision::Deny
    );
}

#[tokio::test]
async fn an_org_deny_exempts_that_orgs_admins_only() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    let app = builder.resource(PROJECT, "app", Some(acme));
    let deployment = builder.resource(DEPLOYMENT, "foo", Some(app));
    builder.role(PLATFORM_ROLE, ORG_ADMIN_PLATFORM_ROLE, None, allow_all());
    builder.role(PLATFORM_ROLE, "everything", None, allow_all());
    builder.role(
        ROLE,
        "no-delete",
        Some(acme),
        json!([{ "effect": "Deny", "kinds": ["rise.dev/Deployment"], "verbs": ["delete"] }]),
    );
    builder.binding(
        ROLE_BINDING,
        "admin",
        Some(acme),
        org_admin_binding("user:u-alice", "acme"),
    );
    builder.binding(
        ROLE_BINDING,
        "member-grant",
        Some(acme),
        json!({
            "subject": "group:acme/platform",
            "scope": "rise.dev/Organization/acme",
            "roleRef": { "kind": "PlatformRole", "name": "everything" }
        }),
    );
    builder.binding(
        ROLE_BINDING,
        "member-cap",
        Some(acme),
        json!({
            "subject": "group:acme/platform",
            "scope": "rise.dev/Organization/acme",
            "roleRef": { "kind": "Role", "name": "no-delete" }
        }),
    );
    let store = builder.build();
    let delete = tuple(Verb::Delete, "rise.dev/Deployment");

    let admin_engine = engine(store.clone(), FakeMemberships::none());
    let admin = snapshot_for(&admin_engine, "user:u-alice").await;
    assert_eq!(
        decide(&admin_engine, &admin, deployment, &delete).await,
        Decision::Allow,
        "the org's own Deny does not cap its own admin"
    );

    let member_engine = engine(
        store.clone(),
        FakeMemberships::groups(&["group:acme/platform"]),
    );
    let member = snapshot_for(&member_engine, "user:u-bob").await;
    assert_eq!(
        decide(&member_engine, &member, deployment, &delete).await,
        Decision::Deny,
        "an ordinary member is capped by their org"
    );
}

#[tokio::test]
async fn an_operator_ignores_every_deny() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    let app = builder.resource(PROJECT, "app", Some(acme));
    let deployment = builder.resource(DEPLOYMENT, "foo", Some(app));
    builder.role(
        PLATFORM_ROLE,
        "lockout",
        None,
        json!([{ "effect": "Deny", "kinds": "*", "verbs": "*" }]),
    );
    builder.binding(
        PLATFORM_ROLE_BINDING,
        "lockout",
        None,
        json!({
            "subject": "system:authenticated",
            "subjectMembership": "Any",
            "scope": "*",
            "roleRef": { "kind": "PlatformRole", "name": "lockout" }
        }),
    );
    let store = builder.build();
    let engine = engine(store.clone(), FakeMemberships::operator());
    let snapshot = snapshot_for(&engine, "user:u-root").await;

    let target = engine.resource_tree(deployment).await.unwrap();
    let policy = engine.effective_policy(&snapshot, &target).await.unwrap();
    assert!(policy.is_operator());
    // No Allow binding exists at all: an operator's authority is the hardcoded
    // guarantee, not a row that a bad restore could take away.
    assert_eq!(
        policy.decide(&tuple(Verb::Delete, "rise.dev/Deployment")),
        Decision::Allow
    );
    assert_eq!(
        policy.decide(&PermissionTuple {
            verb: Verb::Update,
            kind: "rise.dev/Deployment".parse().unwrap(),
            subresource: Some("status".parse().unwrap()),
        }),
        Decision::Allow
    );
    assert!(policy.contributions().iter().all(|contribution| matches!(
        contribution.retention,
        Retention::ExemptDeny(DenyExemption::Operator)
    )));
}

#[tokio::test]
async fn wildcard_replacement_drops_allows_and_keeps_denies() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    let app = builder.resource(PROJECT, "app", Some(acme));
    let deployment = builder.resource(DEPLOYMENT, "foo", Some(app));
    builder.role(
        PLATFORM_ROLE,
        "broad",
        None,
        json!([
            { "effect": "Allow", "kinds": "*", "verbs": "*" },
            { "effect": "Deny", "kinds": ["rise.dev/Deployment"], "verbs": ["delete"] }
        ]),
    );
    builder.role(
        PLATFORM_ROLE,
        "narrow",
        None,
        json!([{ "effect": "Allow", "kinds": ["rise.dev/Deployment"], "verbs": ["get"] }]),
    );
    builder.binding(
        PLATFORM_ROLE_BINDING,
        "wildcard",
        None,
        json!({
            "subject": "controller:reconciler",
            "subjectMembership": "Any",
            "scope": "*",
            "roleRef": { "kind": "PlatformRole", "name": "broad" }
        }),
    );
    builder.binding(
        PLATFORM_ROLE_BINDING,
        "acme-specific",
        None,
        json!({
            "subject": "controller:reconciler",
            "subjectMembership": "Any",
            "scope": "rise.dev/Organization/acme",
            "roleRef": { "kind": "PlatformRole", "name": "narrow" }
        }),
    );
    let store = builder.build();
    let engine = engine(store.clone(), FakeMemberships::none());
    let snapshot = snapshot_for(&engine, "controller:reconciler").await;
    let target = engine.resource_tree(deployment).await.unwrap();
    let policy = engine.effective_policy(&snapshot, &target).await.unwrap();

    assert_eq!(
        policy.decide(&tuple(Verb::Get, "rise.dev/Deployment")),
        Decision::Allow
    );
    assert_eq!(
        policy.decide(&tuple(Verb::Update, "rise.dev/Deployment")),
        Decision::Deny,
        "the specific binding replaces the wildcard's Allow outright"
    );
    assert_eq!(
        policy.decide(&tuple(Verb::Delete, "rise.dev/Deployment")),
        Decision::Deny,
        "and the superseded binding's Deny survives replacement"
    );
    let deny = policy
        .contributions()
        .iter()
        .find(|contribution| contribution.statement.effect == rise_resource_api::Effect::Deny)
        .expect("the Deny is retained with provenance");
    assert_eq!(deny.retention, Retention::Retained);
    assert_eq!(deny.tier, rise_authz::policy::BindingTier::Platform);
}

#[tokio::test]
async fn group_expansion_is_live() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    let app = builder.resource(PROJECT, "app", Some(acme));
    builder.role(PLATFORM_ROLE, "everything", None, allow_all());
    builder.binding(
        ROLE_BINDING,
        "platform-team",
        Some(acme),
        json!({
            "subject": "group:acme/platform",
            "scope": "rise.dev/Organization/acme",
            "roleRef": { "kind": "PlatformRole", "name": "everything" }
        }),
    );
    let store = builder.build();
    let get = tuple(Verb::Get, "rise.dev/Project");

    let member = engine(
        store.clone(),
        FakeMemberships::groups(&["group:acme/platform"]),
    );
    let snapshot = snapshot_for(&member, "user:u-alice").await;
    assert_eq!(decide(&member, &snapshot, app, &get).await, Decision::Allow);

    // The same policy rows, one membership removal later.
    let former = engine(store.clone(), FakeMemberships::none());
    let snapshot = snapshot_for(&former, "user:u-alice").await;
    assert_eq!(decide(&former, &snapshot, app, &get).await, Decision::Deny);
}

#[tokio::test]
async fn an_org_binding_needs_a_live_group_tie() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    let app = builder.resource(PROJECT, "app", Some(acme));
    builder.role(PLATFORM_ROLE, "everything", None, allow_all());
    builder.binding(
        ROLE_BINDING,
        "direct-user",
        Some(acme),
        json!({
            "subject": "user:u-alice",
            "scope": "rise.dev/Organization/acme",
            "roleRef": { "kind": "PlatformRole", "name": "everything" }
        }),
    );
    builder.binding(
        ROLE_BINDING,
        "everyone",
        Some(acme),
        json!({
            "subject": "system:authenticated",
            "scope": "rise.dev/Organization/acme",
            "roleRef": { "kind": "PlatformRole", "name": "everything" }
        }),
    );
    let store = builder.build();
    let get = tuple(Verb::Get, "rise.dev/Project");

    let stranger = engine(store.clone(), FakeMemberships::none());
    let snapshot = snapshot_for(&stranger, "user:u-alice").await;
    let target = stranger.resource_tree(app).await.unwrap();
    let policy = stranger.effective_policy(&snapshot, &target).await.unwrap();
    assert_eq!(policy.decide(&get), Decision::Deny);
    assert_eq!(policy.inert_bindings().len(), 2);
    assert!(policy
        .inert_bindings()
        .iter()
        .all(|binding| binding.reason == InertReason::RecipientBoundary));

    let member = engine(
        store.clone(),
        FakeMemberships::groups(&["group:acme/other"]),
    );
    let snapshot = snapshot_for(&member, "user:u-alice").await;
    assert_eq!(decide(&member, &snapshot, app, &get).await, Decision::Allow);
}

#[tokio::test]
async fn a_direct_org_admin_binding_bootstraps_affiliation() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    let app = builder.resource(PROJECT, "app", Some(acme));
    builder.role(PLATFORM_ROLE, ORG_ADMIN_PLATFORM_ROLE, None, allow_all());
    builder.binding(
        ROLE_BINDING,
        "first-admin",
        Some(acme),
        org_admin_binding("user:u-alice", "acme"),
    );
    let store = builder.build();
    let engine = engine(store.clone(), FakeMemberships::none());
    let snapshot = snapshot_for(&engine, "user:u-alice").await;
    let target = engine.resource_tree(app).await.unwrap();
    let policy = engine.effective_policy(&snapshot, &target).await.unwrap();

    assert!(policy.is_organization_admin());
    assert_eq!(
        policy.decide(&tuple(Verb::Create, "rise.dev/Project")),
        Decision::Allow,
        "no pre-existing Group is required to govern a new org"
    );
}

#[tokio::test]
async fn a_group_targeted_admin_binding_needs_no_special_name() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    let app = builder.resource(PROJECT, "app", Some(acme));
    builder.role(PLATFORM_ROLE, ORG_ADMIN_PLATFORM_ROLE, None, allow_all());
    builder.binding(
        ROLE_BINDING,
        "admins",
        Some(acme),
        org_admin_binding("group:acme/whatever-we-called-it", "acme"),
    );
    // A label-selected binding to the same Role never confers admin standing.
    builder.binding(
        ROLE_BINDING,
        "selected",
        Some(acme),
        json!({
            "subject": "group:acme/whatever-we-called-it",
            "scope": "rise.dev/Organization/acme",
            "labelSelector": { "key": "rise.dev/squad", "value": "platform" },
            "roleRef": { "kind": "PlatformRole", "name": ORG_ADMIN_PLATFORM_ROLE }
        }),
    );
    let store = builder.build();

    let insider = engine(
        store.clone(),
        FakeMemberships::groups(&["group:acme/whatever-we-called-it"]),
    );
    let snapshot = snapshot_for(&insider, "user:u-alice").await;
    let target = insider.resource_tree(app).await.unwrap();
    assert!(insider
        .effective_policy(&snapshot, &target)
        .await
        .unwrap()
        .is_organization_admin());

    let outsider = engine(store.clone(), FakeMemberships::none());
    let snapshot = snapshot_for(&outsider, "user:u-bob").await;
    let target = outsider.resource_tree(app).await.unwrap();
    assert!(!outsider
        .effective_policy(&snapshot, &target)
        .await
        .unwrap()
        .is_organization_admin());
}

#[tokio::test]
async fn a_foreign_subject_is_inert_and_reported() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    let beta = builder.resource(ORGANIZATION, "beta", None);
    let beta_app = builder.resource(PROJECT, "app", Some(beta));
    let _ = acme;
    builder.role(PLATFORM_ROLE, "everything", None, allow_all());
    builder.binding(
        ROLE_BINDING,
        "cross-org",
        Some(beta),
        json!({
            "subject": "group:acme/platform",
            "scope": "rise.dev/Organization/beta",
            "roleRef": { "kind": "PlatformRole", "name": "everything" }
        }),
    );
    let store = builder.build();
    let engine = engine(
        store.clone(),
        FakeMemberships::groups(&["group:acme/platform"]),
    );
    let snapshot = snapshot_for(&engine, "user:u-alice").await;
    let target = engine.resource_tree(beta_app).await.unwrap();
    let policy = engine.effective_policy(&snapshot, &target).await.unwrap();

    assert_eq!(
        policy.decide(&tuple(Verb::Get, "rise.dev/Project")),
        Decision::Deny
    );
    assert_eq!(policy.inert_bindings().len(), 1);
    assert_eq!(
        policy.inert_bindings()[0].reason,
        InertReason::RecipientBoundary
    );
}

#[tokio::test]
async fn resource_organization_clamps_users_and_excludes_controllers() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    let app = builder.resource(PROJECT, "app", Some(acme));
    builder.role(PLATFORM_ROLE, "everything", None, allow_all());
    builder.binding(
        PLATFORM_ROLE_BINDING,
        "clamped-user",
        None,
        json!({
            "subject": "user:u-alice",
            "subjectMembership": "ResourceOrganization",
            "scope": "*",
            "roleRef": { "kind": "PlatformRole", "name": "everything" }
        }),
    );
    builder.binding(
        PLATFORM_ROLE_BINDING,
        "unclamped-user",
        None,
        json!({
            "subject": "user:u-bob",
            "subjectMembership": "Any",
            "scope": "*",
            "roleRef": { "kind": "PlatformRole", "name": "everything" }
        }),
    );
    builder.binding(
        PLATFORM_ROLE_BINDING,
        "clamped-controller",
        None,
        json!({
            "subject": "controller:reconciler",
            "subjectMembership": "ResourceOrganization",
            "scope": "*",
            "roleRef": { "kind": "PlatformRole", "name": "everything" }
        }),
    );
    let store = builder.build();
    let get = tuple(Verb::Get, "rise.dev/Project");

    let stranger = engine(store.clone(), FakeMemberships::none());
    let snapshot = snapshot_for(&stranger, "user:u-alice").await;
    let target = stranger.resource_tree(app).await.unwrap();
    let policy = stranger.effective_policy(&snapshot, &target).await.unwrap();
    assert_eq!(policy.decide(&get), Decision::Deny);
    assert_eq!(
        policy.inert_bindings()[0].reason,
        InertReason::ResourceOrganization
    );

    let member = engine(
        store.clone(),
        FakeMemberships::groups(&["group:acme/platform"]),
    );
    let snapshot = snapshot_for(&member, "user:u-alice").await;
    assert_eq!(decide(&member, &snapshot, app, &get).await, Decision::Allow);

    let non_member = engine(store.clone(), FakeMemberships::none());
    let snapshot = snapshot_for(&non_member, "user:u-bob").await;
    assert_eq!(
        decide(&non_member, &snapshot, app, &get).await,
        Decision::Allow,
        "`Any` is the deliberate non-member escape hatch"
    );

    let controller = engine(store.clone(), FakeMemberships::none());
    let snapshot = snapshot_for(&controller, "controller:reconciler").await;
    assert_eq!(
        decide(&controller, &snapshot, app, &get).await,
        Decision::Deny,
        "a Controller belongs to no organization and never matches the clamp"
    );
}

#[tokio::test]
async fn an_absolute_org_subject_reaches_a_root_resource() {
    let mut builder = StoreBuilder::new();
    let _acme = builder.resource(ORGANIZATION, "acme", None);
    let class = builder.resource("RuntimeClass", "gpu-b", None);
    builder.role(
        PLATFORM_ROLE,
        "rc-user",
        None,
        json!([{ "effect": "Allow", "kinds": ["rise.dev/RuntimeClass"], "verbs": ["use"] }]),
    );
    builder.binding(
        PLATFORM_ROLE_BINDING,
        "acme-gpu",
        None,
        json!({
            "subject": "org:acme",
            "subjectMembership": "Any",
            "scope": "rise.dev/RuntimeClass/gpu-b",
            "roleRef": { "kind": "PlatformRole", "name": "rc-user" }
        }),
    );
    let store = builder.build();
    let use_class = tuple(Verb::Use, "rise.dev/RuntimeClass");

    let acme_member = engine(
        store.clone(),
        FakeMemberships::groups(&["group:acme/platform"]),
    );
    let snapshot = snapshot_for(&acme_member, "user:u-alice").await;
    assert_eq!(
        decide(&acme_member, &snapshot, class, &use_class).await,
        Decision::Allow
    );

    let beta_member = engine(
        store.clone(),
        FakeMemberships::groups(&["group:beta/platform"]),
    );
    let snapshot = snapshot_for(&beta_member, "user:u-bob").await;
    assert_eq!(
        decide(&beta_member, &snapshot, class, &use_class).await,
        Decision::Deny
    );
}

#[tokio::test]
async fn ownership_resolves_nearest_wins_through_effective_labels() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    let app = builder.labeled(
        PROJECT,
        "app",
        Some(acme),
        &[("rise.dev/owner", "group:platform")],
    );
    let inherited = builder.resource(ENVIRONMENT, "staging", Some(app));
    let shadowed = builder.labeled(
        ENVIRONMENT,
        "prod",
        Some(app),
        &[("rise.dev/owner", "group:devops")],
    );
    builder.role(
        PLATFORM_ROLE,
        "resource-owner",
        None,
        json!([{
            "effect": "Allow", "kinds": "*", "verbs": ["get", "list", "update", "delete"]
        }]),
    );
    builder.binding(
        PLATFORM_ROLE_BINDING,
        "resource-owner",
        None,
        json!({
            "subject": "${ref.subject}",
            "subjectMembership": "ResourceOrganization",
            "scope": "*",
            "labelSelector": { "key": "rise.dev/owner" },
            "roleRef": { "kind": "PlatformRole", "name": "resource-owner" }
        }),
    );
    let store = builder.build();
    let update = tuple(Verb::Update, "rise.dev/Environment");
    let create = tuple(Verb::Create, "rise.dev/Environment");

    let platform = engine(
        store.clone(),
        FakeMemberships::groups(&["group:acme/platform"]),
    );
    let snapshot = snapshot_for(&platform, "user:u-alice").await;
    assert_eq!(
        decide(&platform, &snapshot, inherited, &update).await,
        Decision::Allow,
        "an unlabeled child inherits its Project's owner"
    );
    assert_eq!(
        decide(&platform, &snapshot, shadowed, &update).await,
        Decision::Deny,
        "a nearer value shadows the ancestor's rather than unioning with it"
    );
    assert_eq!(
        decide(&platform, &snapshot, inherited, &create).await,
        Decision::Deny,
        "ownership never confers create"
    );

    let devops = engine(
        store.clone(),
        FakeMemberships::groups(&["group:acme/devops"]),
    );
    let snapshot = snapshot_for(&devops, "user:u-bob").await;
    assert_eq!(
        decide(&devops, &snapshot, shadowed, &update).await,
        Decision::Allow
    );
    assert_eq!(
        decide(&devops, &snapshot, inherited, &update).await,
        Decision::Deny
    );
}

#[tokio::test]
async fn literal_and_templated_subjects_never_collide() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    let app = builder.labeled(
        PROJECT,
        "app",
        Some(acme),
        &[("rise.dev/owner", "group:platform")],
    );
    builder.role(
        PLATFORM_ROLE,
        "owner",
        None,
        json!([{ "effect": "Allow", "kinds": "*", "verbs": ["get", "update"] }]),
    );
    builder.role(
        PLATFORM_ROLE,
        "reader",
        None,
        json!([{ "effect": "Allow", "kinds": "*", "verbs": ["get"] }]),
    );
    builder.binding(
        PLATFORM_ROLE_BINDING,
        "dynamic-owner",
        None,
        json!({
            "subject": "${ref.subject}",
            "subjectMembership": "Any",
            "scope": "*",
            "labelSelector": { "key": "rise.dev/owner" },
            "roleRef": { "kind": "PlatformRole", "name": "owner" }
        }),
    );
    // A literal binding that resolves to the very same Group must not replace
    // the platform-wide dynamic default.
    builder.binding(
        PLATFORM_ROLE_BINDING,
        "literal-narrow",
        None,
        json!({
            "subject": "group:acme/platform",
            "subjectMembership": "Any",
            "scope": "rise.dev/Organization/acme",
            "roleRef": { "kind": "PlatformRole", "name": "reader" }
        }),
    );
    let store = builder.build();
    let engine = engine(
        store.clone(),
        FakeMemberships::groups(&["group:acme/platform"]),
    );
    let snapshot = snapshot_for(&engine, "user:u-alice").await;

    assert_eq!(
        decide(
            &engine,
            &snapshot,
            app,
            &tuple(Verb::Update, "rise.dev/Project")
        )
        .await,
        Decision::Allow,
        "replacement compares authored subject forms, not resolved values"
    );
}

#[tokio::test]
async fn a_value_narrowed_selector_replaces_only_matching_resources() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    let platform_app = builder.labeled(
        PROJECT,
        "platform-app",
        Some(acme),
        &[("rise.dev/squad", "platform")],
    );
    let other_app = builder.labeled(
        PROJECT,
        "other-app",
        Some(acme),
        &[("rise.dev/squad", "devops")],
    );
    builder.role(
        PLATFORM_ROLE,
        "editor",
        None,
        json!([{ "effect": "Allow", "kinds": "*", "verbs": ["get", "update"] }]),
    );
    builder.role(
        PLATFORM_ROLE,
        "reader",
        None,
        json!([{ "effect": "Allow", "kinds": "*", "verbs": ["get"] }]),
    );
    builder.binding(
        PLATFORM_ROLE_BINDING,
        "squad-wide",
        None,
        json!({
            "subject": "group:${ref.name}",
            "subjectMembership": "Any",
            "scope": "*",
            "labelSelector": { "key": "rise.dev/squad" },
            "roleRef": { "kind": "PlatformRole", "name": "editor" }
        }),
    );
    builder.binding(
        PLATFORM_ROLE_BINDING,
        "platform-squad-narrowed",
        None,
        json!({
            "subject": "group:${ref.name}",
            "subjectMembership": "Any",
            "scope": "rise.dev/Organization/acme",
            "labelSelector": { "key": "rise.dev/squad", "value": "platform" },
            "roleRef": { "kind": "PlatformRole", "name": "reader" }
        }),
    );
    let store = builder.build();
    let update = tuple(Verb::Update, "rise.dev/Project");

    let platform = engine(
        store.clone(),
        FakeMemberships::groups(&["group:acme/platform"]),
    );
    let snapshot = snapshot_for(&platform, "user:u-alice").await;
    assert_eq!(
        decide(&platform, &snapshot, platform_app, &update).await,
        Decision::Deny,
        "the narrowed selector replaces the broad one where the value matches"
    );

    let devops = engine(
        store.clone(),
        FakeMemberships::groups(&["group:acme/devops"]),
    );
    let snapshot = snapshot_for(&devops, "user:u-bob").await;
    assert_eq!(
        decide(&devops, &snapshot, other_app, &update).await,
        Decision::Allow,
        "resources carrying another value keep the broad rule undiminished"
    );
}

#[tokio::test]
async fn collections_are_filtered_per_item_with_independent_read_granularity() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    let owned = builder.labeled(
        PROJECT,
        "owned",
        Some(acme),
        &[("rise.dev/owner", "group:platform")],
    );
    let other = builder.resource(PROJECT, "other", Some(acme));
    let beta = builder.resource(ORGANIZATION, "beta", None);
    let foreign = builder.resource(PROJECT, "foreign", Some(beta));
    builder.role(
        ROLE,
        "project-lister",
        Some(acme),
        json!([{ "effect": "Allow", "kinds": ["rise.dev/Project"], "verbs": ["list"] }]),
    );
    builder.role(
        PLATFORM_ROLE,
        "resource-owner",
        None,
        json!([{
            "effect": "Allow", "kinds": "*", "verbs": ["get", "list", "update", "delete"]
        }]),
    );
    builder.binding(
        ROLE_BINDING,
        "org-wide-list",
        Some(acme),
        json!({
            "subject": "system:authenticated",
            "scope": "rise.dev/Organization/acme",
            "roleRef": { "kind": "Role", "name": "project-lister" }
        }),
    );
    builder.binding(
        PLATFORM_ROLE_BINDING,
        "resource-owner",
        None,
        json!({
            "subject": "${ref.subject}",
            "subjectMembership": "ResourceOrganization",
            "scope": "*",
            "labelSelector": { "key": "rise.dev/owner" },
            "roleRef": { "kind": "PlatformRole", "name": "resource-owner" }
        }),
    );
    let store = builder.build();
    let engine = engine(
        store.clone(),
        FakeMemberships::groups(&["group:acme/platform"]),
    );
    let snapshot = snapshot_for(&engine, "user:u-alice").await;

    let acme_node = store.node(acme);
    let decisions = engine
        .filter_list(
            &snapshot,
            &[acme_node],
            &[
                ListCandidate {
                    uid: owned,
                    node: store.node(owned),
                },
                ListCandidate {
                    uid: other,
                    node: store.node(other),
                },
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        decisions[0],
        rise_authz::engine::ListDecision {
            uid: owned,
            listable: true,
            readable: true
        }
    );
    assert_eq!(
        decisions[1],
        rise_authz::engine::ListDecision {
            uid: other,
            listable: true,
            readable: false
        },
        "list exposes the name and labels; get stays narrow to owned projects"
    );

    let foreign_decisions = engine
        .filter_list(
            &snapshot,
            &[store.node(beta)],
            &[ListCandidate {
                uid: foreign,
                node: store.node(foreign),
            }],
        )
        .await
        .unwrap();
    assert!(!foreign_decisions[0].listable && !foreign_decisions[0].readable);
}

#[tokio::test]
async fn a_token_ceiling_narrows_every_caller_including_operators() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    let app = builder.resource(PROJECT, "app", Some(acme));
    let deployment = builder.resource(DEPLOYMENT, "foo", Some(app));
    let catalog = builder.resource(PROJECT, "catalog", Some(acme));
    let other_deployment = builder.resource(DEPLOYMENT, "bar", Some(catalog));
    let store = builder.build();

    let capped = AuthenticatedPrincipal::new(
        "user:u-root".parse().unwrap(),
        Uuid::new_v4(),
        AuthorizationCap::Restricted(vec![CapEntry {
            scope: "rise.dev/Project/acme/app".parse().unwrap(),
            permissions: vec![CapPermission {
                verbs: serde_json::from_value(json!(["get"])).unwrap(),
                kinds: serde_json::from_value(json!(["rise.dev/Deployment"])).unwrap(),
                subresources: None,
            }],
        }]),
    )
    .unwrap();

    let engine = engine(store.clone(), FakeMemberships::operator());
    let snapshot = engine.snapshot(capped).await.unwrap();

    let target = engine.resource_tree(deployment).await.unwrap();
    let policy = engine.effective_policy(&snapshot, &target).await.unwrap();
    assert!(policy.is_operator());
    assert_eq!(
        policy.decide(&tuple(Verb::Get, "rise.dev/Deployment")),
        Decision::Allow
    );
    assert_eq!(
        policy.decide(&tuple(Verb::Delete, "rise.dev/Deployment")),
        Decision::Deny,
        "a ceiling never grants, and it narrows an operator like anyone else"
    );

    let outside = engine.resource_tree(other_deployment).await.unwrap();
    assert_eq!(
        engine
            .authorize(
                &snapshot,
                &outside,
                &tuple(Verb::Get, "rise.dev/Deployment")
            )
            .await
            .unwrap(),
        Decision::Deny,
        "no detail entry covers this scope"
    );
}

/// The snapshot memoizes behind mutexes, so this pins the property the HTTP
/// choke point will need: no guard is ever held across an await.
#[tokio::test]
async fn evaluation_futures_and_snapshots_cross_thread_boundaries() {
    fn assert_send<T: Send>(value: T) -> T {
        value
    }

    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    let app = builder.resource(PROJECT, "app", Some(acme));
    let store = builder.build();
    let engine = engine(store, FakeMemberships::none());

    let snapshot = assert_send(engine.snapshot(principal("user:u-alice")))
        .await
        .unwrap();
    let target = assert_send(engine.resource_tree(app)).await.unwrap();
    let handle = tokio::spawn(async move {
        let policy = engine.effective_policy(&snapshot, &target).await.unwrap();
        policy.decide(&tuple(Verb::Get, "rise.dev/Project"))
    });
    assert_eq!(handle.await.unwrap(), Decision::Deny);
}

#[tokio::test]
async fn explain_reports_ignored_denies_with_their_provenance() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    let app = builder.resource(PROJECT, "app", Some(acme));
    builder.role(PLATFORM_ROLE, ORG_ADMIN_PLATFORM_ROLE, None, allow_all());
    builder.role(
        ROLE,
        "org-cap",
        Some(acme),
        json!([{ "effect": "Deny", "kinds": ["rise.dev/Project"], "verbs": ["delete"] }]),
    );
    let admin_binding = builder.binding(
        ROLE_BINDING,
        "admin",
        Some(acme),
        org_admin_binding("user:u-alice", "acme"),
    );
    let cap_binding = builder.binding(
        ROLE_BINDING,
        "org-cap",
        Some(acme),
        json!({
            "subject": "user:u-alice",
            "scope": "rise.dev/Organization/acme",
            "roleRef": { "kind": "Role", "name": "org-cap" }
        }),
    );
    let store = builder.build();
    let engine = engine(store.clone(), FakeMemberships::none());
    let snapshot = snapshot_for(&engine, "user:u-alice").await;
    let target = engine.resource_tree(app).await.unwrap();
    let explanation = engine
        .explain(&snapshot, &target, &tuple(Verb::Delete, "rise.dev/Project"))
        .await
        .unwrap();

    assert_eq!(explanation.decision, Decision::Allow);
    assert!(!explanation.operator_override);
    let allow = explanation
        .contributions
        .iter()
        .find(|contribution| contribution.provenance.uid == admin_binding)
        .expect("the admin grant is recorded");
    assert_eq!(allow.retention, Retention::Retained);
    let deny = explanation
        .contributions
        .iter()
        .find(|contribution| contribution.provenance.uid == cap_binding)
        .expect("the ignored Deny is still recorded");
    assert_eq!(
        deny.retention,
        Retention::ExemptDeny(DenyExemption::OrganizationAdmin)
    );
    assert_eq!(deny.provenance.role.name, "org-cap");
}

#[tokio::test]
async fn tombstoned_bindings_and_dangling_role_refs_contribute_nothing() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    let app = builder.resource(PROJECT, "app", Some(acme));
    builder.role(PLATFORM_ROLE, "everything", None, allow_all());
    let deleted = builder.binding(
        ROLE_BINDING,
        "deleted",
        Some(acme),
        json!({
            "subject": "group:acme/platform",
            "scope": "rise.dev/Organization/acme",
            "roleRef": { "kind": "PlatformRole", "name": "everything" }
        }),
    );
    builder.tombstone(deleted);
    builder.binding(
        ROLE_BINDING,
        "dangling",
        Some(acme),
        json!({
            "subject": "group:acme/platform",
            "scope": "rise.dev/Organization/acme",
            "roleRef": { "kind": "Role", "name": "role-that-was-deleted" }
        }),
    );
    let store = builder.build();
    let engine = engine(
        store.clone(),
        FakeMemberships::groups(&["group:acme/platform"]),
    );
    let snapshot = snapshot_for(&engine, "user:u-alice").await;
    let target = engine.resource_tree(app).await.unwrap();
    let policy = engine.effective_policy(&snapshot, &target).await.unwrap();

    assert_eq!(
        policy.decide(&tuple(Verb::Get, "rise.dev/Project")),
        Decision::Deny
    );
    assert!(
        policy.contributions().is_empty(),
        "a dangling reference resolves to no statements rather than an error"
    );
}

#[tokio::test]
async fn corrupt_stored_policy_fails_the_request() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    let app = builder.resource(PROJECT, "app", Some(acme));
    builder.binding(
        ROLE_BINDING,
        "unnormalized",
        Some(acme),
        json!({
            "subject": "group:acme/platform",
            "roleRef": { "kind": "Role", "name": "whatever" }
        }),
    );
    let store = builder.build();
    let engine = engine(store.clone(), FakeMemberships::none());
    let snapshot = snapshot_for(&engine, "user:u-alice").await;
    let target = engine.resource_tree(app).await.unwrap();

    assert!(matches!(
        engine.effective_policy(&snapshot, &target).await,
        Err(AuthorizationError::CorruptPolicy(_))
    ));
}

#[tokio::test]
async fn one_user_may_administer_several_organizations_independently() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    let acme_app = builder.resource(PROJECT, "app", Some(acme));
    let beta = builder.resource(ORGANIZATION, "beta", None);
    let beta_app = builder.resource(PROJECT, "app", Some(beta));
    builder.role(PLATFORM_ROLE, ORG_ADMIN_PLATFORM_ROLE, None, allow_all());
    for org in ["acme", "beta"] {
        let parent = if org == "acme" { acme } else { beta };
        builder.binding(
            ROLE_BINDING,
            "admin",
            Some(parent),
            org_admin_binding("user:u-alice", org),
        );
    }
    builder.role(
        ROLE,
        "no-delete",
        Some(beta),
        json!([{ "effect": "Deny", "kinds": ["rise.dev/Project"], "verbs": ["delete"] }]),
    );
    builder.binding(
        ROLE_BINDING,
        "beta-cap",
        Some(beta),
        json!({
            "subject": "user:u-alice",
            "scope": "rise.dev/Organization/beta",
            "roleRef": { "kind": "Role", "name": "no-delete" }
        }),
    );
    builder.role(
        PLATFORM_ROLE,
        "acme-ceiling",
        None,
        json!([{ "effect": "Deny", "kinds": ["rise.dev/Project"], "verbs": ["delete"] }]),
    );
    builder.binding(
        PLATFORM_ROLE_BINDING,
        "acme-ceiling",
        None,
        json!({
            "subject": "system:authenticated",
            "subjectMembership": "Any",
            "scope": "rise.dev/Organization/acme",
            "roleRef": { "kind": "PlatformRole", "name": "acme-ceiling" }
        }),
    );
    let store = builder.build();
    let engine = engine(store.clone(), FakeMemberships::none());
    let snapshot = snapshot_for(&engine, "user:u-alice").await;
    let delete = tuple(Verb::Delete, "rise.dev/Project");

    assert_eq!(
        decide(&engine, &snapshot, beta_app, &delete).await,
        Decision::Allow,
        "beta's own Deny does not cap beta's admin"
    );
    assert_eq!(
        decide(&engine, &snapshot, acme_app, &delete).await,
        Decision::Deny,
        "acme's platform ceiling does"
    );
}

#[tokio::test]
async fn only_identity_resources_can_authenticate() {
    assert!(AuthenticatedPrincipal::new(
        "group:acme/platform".parse().unwrap(),
        Uuid::new_v4(),
        AuthorizationCap::Unrestricted
    )
    .is_err());
    assert!(AuthenticatedPrincipal::new(
        "system:operators".parse().unwrap(),
        Uuid::new_v4(),
        AuthorizationCap::Unrestricted
    )
    .is_err());
    assert!(AuthenticatedPrincipal::new(
        "serviceaccount:acme/ci".parse().unwrap(),
        Uuid::new_v4(),
        AuthorizationCap::Unrestricted
    )
    .is_ok());
}

#[tokio::test]
async fn a_scope_matches_only_its_own_subtree() {
    let organization = ResourceNode::new("rise.dev/Organization".parse().unwrap(), "acme");
    let project = ResourceNode::new("rise.dev/Project".parse().unwrap(), "app");
    let environment = ResourceNode::new("rise.dev/Environment".parse().unwrap(), "prod");
    let tree = ResourceTree::new(vec![organization, project, environment]).unwrap();

    for scope in [
        "*",
        "rise.dev/Organization/acme",
        "rise.dev/Project/acme/app",
        "rise.dev/Environment/acme/app/prod",
    ] {
        assert!(tree.covered_by(&scope.parse().unwrap()), "{scope}");
    }
    for scope in [
        "rise.dev/Organization/beta",
        "rise.dev/Project/acme/other",
        "rise.dev/Environment/acme/app/prod/deeper",
        // Right names, wrong kind at the scope's own node.
        "rise.dev/Deployment/acme/app",
    ] {
        assert!(!tree.covered_by(&scope.parse().unwrap()), "{scope}");
    }
}
