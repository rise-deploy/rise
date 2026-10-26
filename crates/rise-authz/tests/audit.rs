mod support;

use std::collections::BTreeMap;
use std::sync::Arc;

use rise_authz::engine::{
    AuditScope, AuthorizationEngine, FindingCategory, Severity, Shadow, ORG_ADMIN_PLATFORM_ROLE,
};
use rise_resource_api::ResourceStore;
use serde_json::json;
use support::{FakeMemberships, FakeStore, StoreBuilder};

const ORGANIZATION: &str = "Organization";
const PROJECT: &str = "Project";
const ROLE: &str = "Role";
const ROLE_BINDING: &str = "RoleBinding";
const PLATFORM_ROLE: &str = "PlatformRole";
const PLATFORM_ROLE_BINDING: &str = "PlatformRoleBinding";
const USER: &str = "User";
const GROUP: &str = "Group";
const GROUP_MEMBERSHIP: &str = "GroupMembership";

fn engine(store: Arc<FakeStore>, memberships: Arc<FakeMemberships>) -> AuthorizationEngine {
    AuthorizationEngine::new(store as Arc<dyn ResourceStore>, memberships)
}

/// Audit every live Organization with a generous cap.
fn every_org() -> AuditScope {
    AuditScope {
        organization: None,
        max_findings: 1000,
    }
}

fn one_org(name: &str) -> AuditScope {
    AuditScope {
        organization: Some(name.to_owned()),
        max_findings: 1000,
    }
}

/// An org `RoleBinding` that satisfies ADR-0001 §5's exact structural admin
/// predicate.
fn org_admin_binding(subject: &str, organization: &str) -> serde_json::Value {
    json!({
        "subject": subject,
        "scope": format!("rise.dev/Organization/{organization}"),
        "roleRef": { "kind": "PlatformRole", "name": ORG_ADMIN_PLATFORM_ROLE }
    })
}

fn user_spec(active: bool) -> serde_json::Value {
    json!({ "active": active })
}

fn allow_all() -> serde_json::Value {
    json!([
        { "effect": "Allow", "kinds": "*", "verbs": "*" },
        { "effect": "Allow", "kinds": "*", "verbs": "*", "subresources": "*" }
    ])
}

fn category_names(
    findings: &[rise_authz::engine::AuditFinding],
    category: FindingCategory,
) -> Vec<&str> {
    findings
        .iter()
        .filter(|finding| finding.category == category)
        .map(|finding| finding.subject.name.as_str())
        .collect()
}

/// The seeded shape a healthy install ships: a `PlatformRole/org-admin`, one
/// Organization, and one qualifying direct admin binding to a live, active
/// User.
#[tokio::test]
async fn a_healthy_install_produces_no_findings() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    builder.role(PLATFORM_ROLE, ORG_ADMIN_PLATFORM_ROLE, None, allow_all());
    builder.row(USER, "u-alice", None, BTreeMap::new(), user_spec(true));
    builder.binding(
        ROLE_BINDING,
        "admin",
        Some(acme),
        org_admin_binding("user:u-alice", "acme"),
    );
    let store = builder.build();
    let engine = engine(store, FakeMemberships::none());

    let report = engine.audit(&every_org()).await.unwrap();
    assert!(report.findings.is_empty(), "{:#?}", report.findings);
    assert!(!report.truncated);
}

#[tokio::test]
async fn dangling_role_ref_positive_and_negative() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    builder.role(
        ROLE,
        "reader",
        Some(acme),
        json!([{ "effect": "Allow", "kinds": "*", "verbs": ["get"] }]),
    );
    builder.binding(
        ROLE_BINDING,
        "resolved",
        Some(acme),
        json!({
            "subject": "user:u-alice",
            "scope": "rise.dev/Organization/acme",
            "roleRef": { "kind": "Role", "name": "reader" }
        }),
    );
    builder.binding(
        ROLE_BINDING,
        "dangling",
        Some(acme),
        json!({
            "subject": "user:u-alice",
            "scope": "rise.dev/Organization/acme",
            "roleRef": { "kind": "Role", "name": "role-that-does-not-exist" }
        }),
    );
    let store = builder.build();
    let engine = engine(store, FakeMemberships::none());

    let report = engine.audit(&one_org("acme")).await.unwrap();
    let dangling = category_names(&report.findings, FindingCategory::DanglingRoleRef);
    assert_eq!(dangling, vec!["dangling"]);
    assert_eq!(
        report
            .findings
            .iter()
            .find(|finding| finding.category == FindingCategory::DanglingRoleRef)
            .unwrap()
            .severity,
        Severity::Warning
    );
}

#[tokio::test]
async fn stale_subject_positive_and_negative() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    builder.role(
        ROLE,
        "reader",
        Some(acme),
        json!([{ "effect": "Allow", "kinds": "*", "verbs": ["get"] }]),
    );
    builder.row(USER, "u-live", None, BTreeMap::new(), user_spec(true));
    builder.row(USER, "u-inactive", None, BTreeMap::new(), user_spec(false));
    for (name, subject) in [
        ("live", "user:u-live"),
        ("inactive", "user:u-inactive"),
        ("missing", "user:u-ghost"),
    ] {
        builder.binding(
            ROLE_BINDING,
            name,
            Some(acme),
            json!({
                "subject": subject,
                "scope": "rise.dev/Organization/acme",
                "roleRef": { "kind": "Role", "name": "reader" }
            }),
        );
    }
    let store = builder.build();
    let engine = engine(store, FakeMemberships::none());

    let report = engine.audit(&one_org("acme")).await.unwrap();
    let mut stale = category_names(&report.findings, FindingCategory::StaleSubject);
    stale.sort_unstable();
    assert_eq!(stale, vec!["inactive", "missing"]);
}

#[tokio::test]
async fn stale_scope_positive_and_negative() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    let _app = builder.resource(PROJECT, "app", Some(acme));
    builder.role(
        ROLE,
        "reader",
        Some(acme),
        json!([{ "effect": "Allow", "kinds": "*", "verbs": ["get"] }]),
    );
    builder.binding(
        ROLE_BINDING,
        "live-scope",
        Some(acme),
        json!({
            "subject": "user:u-alice",
            "scope": "rise.dev/Project/acme/app",
            "roleRef": { "kind": "Role", "name": "reader" }
        }),
    );
    builder.binding(
        ROLE_BINDING,
        "stale-scope",
        Some(acme),
        json!({
            "subject": "user:u-alice",
            "scope": "rise.dev/Project/acme/ghost",
            "roleRef": { "kind": "Role", "name": "reader" }
        }),
    );
    let store = builder.build();
    let engine = engine(store, FakeMemberships::none());

    let report = engine.audit(&one_org("acme")).await.unwrap();
    let stale = category_names(&report.findings, FindingCategory::StaleScope);
    assert_eq!(stale, vec!["stale-scope"]);
}

#[tokio::test]
async fn stale_membership_positive_and_negative() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    let platform = builder.resource(GROUP, "platform", Some(acme));
    builder.row(USER, "u-live", None, BTreeMap::new(), user_spec(true));
    // A live tie: the GroupMembership names a live, active User.
    builder.row(
        GROUP_MEMBERSHIP,
        "u-live",
        Some(platform),
        BTreeMap::new(),
        json!({}),
    );
    // A stale tie: names a User that was never created.
    builder.row(
        GROUP_MEMBERSHIP,
        "u-ghost",
        Some(platform),
        BTreeMap::new(),
        json!({}),
    );
    let store = builder.build();
    let engine = engine(store, FakeMemberships::none());

    let report = engine.audit(&one_org("acme")).await.unwrap();
    let stale = category_names(&report.findings, FindingCategory::StaleMembership);
    assert_eq!(stale, vec!["u-ghost"]);
    assert_eq!(
        report
            .findings
            .iter()
            .find(|finding| finding.category == FindingCategory::StaleMembership)
            .unwrap()
            .severity,
        Severity::Info
    );
}

#[tokio::test]
async fn no_op_membership_constraint_positive_and_negative() {
    let mut builder = StoreBuilder::new();
    let _acme = builder.resource(ORGANIZATION, "acme", None);
    builder.role(PLATFORM_ROLE, "everything", None, allow_all());
    // Negative: a `user:` subject genuinely spans organizations, so the clamp
    // is a live constraint.
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
    // Positive: a Controller belongs to no organization, so the clamp can
    // never bite.
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
    // Positive: a root-scoped, non-Organization scope has no organization to
    // compare the resolved subject against.
    builder.binding(
        PLATFORM_ROLE_BINDING,
        "clamped-root-scope",
        None,
        json!({
            "subject": "user:u-bob",
            "subjectMembership": "ResourceOrganization",
            "scope": "rise.dev/RuntimeClass/gpu",
            "roleRef": { "kind": "PlatformRole", "name": "everything" }
        }),
    );
    let store = builder.build();
    let engine = engine(store, FakeMemberships::none());

    let report = engine.audit(&every_org()).await.unwrap();
    let mut no_op = category_names(&report.findings, FindingCategory::NoOpMembershipConstraint);
    no_op.sort_unstable();
    assert_eq!(no_op, vec!["clamped-controller", "clamped-root-scope"]);
}

#[tokio::test]
async fn recipient_boundary_no_op_positive_and_negative() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    builder.role(
        ROLE,
        "reader",
        Some(acme),
        json!([{ "effect": "Allow", "kinds": "*", "verbs": ["get"] }]),
    );
    builder.binding(
        ROLE_BINDING,
        "controller-subject",
        Some(acme),
        json!({
            "subject": "controller:reconciler",
            "scope": "rise.dev/Organization/acme",
            "roleRef": { "kind": "Role", "name": "reader" }
        }),
    );
    builder.binding(
        ROLE_BINDING,
        "unaffiliated-user",
        Some(acme),
        json!({
            "subject": "user:u-bob",
            "scope": "rise.dev/Organization/acme",
            "roleRef": { "kind": "Role", "name": "reader" }
        }),
    );
    builder.binding(
        ROLE_BINDING,
        "affiliated-user",
        Some(acme),
        json!({
            "subject": "user:u-alice",
            "scope": "rise.dev/Organization/acme",
            "roleRef": { "kind": "Role", "name": "reader" }
        }),
    );
    let store = builder.build();
    let memberships =
        FakeMemberships::none().with_user_groups("user:u-alice", &["group:acme/platform"]);
    let engine = engine(store, memberships);

    let report = engine.audit(&one_org("acme")).await.unwrap();
    let flagged: Vec<(&str, Severity)> = report
        .findings
        .iter()
        .filter(|finding| finding.category == FindingCategory::RecipientBoundaryNoOp)
        .map(|finding| (finding.subject.name.as_str(), finding.severity))
        .collect();
    assert!(flagged.contains(&("controller-subject", Severity::Warning)));
    assert!(flagged.contains(&("unaffiliated-user", Severity::Info)));
    assert!(!flagged.iter().any(|(name, _)| *name == "affiliated-user"));
}

#[tokio::test]
async fn inert_owner_label_positive_and_negative() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    // A Group named `leads` exists, but under a *different* organization. The
    // seeded template always substitutes the resource's own organization when
    // resolving a relative `group:` value, so `group:leads` set on a resource
    // in `acme` can never reach it.
    let other_org = builder.resource(ORGANIZATION, "other-org", None);
    let _foreign_leads = builder.resource(GROUP, "leads", Some(other_org));

    builder.row(USER, "u-alice", None, BTreeMap::new(), user_spec(true));
    builder.row(USER, "u-bob", None, BTreeMap::new(), user_spec(true));

    builder.labeled(
        PROJECT,
        "malformed",
        Some(acme),
        &[("rise.dev/owner", "not-a-subject")],
    );
    builder.labeled(
        PROJECT,
        "foreign-group",
        Some(acme),
        &[("rise.dev/owner", "group:leads")],
    );
    builder.labeled(
        PROJECT,
        "unaffiliated",
        Some(acme),
        &[("rise.dev/owner", "user:u-bob")],
    );
    builder.labeled(
        PROJECT,
        "affiliated",
        Some(acme),
        &[("rise.dev/owner", "user:u-alice")],
    );

    let store = builder.build();
    let memberships =
        FakeMemberships::none().with_user_groups("user:u-alice", &["group:acme/platform"]);
    let engine = engine(store, memberships);

    let report = engine.audit(&one_org("acme")).await.unwrap();
    let flagged: Vec<(&str, Severity)> = report
        .findings
        .iter()
        .filter(|finding| finding.category == FindingCategory::InertOwnerLabel)
        .map(|finding| (finding.subject.name.as_str(), finding.severity))
        .collect();
    assert!(flagged.contains(&("malformed", Severity::Warning)));
    assert!(flagged.contains(&("foreign-group", Severity::Warning)));
    assert!(flagged.contains(&("unaffiliated", Severity::Info)));
    assert!(
        !flagged.iter().any(|(name, _)| *name == "affiliated"),
        "{flagged:#?}"
    );
    assert_eq!(flagged.len(), 3, "{flagged:#?}");
}

#[tokio::test]
async fn selector_matches_nothing_positive_and_negative() {
    fn selector_binding(
        scope: &str,
        role: &str,
        key: &str,
        value: Option<&str>,
    ) -> serde_json::Value {
        let mut label_selector = json!({ "key": key });
        if let Some(value) = value {
            label_selector["value"] = json!(value);
        }
        json!({
            "subject": "user:u-alice",
            "scope": scope,
            "roleRef": { "kind": "Role", "name": role },
            "labelSelector": label_selector
        })
    }

    let mut builder = StoreBuilder::new();
    // The Organization itself sets one key (ancestor-match) and one more
    // (wildcard-match).
    let acme = builder.labeled(
        ORGANIZATION,
        "acme",
        None,
        &[("rise.dev/team", "x"), ("rise.dev/anything", "v")],
    );
    builder.role(
        ROLE,
        "reader",
        Some(acme),
        json!([{ "effect": "Allow", "kinds": "*", "verbs": ["get"] }]),
    );
    builder.role(PLATFORM_ROLE, "everything", None, allow_all());

    // Scenario A: the key is set on an ancestor of the scope root (the
    // Organization), so nearest-wins inheritance answers it directly.
    let app = builder.resource(PROJECT, "app", Some(acme));
    builder.binding(
        ROLE_BINDING,
        "ancestor-match",
        Some(acme),
        selector_binding("rise.dev/Project/acme/app", "reader", "rise.dev/team", None),
    );
    let _ = app;

    // Scenario B: the key is set on a descendant *inside* the scope, which the
    // root's own effective labels do not carry, so the setter scan finds it.
    let app2 = builder.resource(PROJECT, "app2", Some(acme));
    builder.labeled(
        "Environment",
        "prod",
        Some(app2),
        &[("rise.dev/stage", "beta")],
    );
    builder.binding(
        ROLE_BINDING,
        "descendant-match",
        Some(acme),
        selector_binding(
            "rise.dev/Project/acme/app2",
            "reader",
            "rise.dev/stage",
            Some("beta"),
        ),
    );

    // Scenario C: the only setter of the key lies outside the binding's scope
    // (a sibling Project, not a descendant of the scope root).
    let app3 = builder.resource(PROJECT, "app3", Some(acme));
    builder.labeled(PROJECT, "far", Some(acme), &[("rise.dev/region", "us")]);
    builder.binding(
        ROLE_BINDING,
        "outside-scope",
        Some(acme),
        selector_binding(
            "rise.dev/Project/acme/app3",
            "reader",
            "rise.dev/region",
            Some("us"),
        ),
    );
    let _ = app3;

    // Scenario D: the scope root sets the key, but with a different value than
    // the selector requires.
    let _app4 = builder.labeled(PROJECT, "app4", Some(acme), &[("rise.dev/env", "staging")]);
    builder.binding(
        ROLE_BINDING,
        "value-mismatch",
        Some(acme),
        selector_binding(
            "rise.dev/Project/acme/app4",
            "reader",
            "rise.dev/env",
            Some("production"),
        ),
    );

    // Scenario E: a wildcard-scoped binding, matched by a setter anywhere.
    builder.binding(
        PLATFORM_ROLE_BINDING,
        "wildcard-match",
        None,
        json!({
            "subject": "system:authenticated",
            "subjectMembership": "Any",
            "scope": "*",
            "labelSelector": { "key": "rise.dev/anything" },
            "roleRef": { "kind": "PlatformRole", "name": "everything" }
        }),
    );

    // Scenario F: a dynamic-subject binding selecting on a key nobody sets.
    // The seeded ownership binding has exactly this shape on a fresh install,
    // so it is reported at `Info`, not `Warning`.
    builder.binding(
        PLATFORM_ROLE_BINDING,
        "template-unmatched",
        None,
        json!({
            "subject": "${ref.subject}",
            "subjectMembership": "ResourceOrganization",
            "scope": "*",
            "labelSelector": { "key": "rise.dev/owner" },
            "roleRef": { "kind": "PlatformRole", "name": "everything" }
        }),
    );

    let store = builder.build();
    let engine = engine(store, FakeMemberships::none());

    let report = engine.audit(&every_org()).await.unwrap();
    let mut flagged = category_names(&report.findings, FindingCategory::SelectorMatchesNothing);
    flagged.sort_unstable();
    assert_eq!(
        flagged,
        vec!["outside-scope", "template-unmatched", "value-mismatch"],
        "{:#?}",
        report.findings
    );
    let severity_of = |name: &str| {
        report
            .findings
            .iter()
            .find(|finding| {
                finding.category == FindingCategory::SelectorMatchesNothing
                    && finding.subject.name == name
            })
            .map(|finding| finding.severity)
            .unwrap()
    };
    assert_eq!(severity_of("outside-scope"), Severity::Warning);
    assert_eq!(severity_of("template-unmatched"), Severity::Info);
}

#[tokio::test]
async fn shadowed_by_deny_positive_and_negative() {
    fn deny_binding(name: &str, scope: &str) -> serde_json::Value {
        json!({
            "subject": "system:authenticated",
            "subjectMembership": "Any",
            "scope": scope,
            "roleRef": { "kind": "PlatformRole", "name": format!("{name}-role") }
        })
    }

    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    let other_org = builder.resource(ORGANIZATION, "other-org", None);

    // Three platform-tier Denies, each covering a different slice, so the
    // positive and negative cases can be told apart by which one is (or is
    // not) named in a finding's `related`.
    builder.role(
        PLATFORM_ROLE,
        "covers-role",
        None,
        json!([{ "effect": "Deny", "kinds": "*", "verbs": ["get", "list"] }]),
    );
    builder.binding(
        PLATFORM_ROLE_BINDING,
        "covers",
        None,
        deny_binding("covers", "*"),
    );
    // Negative: narrower on verbs than any Allow it is tested against below.
    builder.role(
        PLATFORM_ROLE,
        "narrow-role",
        None,
        json!([{ "effect": "Deny", "kinds": "*", "verbs": ["delete"] }]),
    );
    builder.binding(
        PLATFORM_ROLE_BINDING,
        "narrow",
        None,
        deny_binding("narrow", "*"),
    );
    // Negative: a concrete-scope Deny cannot cover a wildcard-scoped Allow,
    // regardless of how permissive it otherwise is.
    builder.role(
        PLATFORM_ROLE,
        "scoped-role",
        None,
        json!([{ "effect": "Deny", "kinds": "*", "verbs": "*" }]),
    );
    builder.binding(
        PLATFORM_ROLE_BINDING,
        "scoped",
        None,
        deny_binding("scoped", "rise.dev/Organization/acme"),
    );

    // Positive: an org Allow fully covered by both `covers` (identical verbs)
    // and `scoped` (wildcard verbs, matching concrete scope); `narrow` misses
    // it because "list" is not among the verbs it denies.
    builder.role(
        ROLE,
        "reader",
        Some(acme),
        json!([{ "effect": "Allow", "kinds": "*", "verbs": ["get", "list"] }]),
    );
    builder.binding(
        ROLE_BINDING,
        "shadowed",
        Some(acme),
        json!({
            "subject": "user:u-alice",
            "scope": "rise.dev/Organization/acme",
            "roleRef": { "kind": "Role", "name": "reader" }
        }),
    );

    // Negative: a platform-tier Allow with a wildcard scope. `scoped`'s
    // concrete scope can never cover it, and its verb ("update") is missed by
    // both `covers` and `narrow`, so nothing shadows it.
    builder.role(
        PLATFORM_ROLE,
        "wide-allow-role",
        None,
        json!([{ "effect": "Allow", "kinds": "*", "verbs": ["update"] }]),
    );
    builder.binding(
        PLATFORM_ROLE_BINDING,
        "wide-allow",
        None,
        json!({
            "subject": "system:authenticated",
            "subjectMembership": "Any",
            "scope": "*",
            "roleRef": { "kind": "PlatformRole", "name": "wide-allow-role" }
        }),
    );

    // Negative: an org-tier Deny is never a shadowing candidate — only
    // `platform` bindings are, because an org Deny is escaped by that org's
    // own admins. Its Allow target's verb ("update") is also outside every
    // platform Deny's reach, so this row only turns up empty-handed.
    builder.role(
        ROLE,
        "org-ceiling",
        Some(other_org),
        json!([{ "effect": "Deny", "kinds": "*", "verbs": "*" }]),
    );
    builder.binding(
        ROLE_BINDING,
        "org-deny",
        Some(other_org),
        json!({
            "subject": "system:authenticated",
            "scope": "rise.dev/Organization/other-org",
            "roleRef": { "kind": "Role", "name": "org-ceiling" }
        }),
    );
    builder.role(
        ROLE,
        "updater",
        Some(other_org),
        json!([{ "effect": "Allow", "kinds": "*", "verbs": ["update"] }]),
    );
    builder.binding(
        ROLE_BINDING,
        "clean-target",
        Some(other_org),
        json!({
            "subject": "user:u-dave",
            "scope": "rise.dev/Organization/other-org",
            "roleRef": { "kind": "Role", "name": "updater" }
        }),
    );

    let store = builder.build();
    let engine = engine(store, FakeMemberships::none());

    let report = engine.audit(&every_org()).await.unwrap();
    let mut flagged: Vec<(&str, &str)> = report
        .findings
        .iter()
        .filter(|finding| finding.category == FindingCategory::ShadowedAllow { by: Shadow::Deny })
        .map(|finding| {
            (
                finding.subject.name.as_str(),
                finding.related[0].name.as_str(),
            )
        })
        .collect();
    flagged.sort_unstable();
    assert_eq!(
        flagged,
        vec![("shadowed", "covers"), ("shadowed", "scoped")],
        "{:#?}",
        report.findings
    );
    assert!(report
        .findings
        .iter()
        .filter(|finding| finding.category == FindingCategory::ShadowedAllow { by: Shadow::Deny })
        .all(|finding| finding.severity == Severity::Info));
}

#[tokio::test]
async fn shadowed_by_replacement_positive_and_negative() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);

    // A platform wildcard binding with an Allow, selecting on `rise.dev/team`.
    builder.role(PLATFORM_ROLE, "wildcard-role", None, allow_all());
    builder.binding(
        PLATFORM_ROLE_BINDING,
        "wildcard",
        None,
        json!({
            "subject": "user:u-alice",
            "subjectMembership": "Any",
            "scope": "*",
            "labelSelector": { "key": "rise.dev/team" },
            "roleRef": { "kind": "PlatformRole", "name": "wildcard-role" }
        }),
    );

    // Positive: same subject, same selector key, a non-wildcard scope — this
    // replaces the wildcard binding's Allows wherever both apply.
    builder.role(ROLE, "reader", Some(acme), allow_all());
    builder.binding(
        ROLE_BINDING,
        "specific",
        Some(acme),
        json!({
            "subject": "user:u-alice",
            "scope": "rise.dev/Organization/acme",
            "labelSelector": { "key": "rise.dev/team" },
            "roleRef": { "kind": "Role", "name": "reader" }
        }),
    );

    // Negative: same subject, but a different selector key never pairs up.
    builder.binding(
        ROLE_BINDING,
        "different-key",
        Some(acme),
        json!({
            "subject": "user:u-alice",
            "scope": "rise.dev/Organization/acme",
            "labelSelector": { "key": "rise.dev/other" },
            "roleRef": { "kind": "Role", "name": "reader" }
        }),
    );

    // Negative: same selector key, but a different subject never pairs up.
    builder.binding(
        ROLE_BINDING,
        "different-subject",
        Some(acme),
        json!({
            "subject": "user:u-bob",
            "scope": "rise.dev/Organization/acme",
            "labelSelector": { "key": "rise.dev/team" },
            "roleRef": { "kind": "Role", "name": "reader" }
        }),
    );

    let store = builder.build();
    let engine = engine(store, FakeMemberships::none());

    let report = engine.audit(&every_org()).await.unwrap();
    let flagged: Vec<(&str, &str)> = report
        .findings
        .iter()
        .filter(|finding| {
            finding.category
                == FindingCategory::ShadowedAllow {
                    by: Shadow::Replacement,
                }
        })
        .map(|finding| {
            (
                finding.subject.name.as_str(),
                finding.related[0].name.as_str(),
            )
        })
        .collect();
    assert_eq!(
        flagged,
        vec![("wildcard", "specific")],
        "{:#?}",
        report.findings
    );
    assert_eq!(
        report
            .findings
            .iter()
            .find(|finding| finding.subject.name == "wildcard")
            .unwrap()
            .severity,
        Severity::Warning
    );
}

#[tokio::test]
async fn organization_without_admin_positive_and_negative() {
    let mut builder = StoreBuilder::new();
    let no_admin = builder.resource(ORGANIZATION, "no-admin", None);
    let _ = no_admin;
    let direct_admin = builder.resource(ORGANIZATION, "direct-admin", None);
    let group_admin = builder.resource(ORGANIZATION, "group-admin", None);
    builder.role(PLATFORM_ROLE, ORG_ADMIN_PLATFORM_ROLE, None, allow_all());
    builder.row(USER, "u-alice", None, BTreeMap::new(), user_spec(true));
    builder.binding(
        ROLE_BINDING,
        "admin",
        Some(direct_admin),
        org_admin_binding("user:u-alice", "direct-admin"),
    );
    let leads = builder.resource(GROUP, "leads", Some(group_admin));
    builder.row(USER, "u-carol", None, BTreeMap::new(), user_spec(true));
    builder.row(
        GROUP_MEMBERSHIP,
        "u-carol",
        Some(leads),
        BTreeMap::new(),
        json!({}),
    );
    builder.binding(
        ROLE_BINDING,
        "admin",
        Some(group_admin),
        org_admin_binding("group:group-admin/leads", "group-admin"),
    );
    let store = builder.build();
    let engine = engine(store, FakeMemberships::none());

    let report = engine.audit(&every_org()).await.unwrap();
    let flagged = category_names(&report.findings, FindingCategory::OrganizationWithoutAdmin);
    assert_eq!(flagged, vec!["no-admin"]);
    assert_eq!(
        report
            .findings
            .iter()
            .find(|finding| finding.category == FindingCategory::OrganizationWithoutAdmin)
            .unwrap()
            .severity,
        Severity::Warning
    );
}

#[tokio::test]
async fn non_qualifying_org_admin_reference_positive_and_negative() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    builder.role(PLATFORM_ROLE, ORG_ADMIN_PLATFORM_ROLE, None, allow_all());
    builder.row(USER, "u-alice", None, BTreeMap::new(), user_spec(true));
    // Negative: exact org-root, scope-only.
    builder.binding(
        ROLE_BINDING,
        "qualifying",
        Some(acme),
        org_admin_binding("user:u-alice", "acme"),
    );
    // Positive: label-selected, so it can never confer admin standing.
    builder.binding(
        ROLE_BINDING,
        "selected",
        Some(acme),
        json!({
            "subject": "group:acme/whatever",
            "scope": "rise.dev/Organization/acme",
            "labelSelector": { "key": "rise.dev/squad", "value": "platform" },
            "roleRef": { "kind": "PlatformRole", "name": ORG_ADMIN_PLATFORM_ROLE }
        }),
    );
    // Positive: reached through a `PlatformRoleBinding`, which never confers
    // admin standing regardless of scope.
    builder.binding(
        PLATFORM_ROLE_BINDING,
        "platform-wide",
        None,
        json!({
            "subject": "system:authenticated",
            "subjectMembership": "Any",
            "scope": "rise.dev/Organization/acme",
            "roleRef": { "kind": "PlatformRole", "name": ORG_ADMIN_PLATFORM_ROLE }
        }),
    );
    let store = builder.build();
    let engine = engine(store, FakeMemberships::none());

    let report = engine.audit(&every_org()).await.unwrap();
    let mut flagged = category_names(
        &report.findings,
        FindingCategory::NonQualifyingOrgAdminReference,
    );
    flagged.sort_unstable();
    assert_eq!(flagged, vec!["platform-wide", "selected"]);
}

#[tokio::test]
async fn max_findings_truncates_and_sets_truncated() {
    let mut builder = StoreBuilder::new();
    let acme = builder.resource(ORGANIZATION, "acme", None);
    for i in 0..5 {
        builder.binding(
            ROLE_BINDING,
            &format!("dangling-{i}"),
            Some(acme),
            json!({
                "subject": "user:u-alice",
                "scope": "rise.dev/Organization/acme",
                "roleRef": { "kind": "Role", "name": "role-that-does-not-exist" }
            }),
        );
    }
    let store = builder.build();
    let engine = engine(store, FakeMemberships::none());

    let capped = engine
        .audit(&AuditScope {
            organization: Some("acme".to_owned()),
            max_findings: 2,
        })
        .await
        .unwrap();
    assert_eq!(capped.findings.len(), 2);
    assert!(capped.truncated);

    let full = engine.audit(&one_org("acme")).await.unwrap();
    assert!(full.findings.len() > 2, "{:#?}", full.findings);
    assert!(!full.truncated);
}
