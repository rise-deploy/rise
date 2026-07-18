use std::collections::BTreeSet;

use rise_resource_api::{ResourceKind, Scope, SubjectId, SubjectRef};
use schemars::schema_for;

fn schema_accepts<T: schemars::JsonSchema>(value: &str) -> bool {
    let schema = serde_json::to_value(schema_for!(T)).unwrap();
    jsonschema::validator_for(&schema)
        .unwrap()
        .is_valid(&serde_json::json!(value))
}

#[test]
fn subject_id_accepts_exactly_the_seven_canonical_forms() {
    let valid = [
        "user:u-01jz",
        "controller:deployment-controller",
        "group:acme-corp/platform",
        "serviceaccount:acme-corp/ci-bot",
        "org:acme-corp",
        "system:authenticated",
        "system:operators",
    ];
    for value in valid {
        let parsed: SubjectId = value.parse().unwrap_or_else(|e| panic!("{value}: {e}"));
        assert_eq!(parsed.as_ref(), value);
        assert_eq!(parsed.to_string(), value);
        assert_eq!(
            serde_json::to_string(&parsed).unwrap(),
            format!("\"{value}\"")
        );
        assert_eq!(
            serde_json::from_str::<SubjectId>(&format!("\"{value}\"")).unwrap(),
            parsed
        );
    }

    for value in [
        "",
        "User:alice",
        "user:",
        "user:me/extra",
        "user:me:extra",
        "group:acme",
        "group:/platform",
        "group:acme/",
        "group:acme/a/b",
        "serviceaccount:acme",
        "acme/group:platform",
        "org:../acme",
        "system:operator",
        "system:authenticated/extra",
        "unknown:name",
        "group:acme/platform?x=1",
        "user:alice#fragment",
        "user:alice%2fadmin",
    ] {
        assert!(
            value.parse::<SubjectId>().is_err(),
            "unexpectedly accepted {value}"
        );
    }
    assert!(serde_json::from_str::<SubjectId>("42").is_err());
}

#[test]
fn subject_id_classifies_virtual_and_org_native_forms_exhaustively() {
    let cases = [
        ("user:u-01jz", false, false),
        ("controller:deployment-controller", false, false),
        ("group:acme-corp/platform", false, true),
        ("serviceaccount:acme-corp/ci-bot", false, true),
        ("org:acme-corp", true, false),
        ("system:authenticated", true, false),
        ("system:operators", true, false),
    ];
    for (value, is_virtual, is_org_native) in cases {
        let subject: SubjectId = value.parse().unwrap();
        assert_eq!(subject.is_virtual(), is_virtual, "{value}");
        assert_eq!(subject.is_org_native(), is_org_native, "{value}");
    }
}

#[test]
fn subject_ref_is_narrow_and_group_resolution_requires_an_org() {
    for value in ["user:u-01jz", "group:platform"] {
        let parsed: SubjectRef = value.parse().unwrap();
        assert_eq!(parsed.to_string(), value);
        let encoded = serde_json::to_string(&parsed).unwrap();
        assert_eq!(
            serde_json::from_str::<SubjectRef>(&encoded).unwrap(),
            parsed
        );
    }
    for value in [
        "user:me", // aliases are not persisted; `me` happens to be syntactically canonical but is reserved below
        "group:acme/platform",
        "serviceaccount:ci",
        "controller:worker",
        "org:acme",
        "system:authenticated",
        "group:",
        "group:../platform",
        "user:u-01jz/extra",
        "${ref.subject}",
    ] {
        if value == "user:me" {
            assert!(
                value.parse::<SubjectRef>().is_err(),
                "user:me must be rejected"
            );
        } else {
            assert!(
                value.parse::<SubjectRef>().is_err(),
                "unexpectedly accepted {value}"
            );
        }
    }

    let user: SubjectRef = "user:u-01jz".parse().unwrap();
    assert_eq!(user.resolve(None).unwrap().as_ref(), "user:u-01jz");
    let group: SubjectRef = "group:platform".parse().unwrap();
    assert!(group.resolve(None).is_err());
    assert_eq!(
        group.resolve(Some("acme-corp")).unwrap().as_ref(),
        "group:acme-corp/platform"
    );
    assert!(group.resolve(Some("Acme")).is_err());
    assert!(serde_json::from_str::<SubjectRef>("{}").is_err());
}

#[test]
fn resource_kind_is_exact_qualified_and_route_versions_normalize() {
    for value in [
        "rise.dev/Deployment",
        "widgets.example.com/Widget",
        "a.io/X1",
    ] {
        let parsed: ResourceKind = value.parse().unwrap();
        assert_eq!(parsed.as_ref(), value);
        assert_eq!(
            serde_json::from_str::<ResourceKind>(&serde_json::to_string(&parsed).unwrap()).unwrap(),
            parsed
        );
    }
    for value in [
        "Widget",
        "*/Widget",
        "rise.dev/*",
        "rise.dev/",
        "/Widget",
        "rise.dev/v1/Widget",
        "Rise.dev/Widget",
        "rise.dev/widget",
        "rise.dev/Widget?x=1",
        "rise.dev/Widget#x",
        "rise.dev/%57idget",
    ] {
        assert!(
            value.parse::<ResourceKind>().is_err(),
            "unexpectedly accepted {value}"
        );
    }

    let alpha = ResourceKind::from_api_route("rise.dev/v1alpha1", "Deployment").unwrap();
    let stable = ResourceKind::from_api_route("rise.dev/v1", "Deployment").unwrap();
    assert_eq!(alpha, stable);
    assert_ne!(
        "alpha.example/Widget".parse::<ResourceKind>().unwrap(),
        "beta.example/Widget".parse::<ResourceKind>().unwrap()
    );
    for version in ["rise.dev", "rise.dev/", "rise.dev/v1/extra", "rise.dev/V1"] {
        assert!(ResourceKind::from_api_route(version, "Widget").is_err());
    }
    assert!(serde_json::from_str::<ResourceKind>("[]").is_err());
}

#[test]
fn scope_accepts_only_wildcard_or_qualified_kind_with_canonical_names() {
    for value in [
        "*",
        "rise.dev/Organization/acme-corp",
        "rise.dev/Project/acme-corp/app",
        "widgets.example.com/Widget/acme/widget-a",
    ] {
        let parsed: Scope = value.parse().unwrap_or_else(|e| panic!("{value}: {e}"));
        assert_eq!(parsed.as_ref(), value);
        assert_eq!(
            serde_json::from_str::<Scope>(&serde_json::to_string(&parsed).unwrap()).unwrap(),
            parsed
        );
        if value == "*" {
            assert!(parsed.resource_kind().is_none());
            assert_eq!(parsed.names().len(), 0);
        } else {
            assert!(parsed.resource_kind().is_some());
            assert_eq!(parsed.names().len(), value.split('/').count() - 2);
            assert_eq!(
                parsed.names().collect::<Vec<_>>(),
                value.split('/').skip(2).collect::<Vec<_>>()
            );
        }
    }
    for value in [
        "",
        "**",
        "Widget/acme",
        "rise.dev/Project",
        "rise.dev/*/acme",
        "rise.dev/Project/",
        "rise.dev/Project/acme/",
        "rise.dev/Project/acme//app",
        "rise.dev/Project/./app",
        "rise.dev/Project/../app",
        "rise.dev/Project/acme?x=1",
        "rise.dev/Project/acme#x",
        "rise.dev/Project/acme%2fapp",
        "/rise.dev/Project/acme",
    ] {
        assert!(
            value.parse::<Scope>().is_err(),
            "unexpectedly accepted {value}"
        );
    }
    assert!(serde_json::from_str::<Scope>("null").is_err());
}

#[test]
fn canonical_types_have_deterministic_value_order() {
    let set = BTreeSet::from([
        "beta.example/Widget".parse::<ResourceKind>().unwrap(),
        "alpha.example/Widget".parse().unwrap(),
    ]);
    assert_eq!(
        set.into_iter().next().unwrap().as_ref(),
        "alpha.example/Widget"
    );
}

#[test]
fn canonical_json_schemas_enforce_the_closed_string_grammars() {
    for value in [
        "user:u-01jz",
        "controller:worker",
        "group:acme/platform",
        "serviceaccount:acme/ci",
        "org:acme",
        "system:authenticated",
        "system:operators",
    ] {
        assert!(schema_accepts::<SubjectId>(value), "{value}");
    }
    for value in ["User:alice", "group:acme", "system:operator", "user:a/b"] {
        assert!(!schema_accepts::<SubjectId>(value), "{value}");
    }

    for value in ["user:u-01jz", "group:platform"] {
        assert!(schema_accepts::<SubjectRef>(value), "{value}");
    }
    for value in ["user:me", "group:acme/platform", "controller:worker"] {
        assert!(!schema_accepts::<SubjectRef>(value), "{value}");
    }

    for value in ["rise.dev/Deployment", "widgets.example.com/Widget"] {
        assert!(schema_accepts::<ResourceKind>(value), "{value}");
    }
    for value in [
        "Deployment",
        "rise.dev/*",
        "rise.dev/widget",
        "Rise.dev/Widget",
    ] {
        assert!(!schema_accepts::<ResourceKind>(value), "{value}");
    }
    let overlong_group = format!(
        "{}.{}.{}.{}.a",
        "a".repeat(63),
        "b".repeat(63),
        "c".repeat(63),
        "d".repeat(63)
    );
    let overlong_kind = format!("{overlong_group}/Widget");
    assert!(overlong_kind.parse::<ResourceKind>().is_err());
    assert!(!schema_accepts::<ResourceKind>(&overlong_kind));

    for value in ["*", "rise.dev/Project/acme/app"] {
        assert!(schema_accepts::<Scope>(value), "{value}");
    }
    for value in ["rise.dev/Project", "rise.dev/Project/acme/", "Widget/acme"] {
        assert!(!schema_accepts::<Scope>(value), "{value}");
    }
    let overlong_scope = format!("{overlong_group}/Widget/acme");
    assert!(overlong_scope.parse::<Scope>().is_err());
    assert!(!schema_accepts::<Scope>(&overlong_scope));

    // Schemas and parsers establish syntax only. Registry existence and the
    // expected parent-chain shape remain transaction-scoped write-time checks.
    let syntactic_only = "unknown.example/Widget/acme/leaf";
    assert!(schema_accepts::<Scope>(syntactic_only));
    assert!(syntactic_only.parse::<Scope>().is_ok());
}
