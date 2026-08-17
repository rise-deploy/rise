use jsonschema::validator_for;
use rise_resource_api::{
    BindingSubject, PlatformRoleBindingSpec, RoleBindingSpec, SubjectMembership,
    API_VERSION_V1ALPHA1, ORGANIZATION_KIND, PLATFORM_ROLE_BINDING_KIND, PLATFORM_ROLE_KIND,
    POLICY_KIND_DEFINITIONS, ROLE_BINDING_KIND, ROLE_KIND,
};
use schemars::{schema_for, JsonSchema};
use serde_json::{json, Value};

fn schema_accepts<T: JsonSchema>(value: Value) -> bool {
    let schema = serde_json::to_value(schema_for!(T)).unwrap();
    validator_for(&schema).unwrap().is_valid(&value)
}

fn platform_binding(subject: &str) -> Value {
    json!({
        "subject": subject,
        "roleRef": { "kind": "PlatformRole", "name": "viewer" }
    })
}

#[test]
fn role_statement_contract_is_closed_and_non_empty() {
    let valid = json!({
        "statements": [{
            "effect": "Allow",
            "kinds": ["rise.dev/Deployment", "widgets.example/Widget", "rise.dev/*"],
            "verbs": ["get", "use"],
            "subresources": ["status"]
        }]
    });
    assert!(serde_json::from_value::<rise_resource_api::RoleSpec>(valid.clone()).is_ok());
    assert!(schema_accepts::<rise_resource_api::RoleSpec>(valid));

    for invalid in [
        json!({"statements":[{"effect":"allow","kinds":"*","verbs":"*"}]}),
        json!({"statements":[{"effect":"Allow","kinds":[],"verbs":"*"}]}),
        json!({"statements":[{"effect":"Allow","kinds":["Deployment"],"verbs":"*"}]}),
        json!({"statements":[{"effect":"Allow","kinds":"*","verbs":[]}]}),
        json!({"statements":[{"effect":"Allow","kinds":"*","verbs":["read"]}]}),
        json!({"statements":[{"effect":"Allow","kinds":"*","verbs":"*","subresources":[]}]}),
        json!({"statements":[{"effect":"Allow","kinds":"*","verbs":"*","subresources":null}]}),
        json!({"statements":[{"effect":"Allow","kinds":"*","verbs":"*","extra":true}]}),
        json!({"statements":[],"extra":true}),
    ] {
        assert!(
            serde_json::from_value::<rise_resource_api::RoleSpec>(invalid.clone()).is_err(),
            "serde accepted {invalid}"
        );
        assert!(
            !schema_accepts::<rise_resource_api::RoleSpec>(invalid.clone()),
            "schema accepted {invalid}"
        );
    }
}

#[test]
fn matcher_sets_reject_duplicates_and_serialize_canonically() {
    let duplicate = json!({
        "statements": [{
            "effect": "Allow",
            "kinds": ["rise.dev/Deployment", "rise.dev/Deployment"],
            "verbs": "*"
        }]
    });
    assert!(serde_json::from_value::<rise_resource_api::RoleSpec>(duplicate).is_err());

    let role: rise_resource_api::RoleSpec = serde_json::from_value(json!({
        "statements": [{
            "effect": "Deny",
            "kinds": ["widgets.example/Widget", "rise.dev/Deployment"],
            "verbs": ["use", "get"]
        }]
    }))
    .unwrap();
    assert_eq!(
        serde_json::to_value(role).unwrap(),
        json!({
            "statements": [{
                "effect": "Deny",
                "kinds": ["rise.dev/Deployment", "widgets.example/Widget"],
                "verbs": ["get", "use"]
            }]
        })
    );
}

#[test]
fn platform_membership_omission_normalizes_but_null_fails() {
    let parsed: PlatformRoleBindingSpec =
        serde_json::from_value(platform_binding("user:alice")).unwrap();
    assert_eq!(parsed.subject_membership, SubjectMembership::Any);
    assert_eq!(
        serde_json::to_value(parsed).unwrap()["subjectMembership"],
        json!("Any")
    );

    let mut null_membership = platform_binding("user:alice");
    null_membership["subjectMembership"] = Value::Null;
    assert!(serde_json::from_value::<PlatformRoleBindingSpec>(null_membership.clone()).is_err());
    assert!(!schema_accepts::<PlatformRoleBindingSpec>(null_membership));

    let mut invalid_membership = platform_binding("user:alice");
    invalid_membership["subjectMembership"] = json!("Organization");
    assert!(serde_json::from_value::<PlatformRoleBindingSpec>(invalid_membership).is_err());
}

#[test]
fn binding_shapes_reject_plural_wrong_case_and_wrong_reference_direction() {
    for invalid in [
        json!({
            "subjects": ["user:alice"],
            "roleRef": { "kind": "PlatformRole", "name": "viewer" }
        }),
        json!({
            "Subject": "user:alice",
            "roleRef": { "kind": "PlatformRole", "name": "viewer" }
        }),
        json!({
            "subject": "user:alice",
            "subjectMembership": "Any",
            "roleRef": { "kind": "Role", "name": "viewer" }
        }),
        json!({
            "subject": "user:alice",
            "scope": null,
            "roleRef": { "kind": "PlatformRole", "name": "viewer" }
        }),
        json!({
            "subject": "user:alice",
            "labelSelector": { "key": "rise.dev/owner", "value": null },
            "roleRef": { "kind": "PlatformRole", "name": "viewer" }
        }),
    ] {
        assert!(serde_json::from_value::<RoleBindingSpec>(invalid.clone()).is_err());
    }

    let wrong_platform_ref = json!({
        "subject": "user:alice",
        "roleRef": { "kind": "Role", "name": "viewer" }
    });
    assert!(serde_json::from_value::<PlatformRoleBindingSpec>(wrong_platform_ref).is_err());
}

#[test]
fn binding_subject_grammar_is_closed() {
    for value in [
        "user:alice",
        "group:acme/platform",
        "user:${ref.name}",
        "group:${ref.name}",
        "${ref.subject}",
    ] {
        let parsed: BindingSubject = serde_json::from_value(json!(value)).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), json!(value));
    }
    for value in [
        "serviceaccount:${ref.name}",
        "controller:${ref.name}",
        "group:${label.value}",
        "${ref.name}",
    ] {
        assert!(serde_json::from_value::<BindingSubject>(json!(value)).is_err());
    }
}

#[test]
fn binding_normalization_is_contextual_and_fail_closed() {
    let org: RoleBindingSpec = serde_json::from_value(json!({
        "subject": "user:alice",
        "roleRef": { "kind": "Role", "name": "viewer" }
    }))
    .unwrap();
    assert_eq!(
        org.normalize("acme").unwrap().scope().to_string(),
        "rise.dev/Organization/acme"
    );

    let group: PlatformRoleBindingSpec =
        serde_json::from_value(platform_binding("group:acme/platform")).unwrap();
    assert_eq!(
        group.normalize().unwrap().scope().to_string(),
        "rise.dev/Organization/acme"
    );

    let user: PlatformRoleBindingSpec =
        serde_json::from_value(platform_binding("user:alice")).unwrap();
    assert!(user.normalize().unwrap().scope().is_wildcard());

    let explicit_wildcard: PlatformRoleBindingSpec = serde_json::from_value(json!({
        "subject": "serviceaccount:acme/ci",
        "scope": "*",
        "roleRef": { "kind": "PlatformRole", "name": "viewer" }
    }))
    .unwrap();
    assert!(explicit_wildcard.normalize().is_err());

    let dynamic_without_selector: PlatformRoleBindingSpec =
        serde_json::from_value(platform_binding("${ref.subject}")).unwrap();
    assert!(dynamic_without_selector.normalize().is_err());

    let static_exists_selector: PlatformRoleBindingSpec = serde_json::from_value(json!({
        "subject": "user:alice",
        "labelSelector": { "key": "rise.dev/owner" },
        "roleRef": { "kind": "PlatformRole", "name": "viewer" }
    }))
    .unwrap();
    assert!(static_exists_selector.normalize().is_err());

    let reserved_platform: PlatformRoleBindingSpec =
        serde_json::from_value(platform_binding("system:operators")).unwrap();
    assert!(reserved_platform.normalize().is_err());

    let reserved_org: RoleBindingSpec = serde_json::from_value(json!({
        "subject": "system:operators",
        "roleRef": { "kind": "PlatformRole", "name": "system-admin" }
    }))
    .unwrap();
    assert!(reserved_org.normalize("acme").is_err());
}

#[test]
fn generated_binding_schemas_match_omission_and_null_rules() {
    let omitted = platform_binding("user:alice");
    assert!(schema_accepts::<PlatformRoleBindingSpec>(omitted));

    let org = json!({
        "subject": "user:alice",
        "roleRef": { "kind": "Role", "name": "viewer" }
    });
    assert!(schema_accepts::<RoleBindingSpec>(org));

    let null_scope = json!({
        "subject": "user:alice",
        "scope": null,
        "roleRef": { "kind": "Role", "name": "viewer" }
    });
    assert!(!schema_accepts::<RoleBindingSpec>(null_scope));
}

#[test]
fn policy_collections_are_reserved_against_external_definitions() {
    for collection in [
        rise_resource_api::ROLE_COLLECTION,
        rise_resource_api::ROLE_BINDING_COLLECTION,
        rise_resource_api::PLATFORM_ROLE_COLLECTION,
        rise_resource_api::PLATFORM_ROLE_BINDING_COLLECTION,
    ] {
        assert!(rise_resource_api::is_reserved_collection_name(collection));
    }
}

#[test]
fn policy_kind_definitions_capture_fixed_adr_placement() {
    // Two same-shaped pairs, one per placement level: the org pair hangs under
    // an Organization, the platform pair at the root (ADR-0001 §3).
    let definitions: Vec<_> = POLICY_KIND_DEFINITIONS
        .iter()
        .map(|definition| {
            (
                definition.kind,
                definition.collection,
                definition.parent.map(|parent| parent.kind),
            )
        })
        .collect();
    assert_eq!(
        definitions,
        vec![
            (ROLE_KIND, "roles", Some(ORGANIZATION_KIND)),
            (ROLE_BINDING_KIND, "rolebindings", Some(ORGANIZATION_KIND)),
            (PLATFORM_ROLE_KIND, "platformroles", None),
            (PLATFORM_ROLE_BINDING_KIND, "platformrolebindings", None),
        ]
    );
    for definition in POLICY_KIND_DEFINITIONS {
        assert_eq!(definition.api_version, API_VERSION_V1ALPHA1);
        if let Some(parent) = definition.parent {
            assert_eq!(parent.api_version, API_VERSION_V1ALPHA1);
        }
    }
}
