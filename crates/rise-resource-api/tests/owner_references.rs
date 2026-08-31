use jsonschema::validator_for;
use rise_resource_api::{CreateResourceRequest, OwnerReference, Resource, UpdateResourceRequest};
use schemars::{schema_for, JsonSchema};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use uuid::Uuid;

fn assert_rejected<T>(value: Value)
where
    T: JsonSchema + DeserializeOwned,
{
    assert!(serde_json::from_value::<T>(value.clone()).is_err());
    let schema = serde_json::to_value(schema_for!(T)).unwrap();
    assert!(!validator_for(&schema).unwrap().is_valid(&value));
}

#[test]
fn owner_reference_is_closed_typed_and_uid_bound() {
    let uid = Uuid::new_v4();
    let label = "a".repeat(63);
    let overlong_api_group = format!("{label}.{label}.{label}.{label}/v1");
    let reference: OwnerReference = serde_json::from_value(json!({
        "apiVersion": "rise.dev/v1alpha1",
        "kind": "User",
        "name": "u-alice",
        "uid": uid
    }))
    .unwrap();
    assert_eq!(reference.api_version(), "rise.dev/v1alpha1");
    assert_eq!(reference.kind(), "User");
    assert_eq!(reference.name(), "u-alice");
    assert_eq!(reference.uid(), uid);
    assert!(!reference.block_owner_deletion());
    assert_eq!(reference.resource_kind().unwrap().as_ref(), "rise.dev/User");

    let blocking: OwnerReference = serde_json::from_value(json!({
        "apiVersion": "rise.dev/v1alpha1",
        "kind": "User",
        "name": "u-alice",
        "uid": uid,
        "blockOwnerDeletion": true
    }))
    .unwrap();
    assert!(blocking.block_owner_deletion());

    for invalid in [
        json!({"kind":"User","name":"u-alice","uid":uid}),
        json!({"apiVersion":"rise.dev","kind":"User","name":"u-alice","uid":uid}),
        json!({"apiVersion":"rise.dev/v1alpha1","kind":"user","name":"u-alice","uid":uid}),
        json!({"apiVersion":"rise.dev/v1alpha1","kind":"User","name":"Alice","uid":uid}),
        json!({"apiVersion":"rise..dev/v1alpha1","kind":"User","name":"u-alice","uid":uid}),
        json!({"apiVersion":"rise.dev/v1alpha1","kind":"User","name":"bad..name","uid":uid}),
        json!({"apiVersion":overlong_api_group,"kind":"User","name":"u-alice","uid":uid}),
        json!({"apiVersion":"rise.dev/v1alpha1","kind":"User","name":"u-alice","uid":"00000000-0000-0000-0000-000000000000"}),
        json!({"apiVersion":"rise.dev/v1alpha1","kind":"User","name":"u-alice","uid":uid,"controller":true}),
    ] {
        assert_rejected::<OwnerReference>(invalid);
    }

    let resource_definition = OwnerReference::new(
        "rise.dev/v1alpha1",
        "ResourceDefinition",
        "widgets.example.dev",
        Uuid::new_v4(),
    )
    .unwrap();
    assert_eq!(resource_definition.name(), "widgets.example.dev");
}

#[test]
fn owner_references_are_optional_metadata_on_every_write_shape() {
    let uid = Uuid::new_v4();
    let reference = json!({
        "apiVersion": "rise.dev/v1alpha1",
        "kind": "User",
        "name": "u-alice",
        "uid": uid
    });

    let create: CreateResourceRequest = serde_json::from_value(json!({
        "apiVersion": "example.dev/v1",
        "kind": "Widget",
        "metadata": {"name":"widget-a","ownerReferences":[reference.clone()]}
    }))
    .unwrap();
    assert_eq!(create.metadata.owner_references[0].uid(), uid);

    let update: UpdateResourceRequest = serde_json::from_value(json!({
        "apiVersion": "example.dev/v1",
        "kind": "Widget",
        "metadata": {
            "name":"widget-a",
            "revision":1,
            "ownerReferences":[reference.clone()]
        }
    }))
    .unwrap();
    assert_eq!(update.metadata.owner_references[0].uid(), uid);

    let resource: Resource = serde_json::from_value(json!({
        "apiVersion": "example.dev/v1",
        "kind": "Widget",
        "metadata": {"name":"widget-a","ownerReferences":[reference]}
    }))
    .unwrap();
    assert_eq!(resource.metadata.owner_references[0].uid(), uid);

    let empty: CreateResourceRequest = serde_json::from_value(json!({
        "apiVersion": "example.dev/v1",
        "kind": "Widget",
        "metadata": {"name":"widget-a"}
    }))
    .unwrap();
    assert!(empty.metadata.owner_references.is_empty());
    assert!(serde_json::to_value(empty).unwrap()["metadata"]
        .get("ownerReferences")
        .is_none());
}
