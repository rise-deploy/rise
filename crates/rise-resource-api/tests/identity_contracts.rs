use jsonschema::validator_for;
use rise_resource_api::{
    ControllerSpec, ControllerTrustPolicySpec, ExternalSubject, GroupMembershipSpec, GroupSpec,
    Issuer, ServiceAccountSpec, ServiceAccountTrustPolicySpec, UserIdentitySpec, UserSpec,
    API_GROUP, API_VERSION_V1ALPHA1, CONTROLLER_KIND, CONTROLLER_TRUST_POLICY_KIND, GROUP_KIND,
    GROUP_MEMBERSHIP_KIND, IDENTITY_KIND_DEFINITIONS, MAX_EXTERNAL_SUBJECT_CHARS, MAX_ISSUER_BYTES,
    ORGANIZATION_KIND, SERVICE_ACCOUNT_KIND, SERVICE_ACCOUNT_TRUST_POLICY_KIND, USER_IDENTITY_KIND,
    USER_KIND,
};
use schemars::{schema_for, JsonSchema};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

fn schema_accepts<T: JsonSchema>(value: Value) -> bool {
    let schema = serde_json::to_value(schema_for!(T)).unwrap();
    validator_for(&schema).unwrap().is_valid(&value)
}

fn assert_rejected<T>(value: Value)
where
    T: DeserializeOwned + JsonSchema,
{
    assert!(
        serde_json::from_value::<T>(value.clone()).is_err(),
        "serde accepted {value}"
    );
    assert!(
        !schema_accepts::<T>(value.clone()),
        "schema accepted {value}"
    );
}

#[test]
fn user_and_user_identity_active_default_true_but_null_fails() {
    let user: UserSpec = serde_json::from_value(json!({})).unwrap();
    assert!(user.active);
    assert_eq!(serde_json::to_value(user).unwrap()["active"], json!(true));
    assert!(schema_accepts::<UserSpec>(json!({})));

    let identity: UserIdentitySpec = serde_json::from_value(json!({
        "issuer": "https://issuer.example",
        "subject": "opaque-subject"
    }))
    .unwrap();
    assert!(identity.active);
    assert_eq!(
        serde_json::to_value(identity).unwrap()["active"],
        json!(true)
    );
    assert!(schema_accepts::<UserIdentitySpec>(json!({
        "issuer": "https://issuer.example",
        "subject": "opaque-subject"
    })));

    for invalid in [json!({"active": null}), json!({"active": "true"})] {
        assert_rejected::<UserSpec>(invalid);
    }
    for invalid in [
        json!({"issuer":"https://issuer.example","subject":"subject","active":null}),
        json!({"issuer":"https://issuer.example","subject":"subject","active":"true"}),
    ] {
        assert_rejected::<UserIdentitySpec>(invalid);
    }
}

#[test]
fn user_profile_is_optional_non_authoritative_and_closed() {
    let user: UserSpec = serde_json::from_value(json!({
        "displayName": "Alice Example",
        "primaryEmail": "shared@example.com"
    }))
    .unwrap();
    assert_eq!(user.display_name.as_deref(), Some("Alice Example"));
    assert_eq!(user.primary_email.as_deref(), Some("shared@example.com"));

    for invalid in [
        json!({"displayName": null}),
        json!({"primaryEmail": null}),
        json!({"email": "authoritative@example.com"}),
        json!({"unknown": true}),
    ] {
        assert_rejected::<UserSpec>(invalid);
    }
}

#[test]
fn external_identity_fields_are_required_and_canonical() {
    let identity: UserIdentitySpec = serde_json::from_value(json!({
        "issuer": "https://issuer.example",
        "subject": " subject with spaces "
    }))
    .unwrap();
    assert_eq!(identity.issuer.as_str(), "https://issuer.example");
    assert_eq!(identity.subject.as_str(), " subject with spaces ");
    assert_eq!(
        serde_json::to_value(identity).unwrap()["issuer"],
        json!("https://issuer.example")
    );

    for invalid in [
        json!({"subject":"subject"}),
        json!({"issuer":"https://issuer.example"}),
        json!({"issuer":"","subject":"subject"}),
        json!({"issuer":"   ","subject":"subject"}),
        json!({"issuer":"https://issuer.example","subject":""}),
        json!({"issuer":"https://issuer.example","subject":"\t"}),
        json!({"issuer":"https://issuer.example","subject":"subject","extra":true}),
    ] {
        assert_rejected::<UserIdentitySpec>(invalid);
    }

    assert!(serde_json::from_value::<UserIdentitySpec>(json!({
        "issuer": "https://issuer.example/",
        "subject": "subject"
    }))
    .is_err());
}

#[test]
fn issuers_are_canonical_urls() {
    for canonical in [
        "https://issuer.example",
        "http://rise-dex:5556/dex",
        "https://127.0.0.1:8443",
    ] {
        let issuer = Issuer::new(canonical).unwrap();
        assert_eq!(issuer.as_str(), canonical);
        assert_eq!(serde_json::to_value(&issuer).unwrap(), json!(canonical));
        assert!(schema_accepts::<Issuer>(json!(canonical)));
    }

    // These parse to one of the canonical URLs above, but authentication
    // identifiers are exact strings and must not be silently repaired.
    for (noncanonical, canonical) in [
        (
            "https://ISSUER.example:443/a/../tenant///",
            "https://issuer.example/tenant",
        ),
        ("http://rise-dex:5556/dex/", "http://rise-dex:5556/dex"),
        ("https://127.0.0.1:8443/", "https://127.0.0.1:8443"),
    ] {
        let error = Issuer::new(noncanonical).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("issuer URL must be in its canonical form \"{canonical}\"")
        );
    }

    for invalid in [
        "",
        " issuer ",
        "not-a-url",
        "ftp://issuer.example",
        "https:///missing-host",
        "https://user@issuer.example",
        "https://issuer.example?tenant=one",
        "https://issuer.example#tenant",
        "https://issuer.exa\nmple",
        "https://issuer.exa\tmple",
        "https://issuer.exa mple",
        "https://issuer.example\\tenant",
        "https://issuer.example/%GG",
        "https://issuer.example/%",
        "https://issuer.example/ümlaut",
        "https://@issuer.example",
        "https://issuer.example/a|b",
        "https://issuer.example:65536",
        "https://%2F.example",
    ] {
        assert!(Issuer::new(invalid).is_err(), "accepted {invalid}");
        assert!(
            !schema_accepts::<Issuer>(json!(invalid)),
            "schema accepted {invalid}"
        );
    }

    let overlong = format!("https://issuer.example/{}", "a".repeat(MAX_ISSUER_BYTES));
    assert!(Issuer::new(&overlong).is_err());
    assert!(!schema_accepts::<Issuer>(json!(overlong)));

    let overlong_trailing_slashes =
        format!("https://issuer.example{}", "/".repeat(MAX_ISSUER_BYTES));
    assert!(Issuer::new(&overlong_trailing_slashes).is_err());
    assert!(!schema_accepts::<Issuer>(json!(overlong_trailing_slashes)));
}

#[test]
fn external_subject_is_opaque_nonblank_and_bounded() {
    let exact = "x".repeat(MAX_EXTERNAL_SUBJECT_CHARS);
    let subject = ExternalSubject::new(exact.clone()).unwrap();
    assert_eq!(subject.as_str(), exact);
    assert!(schema_accepts::<ExternalSubject>(json!(exact)));

    let spaced = ExternalSubject::new(" subject ").unwrap();
    assert_eq!(spaced.as_str(), " subject ");

    for invalid in [
        String::new(),
        " \t ".to_string(),
        "subject\0suffix".to_string(),
        "x".repeat(MAX_EXTERNAL_SUBJECT_CHARS + 1),
    ] {
        assert!(ExternalSubject::new(invalid.clone()).is_err());
        assert!(!schema_accepts::<ExternalSubject>(json!(invalid)));
    }
}

#[test]
fn membership_is_an_empty_name_bound_marker() {
    let membership: GroupMembershipSpec = serde_json::from_value(json!({})).unwrap();
    assert_eq!(membership, GroupMembershipSpec::default());
    assert!(schema_accepts::<GroupMembershipSpec>(json!({})));

    for invalid in [json!({"userRef": null}), json!({"role": "admin"})] {
        assert_rejected::<GroupMembershipSpec>(invalid);
    }
}

fn valid_trust_policy() -> Value {
    json!({
        "issuer": "https://issuer.example",
        "claims": {
            "aud": "rise-*",
            "sub": "project-*"
        }
    })
}

#[test]
fn workload_trust_policies_use_closed_string_claim_constraints() {
    let controller: ControllerTrustPolicySpec =
        serde_json::from_value(valid_trust_policy()).unwrap();
    assert_eq!(controller.claims.get("aud"), Some("rise-*"));
    assert_eq!(controller.claims.get("sub"), Some("project-*"));

    let service_account: ServiceAccountTrustPolicySpec =
        serde_json::from_value(valid_trust_policy()).unwrap();
    assert_eq!(service_account.claims.get("aud"), Some("rise-*"));

    assert!(schema_accepts::<ControllerTrustPolicySpec>(
        valid_trust_policy()
    ));
    assert!(schema_accepts::<ServiceAccountTrustPolicySpec>(
        valid_trust_policy()
    ));

    let audience_only = json!({
        "issuer": "https://issuer.example",
        "claims": {"aud": "rise"}
    });
    assert!(serde_json::from_value::<ControllerTrustPolicySpec>(audience_only.clone()).is_ok());
    assert!(serde_json::from_value::<ServiceAccountTrustPolicySpec>(audience_only.clone()).is_ok());
    assert!(schema_accepts::<ControllerTrustPolicySpec>(
        audience_only.clone()
    ));
    assert!(schema_accepts::<ServiceAccountTrustPolicySpec>(
        audience_only
    ));

    for invalid in [
        json!({"issuer":"https://issuer.example","claims":{}}),
        json!({"issuer":"https://issuer.example","claims":{"sub":"subject"}}),
        json!({"issuer":"https://issuer.example","claims":{"aud":""}}),
        json!({"issuer":"https://issuer.example","claims":{"aud":"   "}}),
        json!({"issuer":"https://issuer.example","claims":{"aud":"rise","":"value"}}),
        json!({"issuer":"https://issuer.example","claims":{"aud":"rise","sub":""}}),
        json!({"issuer":"https://issuer.example","claims":{"aud":42}}),
        json!({"issuer":"https://issuer.example","claims":null}),
        json!({"issuer":"","claims":{"aud":"rise"}}),
        json!({"issuer":"   ","claims":{"aud":"rise"}}),
        json!({"issuer":"https://issuer.example","claims":{"aud":"rise"},"secret":"x"}),
    ] {
        assert_rejected::<ControllerTrustPolicySpec>(invalid.clone());
        assert_rejected::<ServiceAccountTrustPolicySpec>(invalid);
    }
}

#[test]
fn marker_specs_are_closed_empty_objects() {
    assert!(serde_json::from_value::<ControllerSpec>(json!({})).is_ok());
    assert!(serde_json::from_value::<GroupSpec>(json!({})).is_ok());
    assert!(serde_json::from_value::<ServiceAccountSpec>(json!({})).is_ok());
    assert_rejected::<ControllerSpec>(json!({"active": true}));
    assert_rejected::<GroupSpec>(json!({"displayName": "platform"}));
    assert_rejected::<ServiceAccountSpec>(json!({"issuer": "https://issuer.example"}));
}

#[test]
fn identity_kind_definitions_capture_fixed_adr_placement() {
    let definitions: Vec<_> = IDENTITY_KIND_DEFINITIONS
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
            (USER_KIND, "users", None),
            (USER_IDENTITY_KIND, "useridentities", Some(USER_KIND)),
            (CONTROLLER_KIND, "controllers", None),
            (
                CONTROLLER_TRUST_POLICY_KIND,
                "controllertrustpolicies",
                Some(CONTROLLER_KIND)
            ),
            (GROUP_KIND, "groups", Some(ORGANIZATION_KIND)),
            (GROUP_MEMBERSHIP_KIND, "groupmemberships", Some(GROUP_KIND)),
            (
                SERVICE_ACCOUNT_KIND,
                "serviceaccounts",
                Some(ORGANIZATION_KIND)
            ),
            (
                SERVICE_ACCOUNT_TRUST_POLICY_KIND,
                "serviceaccounttrustpolicies",
                Some(SERVICE_ACCOUNT_KIND)
            ),
        ]
    );
    assert!(IDENTITY_KIND_DEFINITIONS
        .iter()
        .all(|definition| definition.api_version == API_VERSION_V1ALPHA1));
}

#[test]
fn identity_collections_are_reserved_before_runtime_registration() {
    for definition in IDENTITY_KIND_DEFINITIONS {
        assert!(rise_resource_api::is_reserved_collection_name(
            definition.collection
        ));
    }

    for kind in [
        USER_KIND,
        USER_IDENTITY_KIND,
        "Role",
        "PlatformRoleBinding",
        "FutureBuiltIn",
    ] {
        assert!(rise_resource_api::is_reserved_resource_kind(
            API_GROUP, kind
        ));
        assert!(!rise_resource_api::is_reserved_resource_kind(
            "other.example",
            kind
        ));
    }
}
