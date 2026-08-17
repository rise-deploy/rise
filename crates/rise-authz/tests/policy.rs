use rise_authz::policy::{
    apply_wildcard_replacement, domain_covers, domain_covers_with, evaluate,
    newly_allowed_is_subset, policy_is_subset, resolve_subject, scoped_newly_allowed_is_subset,
    scoped_newly_allowed_is_subset_with, ApplicableBinding, BindingTier, Decision, PermissionTuple,
    PolicyDomain,
};
use rise_resource_api::{
    BindingSubject, Effect, LabelSelector, PolicyStatement, ResourceKind, Scope, SubresourceName,
    Verb,
};
use serde_json::json;

fn statement(value: serde_json::Value) -> PolicyStatement {
    serde_json::from_value(value).unwrap()
}

fn tuple(verb: Verb, kind: &str, subresource: Option<&str>) -> PermissionTuple {
    PermissionTuple {
        verb,
        kind: kind.parse().unwrap(),
        subresource: subresource.map(|name| name.parse().unwrap()),
    }
}

fn selector(key: &str, value: Option<&str>) -> LabelSelector {
    LabelSelector {
        key: key.parse().unwrap(),
        value: value.map(str::to_owned),
    }
}

#[test]
fn qualified_kind_matching_is_version_independent() {
    let exact = statement(json!({
        "effect": "Allow",
        "kinds": ["alpha.example/Widget"],
        "verbs": ["get"]
    }));
    assert_eq!(
        evaluate([&exact], &tuple(Verb::Get, "alpha.example/Widget", None)),
        Decision::Allow
    );
    assert_eq!(
        evaluate([&exact], &tuple(Verb::Get, "beta.example/Widget", None)),
        Decision::Deny
    );

    let v1 = ResourceKind::from_api_route("alpha.example/v1", "Widget").unwrap();
    let v2 = ResourceKind::from_api_route("alpha.example/v2", "Widget").unwrap();
    assert_eq!(v1, v2);
}

#[test]
fn group_and_global_kind_wildcards_have_distinct_reach() {
    let group = statement(json!({
        "effect": "Allow", "kinds": ["alpha.example/*"], "verbs": "*"
    }));
    let global = statement(json!({
        "effect": "Allow", "kinds": "*", "verbs": "*"
    }));
    let alpha = tuple(Verb::Use, "alpha.example/Gadget", None);
    let beta = tuple(Verb::Use, "beta.example/Gadget", None);
    assert_eq!(evaluate([&group], &alpha), Decision::Allow);
    assert_eq!(evaluate([&group], &beta), Decision::Deny);
    assert_eq!(evaluate([&global], &beta), Decision::Allow);
}

#[test]
fn main_and_subresource_permissions_are_disjoint() {
    let main = statement(json!({
        "effect": "Allow", "kinds": "*", "verbs": "*"
    }));
    let all_subresources = statement(json!({
        "effect": "Allow", "kinds": "*", "verbs": "*", "subresources": "*"
    }));
    let status_only = statement(json!({
        "effect": "Allow", "kinds": "*", "verbs": ["update"], "subresources": ["status"]
    }));
    let main_request = tuple(Verb::Update, "rise.dev/Deployment", None);
    let status_request = tuple(Verb::Update, "rise.dev/Deployment", Some("status"));
    let token_request = tuple(Verb::Create, "rise.dev/ServiceAccount", Some("token"));

    assert_eq!(evaluate([&main], &main_request), Decision::Allow);
    assert_eq!(evaluate([&main], &status_request), Decision::Deny);
    assert_eq!(evaluate([&all_subresources], &main_request), Decision::Deny);
    assert_eq!(
        evaluate([&all_subresources], &status_request),
        Decision::Allow
    );
    assert_eq!(evaluate([&status_only], &status_request), Decision::Allow);
    assert_eq!(evaluate([&status_only], &token_request), Decision::Deny);
}

#[test]
fn union_defaults_to_deny_and_matching_deny_wins() {
    let allow = statement(json!({
        "effect": "Allow", "kinds": ["rise.dev/*"], "verbs": ["get", "delete"]
    }));
    let deny = statement(json!({
        "effect": "Deny", "kinds": ["rise.dev/Environment"], "verbs": ["delete"]
    }));
    let get = tuple(Verb::Get, "rise.dev/Environment", None);
    let delete = tuple(Verb::Delete, "rise.dev/Environment", None);
    let update = tuple(Verb::Update, "rise.dev/Environment", None);
    assert_eq!(evaluate([&allow, &deny], &get), Decision::Allow);
    assert_eq!(evaluate([&allow, &deny], &delete), Decision::Deny);
    assert_eq!(evaluate([&allow, &deny], &update), Decision::Deny);
}

#[test]
fn subject_substitution_uses_the_closed_subject_ref_grammar() {
    assert_eq!(
        resolve_subject(
            &"group:${ref.name}".parse().unwrap(),
            Some("platform"),
            Some("acme")
        )
        .unwrap()
        .to_string(),
        "group:acme/platform"
    );
    assert_eq!(
        resolve_subject(
            &"${ref.subject}".parse().unwrap(),
            Some("group:platform"),
            Some("acme")
        )
        .unwrap()
        .to_string(),
        "group:acme/platform"
    );
    assert!(resolve_subject(
        &"${ref.subject}".parse().unwrap(),
        Some("serviceaccount:ci"),
        Some("acme")
    )
    .is_err());
    assert!(resolve_subject(
        &"group:${ref.name}".parse().unwrap(),
        Some("platform"),
        None
    )
    .is_err());
}

#[test]
fn wildcard_replacement_uses_authored_identity_and_preserves_deny_provenance() {
    let wildcard_allow = statement(json!({
        "effect": "Allow", "kinds": "*", "verbs": ["get"]
    }));
    let wildcard_deny = statement(json!({
        "effect": "Deny", "kinds": "*", "verbs": ["delete"]
    }));
    let specific_allow = statement(json!({
        "effect": "Allow", "kinds": ["rise.dev/Deployment"], "verbs": ["get"]
    }));
    let bindings = vec![
        ApplicableBinding {
            provenance: "wildcard",
            subject: "${ref.subject}".parse().unwrap(),
            scope: Scope::wildcard(),
            selector: Some(selector("rise.dev/owner", None)),
            tier: BindingTier::Platform,
            statements: vec![wildcard_allow, wildcard_deny],
        },
        ApplicableBinding {
            provenance: "specific",
            subject: "${ref.subject}".parse().unwrap(),
            scope: "rise.dev/Organization/acme".parse().unwrap(),
            selector: Some(selector("rise.dev/owner", Some("user:alice"))),
            tier: BindingTier::Organization("acme".to_owned()),
            statements: vec![specific_allow],
        },
    ];
    let result = apply_wildcard_replacement(&bindings);
    assert_eq!(result.len(), 2);
    assert!(result
        .iter()
        .all(|item| item.provenance != "wildcard" || item.statement.effect == Effect::Deny));
    let retained = result
        .iter()
        .find(|item| item.provenance == "wildcard")
        .unwrap();
    assert_eq!(retained.tier, BindingTier::Platform);
}

#[test]
fn literals_templates_and_different_selector_keys_do_not_collide() {
    let allow = statement(json!({"effect":"Allow", "kinds":"*", "verbs":"*"}));
    let wildcard = ApplicableBinding {
        provenance: 1,
        subject: "group:acme/platform".parse().unwrap(),
        scope: Scope::wildcard(),
        selector: Some(selector("rise.dev/owner", None)),
        tier: BindingTier::Platform,
        statements: vec![allow.clone()],
    };
    let template = ApplicableBinding {
        provenance: 2,
        subject: "group:${ref.name}".parse().unwrap(),
        scope: "rise.dev/Organization/acme".parse().unwrap(),
        selector: Some(selector("rise.dev/owner", None)),
        tier: BindingTier::Organization("acme".to_owned()),
        statements: vec![allow.clone()],
    };
    assert_eq!(
        apply_wildcard_replacement(&[wildcard.clone(), template]).len(),
        2
    );

    let different_key = ApplicableBinding {
        provenance: 3,
        subject: wildcard.subject.clone(),
        scope: "rise.dev/Organization/acme".parse().unwrap(),
        selector: Some(selector("rise.dev/squad", None)),
        tier: BindingTier::Organization("acme".to_owned()),
        statements: vec![allow],
    };
    assert_eq!(
        apply_wildcard_replacement(&[wildcard, different_key]).len(),
        2
    );
}

#[test]
fn deny_aware_subset_detects_removed_denies_as_new_authority() {
    let allow_all = statement(json!({"effect":"Allow", "kinds":"*", "verbs":"*"}));
    let deny_delete = statement(json!({
        "effect":"Deny", "kinds":["rise.dev/Environment"], "verbs":["delete"]
    }));
    let allow_delete = statement(json!({
        "effect":"Allow", "kinds":["rise.dev/Environment"], "verbs":["delete"]
    }));
    let before = vec![allow_all.clone(), deny_delete];
    let after = vec![allow_all];
    assert!(!newly_allowed_is_subset(&before, &after, &[]));
    assert!(newly_allowed_is_subset(&before, &after, &[allow_delete]));
}

#[test]
fn scoped_binding_deletion_checks_authority_exposed_by_removed_deny() {
    let allow_all = statement(json!({"effect":"Allow", "kinds":"*", "verbs":"*"}));
    let deny_delete = statement(json!({
        "effect":"Deny", "kinds":["rise.dev/Environment"], "verbs":["delete"]
    }));
    let allow_delete = statement(json!({
        "effect":"Allow", "kinds":["rise.dev/Environment"], "verbs":["delete"]
    }));
    let before = vec![allow_all.clone(), deny_delete];
    let after = vec![allow_all];
    let domain = PolicyDomain {
        scope: "rise.dev/Organization/acme".parse().unwrap(),
        selector: None,
    };

    assert!(!scoped_newly_allowed_is_subset(
        Some((&before, &domain)),
        (&after, &domain),
        &[],
        &domain,
    ));
    assert!(scoped_newly_allowed_is_subset(
        Some((&before, &domain)),
        (&after, &domain),
        std::slice::from_ref(&allow_delete),
        &domain,
    ));
}

#[test]
fn subset_is_exact_across_kind_and_subresource_partitions() {
    let exact = statement(json!({
        "effect":"Allow", "kinds":["rise.dev/Deployment"], "verbs":["get"]
    }));
    let group = statement(json!({
        "effect":"Allow", "kinds":["rise.dev/*"], "verbs":["get"]
    }));
    let main = statement(json!({
        "effect":"Allow", "kinds":"*", "verbs":"*"
    }));
    let subresources = statement(json!({
        "effect":"Allow", "kinds":"*", "verbs":"*", "subresources":"*"
    }));
    assert!(policy_is_subset(
        std::slice::from_ref(&exact),
        std::slice::from_ref(&group)
    ));
    assert!(!policy_is_subset(
        std::slice::from_ref(&group),
        &[statement(json!({
            "effect":"Allow", "kinds":["rise.dev/Deployment"], "verbs":"*"
        }))]
    ));
    assert!(!policy_is_subset(
        std::slice::from_ref(&subresources),
        std::slice::from_ref(&main)
    ));
    assert!(!policy_is_subset(&[main], &[subresources]));
}

#[test]
fn policy_subset_is_transitive_across_representative_matchers() {
    let policies = [
        vec![],
        vec![statement(json!({
            "effect":"Allow", "kinds":["rise.dev/Deployment"], "verbs":["get"]
        }))],
        vec![statement(json!({
            "effect":"Allow", "kinds":["rise.dev/*"], "verbs":["get"]
        }))],
        vec![statement(json!({
            "effect":"Allow", "kinds":"*", "verbs":["get"]
        }))],
        vec![statement(json!({
            "effect":"Allow", "kinds":"*", "verbs":"*"
        }))],
        vec![
            statement(json!({"effect":"Allow", "kinds":"*", "verbs":"*"})),
            statement(json!({
                "effect":"Deny", "kinds":["rise.dev/Environment"], "verbs":["delete"]
            })),
        ],
    ];

    for left in &policies {
        for middle in &policies {
            for right in &policies {
                if policy_is_subset(left, middle) && policy_is_subset(middle, right) {
                    assert!(policy_is_subset(left, right));
                }
            }
        }
    }
}

#[test]
fn domain_containment_uses_the_fail_closed_selector_lattice() {
    let scope: Scope = "rise.dev/Organization/acme".parse().unwrap();
    let no_selector = PolicyDomain {
        scope: scope.clone(),
        selector: None,
    };
    let exists = PolicyDomain {
        scope: scope.clone(),
        selector: Some(selector("rise.dev/owner", None)),
    };
    let equal_alice = PolicyDomain {
        scope: scope.clone(),
        selector: Some(selector("rise.dev/owner", Some("user:alice"))),
    };
    let equal_bob = PolicyDomain {
        scope: scope.clone(),
        selector: Some(selector("rise.dev/owner", Some("user:bob"))),
    };
    let other_key = PolicyDomain {
        scope,
        selector: Some(selector("rise.dev/squad", None)),
    };
    assert!(domain_covers(&no_selector, &exists));
    assert!(domain_covers(&exists, &equal_alice));
    assert!(!domain_covers(&equal_alice, &exists));
    assert!(!domain_covers(&equal_alice, &equal_bob));
    assert!(!domain_covers(&exists, &other_key));
}

#[test]
fn registry_resolved_scope_ancestry_can_cover_descendants() {
    let organization = PolicyDomain {
        scope: "rise.dev/Organization/acme".parse().unwrap(),
        selector: None,
    };
    let project = PolicyDomain {
        scope: "rise.dev/Project/acme/app".parse().unwrap(),
        selector: None,
    };
    let ancestry = |covering: &Scope, covered: &Scope| {
        covering == covered
            || (covering.to_string() == "rise.dev/Organization/acme"
                && covered.names().next() == Some("acme"))
    };
    assert!(domain_covers_with(&organization, &project, &ancestry));
    assert!(!domain_covers_with(&project, &organization, &ancestry));

    let allow = statement(json!({"effect":"Allow", "kinds":"*", "verbs":"*"}));
    assert!(scoped_newly_allowed_is_subset_with(
        None,
        (std::slice::from_ref(&allow), &project),
        std::slice::from_ref(&allow),
        &organization,
        &ancestry,
    ));
}

#[test]
fn narrow_domain_cannot_justify_a_broader_grant() {
    let allow = statement(json!({"effect":"Allow", "kinds":"*", "verbs":"*"}));
    let broad = PolicyDomain {
        scope: Scope::wildcard(),
        selector: None,
    };
    let narrow = PolicyDomain {
        scope: "rise.dev/Organization/acme".parse().unwrap(),
        selector: None,
    };
    assert!(!scoped_newly_allowed_is_subset(
        None,
        (std::slice::from_ref(&allow), &broad),
        std::slice::from_ref(&allow),
        &narrow
    ));
}

#[test]
fn unchanged_statements_do_not_hide_scope_or_selector_expansion() {
    let allow = statement(json!({"effect":"Allow", "kinds":"*", "verbs":"*"}));
    let broad = PolicyDomain {
        scope: Scope::wildcard(),
        selector: None,
    };
    let narrow_scope = PolicyDomain {
        scope: "rise.dev/Organization/acme".parse().unwrap(),
        selector: None,
    };
    let narrow_selector = PolicyDomain {
        scope: Scope::wildcard(),
        selector: Some(selector("rise.dev/owner", Some("user:alice"))),
    };
    let exists_selector = PolicyDomain {
        scope: Scope::wildcard(),
        selector: Some(selector("rise.dev/owner", None)),
    };

    for (before, after) in [
        (&narrow_scope, &broad),
        (&narrow_selector, &exists_selector),
    ] {
        assert!(!scoped_newly_allowed_is_subset(
            Some((std::slice::from_ref(&allow), before)),
            (std::slice::from_ref(&allow), after),
            &[],
            &broad,
        ));
        assert!(scoped_newly_allowed_is_subset(
            Some((std::slice::from_ref(&allow), before)),
            (std::slice::from_ref(&allow), after),
            std::slice::from_ref(&allow),
            &broad,
        ));
    }
}

#[test]
fn typed_subject_values_remain_canonical() {
    let subject: BindingSubject = "user:alice".parse().unwrap();
    assert_eq!(
        resolve_subject(&subject, Some("ignored"), Some("acme"))
            .unwrap()
            .to_string(),
        "user:alice"
    );
    let _: SubresourceName = "status".parse().unwrap();
}
