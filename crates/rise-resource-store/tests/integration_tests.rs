use rise_resource_api::{
    ResourceScope, API_VERSION_V1ALPHA1, ORGANIZATION_KIND, RESOURCE_DEFINITION_KIND,
};
use rise_resource_store::{
    CreateResourceParams, DeleteOutcome, PathSegment, PgResourceStore, PropagationPolicy,
    ResourceStore, StoreError, UpdateResourceParams, CASCADE_DELETION_FINALIZER,
};
use serde_json::json;
use std::collections::BTreeMap;
use uuid::Uuid;

#[sqlx::test]
async fn create_and_get_resource(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    let params = CreateResourceParams {
        api_version: API_VERSION_V1ALPHA1.to_string(),
        kind: ORGANIZATION_KIND.to_string(),
        name: "my-org".to_string(),
        parent_uid: None,
        annotations: BTreeMap::new(),
        finalizers: vec![],
        spec: json!({"displayName": "My Org"}),
        validator: None,
    };

    let row = store.create(params).await.unwrap();
    assert_eq!(row.name, "my-org");
    assert_eq!(row.kind, ORGANIZATION_KIND);
    assert_eq!(row.revision, 1);
    assert_eq!(row.discriminator.len(), 8);
    assert!(row.deletion_timestamp.is_none());

    let fetched = store.get(row.uid).await.unwrap().unwrap();
    assert_eq!(fetched.uid, row.uid);
    assert_eq!(fetched.name, "my-org");

    Ok(())
}

#[sqlx::test]
async fn list_resources(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    for name in ["beta", "alpha", "gamma"] {
        store
            .create(CreateResourceParams {
                api_version: API_VERSION_V1ALPHA1.to_string(),
                kind: ORGANIZATION_KIND.to_string(),
                name: name.to_string(),
                parent_uid: None,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                spec: json!({"displayName": name}),
                validator: None,
            })
            .await
            .unwrap();
    }

    let list = store
        .list(API_VERSION_V1ALPHA1, ORGANIZATION_KIND, None)
        .await
        .unwrap();
    assert_eq!(list.len(), 3);
    // Returned in name order
    assert_eq!(list[0].name, "alpha");
    assert_eq!(list[1].name, "beta");
    assert_eq!(list[2].name, "gamma");

    Ok(())
}

#[sqlx::test]
async fn same_kind_name_is_isolated_by_api_version(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    let first = store
        .create(CreateResourceParams {
            api_version: "alpha.example.dev/v1".to_string(),
            kind: "Widget".to_string(),
            name: "shared".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            spec: json!({}),
            validator: None,
        })
        .await
        .unwrap();

    let second = store
        .create(CreateResourceParams {
            api_version: "beta.example.dev/v1".to_string(),
            kind: "Widget".to_string(),
            name: "shared".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            spec: json!({}),
            validator: None,
        })
        .await
        .unwrap();

    let alpha = store
        .list("alpha.example.dev/v1", "Widget", None)
        .await
        .unwrap();
    assert_eq!(alpha.len(), 1);
    assert_eq!(alpha[0].uid, first.uid);

    let beta = store
        .get_by_name("beta.example.dev/v1", "Widget", "shared", None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(beta.uid, second.uid);

    Ok(())
}

#[sqlx::test]
async fn update_resource_increments_revision(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    let row = store
        .create(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: ORGANIZATION_KIND.to_string(),
            name: "my-org".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            spec: json!({"displayName": "My Org"}),
            validator: None,
        })
        .await
        .unwrap();

    assert_eq!(row.revision, 1);

    let updated = store
        .update(
            row.uid,
            UpdateResourceParams {
                revision: 1,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                spec: json!({"displayName": "Updated Org"}),
                validator: None,
            },
        )
        .await
        .unwrap();

    assert_eq!(updated.revision, 2);
    assert_eq!(updated.spec["displayName"], "Updated Org");

    Ok(())
}

#[sqlx::test]
async fn update_rejects_wrong_revision(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    let row = store
        .create(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: ORGANIZATION_KIND.to_string(),
            name: "my-org".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            spec: json!({"displayName": "My Org"}),
            validator: None,
        })
        .await
        .unwrap();

    let err = store
        .update(
            row.uid,
            UpdateResourceParams {
                revision: 99, // wrong
                annotations: BTreeMap::new(),
                finalizers: vec![],
                spec: json!({"displayName": "Updated"}),
                validator: None,
            },
        )
        .await
        .unwrap_err();

    assert!(matches!(
        err,
        StoreError::RevisionConflict {
            expected: 99,
            found: 1
        }
    ));

    Ok(())
}

#[sqlx::test]
async fn duplicate_name_returns_conflict(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    let params = || CreateResourceParams {
        api_version: API_VERSION_V1ALPHA1.to_string(),
        kind: ORGANIZATION_KIND.to_string(),
        name: "same-name".to_string(),
        parent_uid: None,
        annotations: BTreeMap::new(),
        finalizers: vec![],
        spec: json!({"displayName": "Org"}),
        validator: None,
    };

    store.create(params()).await.unwrap();
    let err = store.create(params()).await.unwrap_err();
    assert!(matches!(err, StoreError::NameConflict));

    Ok(())
}

#[sqlx::test]
async fn delete_without_finalizers_removes_row(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    let row = store
        .create(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: ORGANIZATION_KIND.to_string(),
            name: "to-delete".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            spec: json!({"displayName": "Delete Me"}),
            validator: None,
        })
        .await
        .unwrap();

    let outcome = store
        .delete(row.uid, PropagationPolicy::Cascade)
        .await
        .unwrap();
    assert!(matches!(outcome, DeleteOutcome::Deleted));
    assert!(store.get(row.uid).await.unwrap().is_none());

    Ok(())
}

#[sqlx::test]
async fn delete_with_finalizers_marks_deletion_timestamp(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    let row = store
        .create(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: ORGANIZATION_KIND.to_string(),
            name: "finalized".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec!["controller.example.com/cleanup".to_string()],
            spec: json!({"displayName": "Org"}),
            validator: None,
        })
        .await
        .unwrap();

    let outcome = store
        .delete(row.uid, PropagationPolicy::Cascade)
        .await
        .unwrap();
    let marked = match outcome {
        DeleteOutcome::MarkedForDeletion(r) => *r,
        DeleteOutcome::Deleted => panic!("expected MarkedForDeletion"),
    };
    assert!(marked.deletion_timestamp.is_some());
    assert_eq!(marked.finalizers.len(), 1);

    Ok(())
}

#[sqlx::test]
async fn finalizer_flow_completes_deletion(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);
    let controller = "controller.example.com";
    let finalizer = "controller.example.com/cleanup";

    let row = store
        .create(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: ORGANIZATION_KIND.to_string(),
            name: "with-finalizer".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![finalizer.to_string()],
            spec: json!({"displayName": "Org"}),
            validator: None,
        })
        .await
        .unwrap();

    // Delete marks timestamp but doesn't remove
    let outcome = store
        .delete(row.uid, PropagationPolicy::Cascade)
        .await
        .unwrap();
    assert!(matches!(outcome, DeleteOutcome::MarkedForDeletion(_)));

    // Controller removes its finalizer
    let without_finalizer = store
        .update_controller_finalizers(row.uid, controller, &[], &[finalizer.to_string()])
        .await
        .unwrap();
    assert!(without_finalizer.finalizers.is_empty());

    // try_collect now succeeds: row is tombstoned, no finalizers, no children.
    let outcome2 = store.try_collect(row.uid).await.unwrap();
    assert!(matches!(outcome2, DeleteOutcome::Deleted));

    Ok(())
}

#[sqlx::test]
async fn controller_status_update_merges_key(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    let row = store
        .create(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: ORGANIZATION_KIND.to_string(),
            name: "status-org".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            spec: json!({"displayName": "Org"}),
            validator: None,
        })
        .await
        .unwrap();

    let updated = store
        .update_controller_status(row.uid, "controller.example.com", json!({"ready": true}))
        .await
        .unwrap();

    assert_eq!(
        updated.status["controllers"]["controller.example.com"]["ready"],
        true
    );
    assert_eq!(updated.revision, 2);

    // Second update only affects that controller's key
    let updated2 = store
        .update_controller_status(row.uid, "other.example.com", json!({"synced": true}))
        .await
        .unwrap();

    assert_eq!(
        updated2.status["controllers"]["controller.example.com"]["ready"],
        true
    );
    assert_eq!(
        updated2.status["controllers"]["other.example.com"]["synced"],
        true
    );

    Ok(())
}

#[sqlx::test]
async fn controller_finalizers_enforces_ownership(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    let row = store
        .create(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: ORGANIZATION_KIND.to_string(),
            name: "owned".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            spec: json!({"displayName": "Org"}),
            validator: None,
        })
        .await
        .unwrap();

    // Adding a finalizer owned by a different controller should fail
    let err = store
        .update_controller_finalizers(
            row.uid,
            "controller.example.com",
            &["other.example.com/cleanup".to_string()],
            &[],
        )
        .await
        .unwrap_err();

    assert!(matches!(err, StoreError::Validation(_)));

    // Adding an owned finalizer succeeds
    store
        .update_controller_finalizers(
            row.uid,
            "controller.example.com",
            &["controller.example.com/cleanup".to_string()],
            &[],
        )
        .await
        .unwrap();

    Ok(())
}

#[sqlx::test]
async fn resolve_builtin_collection(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    let org_info = store.resolve_collection("organizations").await.unwrap();
    assert!(org_info.is_some());
    let info = org_info.unwrap();
    assert_eq!(info.kind, ORGANIZATION_KIND);
    assert_eq!(info.api_version, API_VERSION_V1ALPHA1);

    let rd_info = store
        .resolve_collection("resourcedefinitions")
        .await
        .unwrap();
    assert!(rd_info.is_some());
    assert_eq!(rd_info.unwrap().kind, RESOURCE_DEFINITION_KIND);

    let none = store.resolve_collection("unknown").await.unwrap();
    assert!(none.is_none());

    Ok(())
}

#[sqlx::test]
async fn register_resource_definition(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    let spec = json!({
        "group": "example.dev",
        "kind": "Widget",
        "plural": "widgets",
        "scope": "root",
        "versions": [{"name": "v1", "served": true, "storage": true}],
        "allowedStatusControllerIds": []
    });

    let row = store
        .register_resource_definition(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: RESOURCE_DEFINITION_KIND.to_string(),
            name: "widgets.example.dev".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            spec,
            validator: None,
        })
        .await
        .unwrap();

    assert_eq!(row.kind, RESOURCE_DEFINITION_KIND);

    // Should now resolve via the collection
    let info = store.resolve_collection("widgets").await.unwrap().unwrap();
    assert_eq!(info.kind, "Widget");
    assert_eq!(info.api_version, "example.dev/v1");

    Ok(())
}

#[sqlx::test]
async fn delete_resource_definition_rejects_existing_instances(
    pool: sqlx::PgPool,
) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    let spec = json!({
        "group": "example.dev",
        "kind": "Widget",
        "plural": "widgets",
        "scope": "root",
        "versions": [{"name": "v1", "served": true, "storage": true}],
        "allowedStatusControllerIds": []
    });

    let definition = store
        .register_resource_definition(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: RESOURCE_DEFINITION_KIND.to_string(),
            name: "widgets.example.dev".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            spec,
            validator: None,
        })
        .await
        .unwrap();

    store
        .create(CreateResourceParams {
            api_version: "example.dev/v1".to_string(),
            kind: "Widget".to_string(),
            name: "w1".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            spec: json!({}),
            validator: None,
        })
        .await
        .unwrap();

    let err = store
        .delete(definition.uid, PropagationPolicy::Cascade)
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Validation(_)),
        "expected validation error, got {err:?}"
    );
    assert!(store.resolve_collection("widgets").await.unwrap().is_some());

    Ok(())
}

#[sqlx::test]
async fn register_resource_definition_rejects_reserved_plural(
    pool: sqlx::PgPool,
) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    let spec = json!({
        "group": "example.dev",
        "kind": "Organization",
        "plural": "organizations",
        "scope": "root",
        "versions": [{"name": "v1", "served": true, "storage": true}],
        "allowedStatusControllerIds": []
    });

    let err = store
        .register_resource_definition(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: RESOURCE_DEFINITION_KIND.to_string(),
            name: "organizations.example.dev".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            spec,
            validator: None,
        })
        .await
        .unwrap_err();

    assert!(matches!(err, StoreError::Validation(_)));

    Ok(())
}

#[sqlx::test]
async fn get_by_name(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    store
        .create(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: ORGANIZATION_KIND.to_string(),
            name: "lookup-org".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            spec: json!({"displayName": "Lookup Org"}),
            validator: None,
        })
        .await
        .unwrap();

    let found = store
        .get_by_name(API_VERSION_V1ALPHA1, ORGANIZATION_KIND, "lookup-org", None)
        .await
        .unwrap();
    assert!(found.is_some());
    assert_eq!(found.unwrap().name, "lookup-org");

    let not_found = store
        .get_by_name(API_VERSION_V1ALPHA1, ORGANIZATION_KIND, "nonexistent", None)
        .await
        .unwrap();
    assert!(not_found.is_none());

    Ok(())
}

#[sqlx::test]
async fn get_returns_none_for_unknown_uid(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);
    let result = store.get(Uuid::new_v4()).await.unwrap();
    assert!(result.is_none());
    Ok(())
}

#[sqlx::test]
async fn create_rejects_invalid_name(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    // Leading hyphen violates the name format constraint
    let err = store
        .create(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: ORGANIZATION_KIND.to_string(),
            name: "--bad-name".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            spec: json!({"displayName": "Bad"}),
            validator: None,
        })
        .await
        .unwrap_err();

    assert!(
        matches!(err, StoreError::Validation(_)),
        "expected Validation error, got {err:?}"
    );

    Ok(())
}

#[sqlx::test]
async fn create_invokes_spec_validator(pool: sqlx::PgPool) -> sqlx::Result<()> {
    use rise_resource_store::OrganizationValidator;
    use std::sync::Arc;

    let store = PgResourceStore::new(pool);

    // OrganizationValidator requires a non-empty displayName
    let err = store
        .create(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: ORGANIZATION_KIND.to_string(),
            name: "bad-spec-org".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            spec: json!({"displayName": "   "}),
            validator: Some(Arc::new(OrganizationValidator)),
        })
        .await
        .unwrap_err();

    assert!(
        matches!(err, StoreError::Validation(_)),
        "expected Validation error, got {err:?}"
    );

    Ok(())
}

#[sqlx::test]
async fn delete_already_marked_resource_is_idempotent(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    let row = store
        .create(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: ORGANIZATION_KIND.to_string(),
            name: "marked-org".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec!["controller.example.com/cleanup".to_string()],
            spec: json!({"displayName": "Org"}),
            validator: None,
        })
        .await
        .unwrap();

    // First delete: marks for deletion
    let outcome1 = store
        .delete(row.uid, PropagationPolicy::Cascade)
        .await
        .unwrap();
    let first_ts = match outcome1 {
        DeleteOutcome::MarkedForDeletion(r) => r.deletion_timestamp,
        DeleteOutcome::Deleted => panic!("expected MarkedForDeletion"),
    };
    assert!(first_ts.is_some());

    // Second delete while finalizers are still present: re-marks (idempotent) and preserves
    // the original deletion_timestamp.
    let outcome2 = store
        .delete(row.uid, PropagationPolicy::Cascade)
        .await
        .unwrap();
    let second = match outcome2 {
        DeleteOutcome::MarkedForDeletion(r) => *r,
        DeleteOutcome::Deleted => panic!("expected MarkedForDeletion"),
    };
    assert_eq!(second.deletion_timestamp, first_ts);

    Ok(())
}

#[sqlx::test]
async fn update_resource_definition_updates_projection(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    let spec_v1 = json!({
        "group": "example.dev",
        "kind": "Widget",
        "plural": "widgets",
        "scope": "root",
        "versions": [{"name": "v1", "served": true, "storage": true}],
        "allowedStatusControllerIds": []
    });

    let row = store
        .register_resource_definition(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: RESOURCE_DEFINITION_KIND.to_string(),
            name: "widgets.example.dev".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            spec: spec_v1,
            validator: None,
        })
        .await
        .unwrap();

    // Add a v2 version (identity fields unchanged)
    let spec_v2 = json!({
        "group": "example.dev",
        "kind": "Widget",
        "plural": "widgets",
        "scope": "root",
        "versions": [
            {"name": "v1", "served": true, "storage": false},
            {"name": "v2", "served": true, "storage": true}
        ],
        "allowedStatusControllerIds": []
    });

    let updated = store
        .update_resource_definition(
            row.uid,
            UpdateResourceParams {
                revision: row.revision,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                spec: spec_v2,
                validator: None,
            },
        )
        .await
        .unwrap();

    assert_eq!(updated.revision, 2);

    // resolve_collection should reflect the new storage version
    let info = store.resolve_collection("widgets").await.unwrap().unwrap();
    assert_eq!(info.api_version, "example.dev/v2");

    Ok(())
}

#[sqlx::test]
async fn update_resource_definition_rejects_identity_change(
    pool: sqlx::PgPool,
) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    let spec = json!({
        "group": "example.dev",
        "kind": "Widget",
        "plural": "widgets",
        "scope": "root",
        "versions": [{"name": "v1", "served": true, "storage": true}],
        "allowedStatusControllerIds": []
    });

    let row = store
        .register_resource_definition(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: RESOURCE_DEFINITION_KIND.to_string(),
            name: "widgets.example.dev".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            spec,
            validator: None,
        })
        .await
        .unwrap();

    // Attempt to change the group — must be rejected
    let changed_group_spec = json!({
        "group": "changed.dev",
        "kind": "Widget",
        "plural": "widgets",
        "scope": "root",
        "versions": [{"name": "v1", "served": true, "storage": true}],
        "allowedStatusControllerIds": []
    });

    let err = store
        .update_resource_definition(
            row.uid,
            UpdateResourceParams {
                revision: row.revision,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                spec: changed_group_spec,
                validator: None,
            },
        )
        .await
        .unwrap_err();

    assert!(
        matches!(err, StoreError::Validation(_)),
        "expected Validation error for identity change, got {err:?}"
    );

    Ok(())
}

#[sqlx::test]
async fn update_rejects_resource_definition(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    let spec = json!({
        "group": "example.dev",
        "kind": "Gadget",
        "plural": "gadgets",
        "scope": "root",
        "versions": [{"name": "v1", "served": true, "storage": true}],
        "allowedStatusControllerIds": []
    });

    let row = store
        .register_resource_definition(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: RESOURCE_DEFINITION_KIND.to_string(),
            name: "gadgets.example.dev".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            spec: spec.clone(),
            validator: None,
        })
        .await
        .unwrap();

    // Calling update() on a ResourceDefinition must be rejected
    let err = store
        .update(
            row.uid,
            UpdateResourceParams {
                revision: row.revision,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                spec,
                validator: None,
            },
        )
        .await
        .unwrap_err();

    assert!(
        matches!(err, StoreError::Validation(_)),
        "expected Validation error, got {err:?}"
    );

    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Cascade / orphan deletion
// ---------------------------------------------------------------------------------------------

async fn create_org(store: &PgResourceStore, name: &str) -> rise_resource_store::ResourceRow {
    store
        .create(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: ORGANIZATION_KIND.to_string(),
            name: name.to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            spec: json!({"displayName": name}),
            validator: None,
        })
        .await
        .unwrap()
}

async fn create_child(
    store: &PgResourceStore,
    parent: Uuid,
    kind: &str,
    name: &str,
    finalizers: Vec<String>,
) -> rise_resource_store::ResourceRow {
    store
        .create(CreateResourceParams {
            api_version: "example.dev/v1".to_string(),
            kind: kind.to_string(),
            name: name.to_string(),
            parent_uid: Some(parent),
            annotations: BTreeMap::new(),
            finalizers,
            spec: json!({}),
            validator: None,
        })
        .await
        .unwrap()
}

#[sqlx::test]
async fn cascade_delete_stamps_immediate_children(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);
    let parent = create_org(&store, "cascade-org").await;
    let c1 = create_child(&store, parent.uid, "Widget", "w1", vec![]).await;
    let c2 = create_child(&store, parent.uid, "Widget", "w2", vec![]).await;

    let outcome = store
        .delete(parent.uid, PropagationPolicy::Cascade)
        .await
        .unwrap();
    let marked = match outcome {
        DeleteOutcome::MarkedForDeletion(r) => *r,
        DeleteOutcome::Deleted => panic!("parent still has children, should be MarkedForDeletion"),
    };
    assert!(marked.deletion_timestamp.is_some());
    assert!(
        marked
            .finalizers
            .contains(&CASCADE_DELETION_FINALIZER.to_string()),
        "parent missing cascade finalizer: {:?}",
        marked.finalizers
    );

    let c1_after = store.get(c1.uid).await.unwrap().unwrap();
    let c2_after = store.get(c2.uid).await.unwrap().unwrap();
    assert!(c1_after.deletion_timestamp.is_some());
    assert!(c2_after.deletion_timestamp.is_some());

    Ok(())
}

#[sqlx::test]
async fn cascade_delete_idempotent(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);
    let parent = create_org(&store, "cascade-org").await;
    create_child(&store, parent.uid, "Widget", "w1", vec![]).await;

    let first = match store
        .delete(parent.uid, PropagationPolicy::Cascade)
        .await
        .unwrap()
    {
        DeleteOutcome::MarkedForDeletion(r) => r.deletion_timestamp,
        _ => panic!("expected MarkedForDeletion"),
    };
    let second = match store
        .delete(parent.uid, PropagationPolicy::Cascade)
        .await
        .unwrap()
    {
        DeleteOutcome::MarkedForDeletion(r) => {
            let count = r
                .finalizers
                .iter()
                .filter(|f| f.as_str() == CASCADE_DELETION_FINALIZER)
                .count();
            assert_eq!(count, 1, "cascade finalizer duplicated: {:?}", r.finalizers);
            r.deletion_timestamp
        }
        _ => panic!("expected MarkedForDeletion"),
    };
    assert_eq!(
        first, second,
        "deletion_timestamp must be preserved on re-delete"
    );

    Ok(())
}

#[sqlx::test]
async fn cascade_collection_drains_bottom_up(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);
    let parent = create_org(&store, "drain-org").await;
    let child = create_child(
        &store,
        parent.uid,
        "Widget",
        "w1",
        vec!["controller.example.com/cleanup".to_string()],
    )
    .await;

    // Delete the parent. Child gets stamped, parent gets cascade finalizer.
    store
        .delete(parent.uid, PropagationPolicy::Cascade)
        .await
        .unwrap();

    // try_collect on parent while child still exists: stays marked, cascade finalizer persists.
    let outcome = store.try_collect(parent.uid).await.unwrap();
    let p_marked = match outcome {
        DeleteOutcome::MarkedForDeletion(r) => *r,
        _ => panic!("parent should still be marked"),
    };
    assert!(p_marked
        .finalizers
        .contains(&CASCADE_DELETION_FINALIZER.to_string()));

    // try_collect on the child while it has controller finalizers: still marked, no hard delete.
    let outcome = store.try_collect(child.uid).await.unwrap();
    assert!(matches!(outcome, DeleteOutcome::MarkedForDeletion(_)));
    assert!(store.get(child.uid).await.unwrap().is_some());

    // Controller clears its finalizer.
    store
        .update_controller_finalizers(
            child.uid,
            "controller.example.com",
            &[],
            &["controller.example.com/cleanup".to_string()],
        )
        .await
        .unwrap();

    // try_collect on child now hard-deletes it.
    let outcome = store.try_collect(child.uid).await.unwrap();
    assert!(matches!(outcome, DeleteOutcome::Deleted));
    assert!(store.get(child.uid).await.unwrap().is_none());

    // try_collect on parent: no remaining children → cascade finalizer cleared, hard-delete.
    let outcome = store.try_collect(parent.uid).await.unwrap();
    assert!(matches!(outcome, DeleteOutcome::Deleted));
    assert!(store.get(parent.uid).await.unwrap().is_none());

    Ok(())
}

#[sqlx::test]
async fn try_collect_on_live_row_is_noop(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);
    let parent = create_org(&store, "live-org").await;

    let outcome = store.try_collect(parent.uid).await.unwrap();
    match outcome {
        DeleteOutcome::MarkedForDeletion(r) => {
            assert!(r.deletion_timestamp.is_none());
        }
        DeleteOutcome::Deleted => panic!("live row should not be deleted"),
    }
    assert!(store.get(parent.uid).await.unwrap().is_some());

    Ok(())
}

#[sqlx::test]
async fn list_pending_collection_returns_tombstoned_only(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);
    let alive = create_org(&store, "alive-org").await;
    let dying = create_org(&store, "dying-org").await;

    store
        .update_controller_finalizers(
            dying.uid,
            "controller.example.com",
            &["controller.example.com/cleanup".to_string()],
            &[],
        )
        .await
        .unwrap();
    store
        .delete(dying.uid, PropagationPolicy::Cascade)
        .await
        .unwrap();

    let pending = store.list_pending_collection(100).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].uid, dying.uid);
    assert!(pending.iter().all(|r| r.uid != alive.uid));

    Ok(())
}

#[sqlx::test]
async fn orphan_delete_detaches_children(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);
    let parent = create_org(&store, "orphan-org").await;
    let c1 = create_child(&store, parent.uid, "Widget", "w1", vec![]).await;
    let c2 = create_child(&store, parent.uid, "Widget", "w2", vec![]).await;

    let outcome = store
        .delete(parent.uid, PropagationPolicy::Orphan)
        .await
        .unwrap();
    assert!(matches!(outcome, DeleteOutcome::Deleted));
    assert!(store.get(parent.uid).await.unwrap().is_none());

    let c1_after = store.get(c1.uid).await.unwrap().unwrap();
    let c2_after = store.get(c2.uid).await.unwrap().unwrap();
    assert_eq!(c1_after.parent_uid, None);
    assert_eq!(c2_after.parent_uid, None);
    assert!(c1_after.deletion_timestamp.is_none());

    Ok(())
}

#[sqlx::test]
async fn cascade_finalizer_not_addable_by_controller(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);
    let row = create_org(&store, "guarded").await;

    let err = store
        .update_controller_finalizers(
            row.uid,
            "controller.example.com",
            &[CASCADE_DELETION_FINALIZER.to_string()],
            &[],
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::ReservedFinalizer(_)),
        "expected ReservedFinalizer, got {err:?}"
    );

    let err = store
        .update_controller_finalizers(
            row.uid,
            "controller.example.com",
            &[],
            &[CASCADE_DELETION_FINALIZER.to_string()],
        )
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::ReservedFinalizer(_)));

    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------------------------

#[sqlx::test]
async fn resolve_path_walks_named_segments(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);
    let org = create_org(&store, "acme").await;
    let widget = create_child(&store, org.uid, "Widget", "w1", vec![]).await;

    let chain = store
        .resolve_path(&[
            PathSegment::Name {
                api_version: API_VERSION_V1ALPHA1.to_string(),
                kind: ORGANIZATION_KIND.to_string(),
                name: "acme".to_string(),
            },
            PathSegment::Name {
                api_version: "example.dev/v1".to_string(),
                kind: "Widget".to_string(),
                name: "w1".to_string(),
            },
        ])
        .await
        .unwrap();
    assert_eq!(chain.len(), 2);
    assert_eq!(chain[0].uid, org.uid);
    assert_eq!(chain[1].uid, widget.uid);

    Ok(())
}

#[sqlx::test]
async fn resolve_path_supports_uid_segments(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);
    let org = create_org(&store, "acme").await;
    let widget = create_child(&store, org.uid, "Widget", "w1", vec![]).await;

    let chain = store
        .resolve_path(&[
            PathSegment::Uid {
                api_version: API_VERSION_V1ALPHA1.to_string(),
                kind: ORGANIZATION_KIND.to_string(),
                uid: org.uid,
            },
            PathSegment::Name {
                api_version: "example.dev/v1".to_string(),
                kind: "Widget".to_string(),
                name: "w1".to_string(),
            },
        ])
        .await
        .unwrap();
    assert_eq!(chain.len(), 2);
    assert_eq!(chain[1].uid, widget.uid);

    Ok(())
}

#[sqlx::test]
async fn resolve_path_rejects_kind_mismatch(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);
    let org = create_org(&store, "acme").await;

    let err = store
        .resolve_path(&[PathSegment::Uid {
            api_version: "example.dev/v1".to_string(),
            kind: "Widget".to_string(),
            uid: org.uid,
        }])
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::KindMismatch { .. }),
        "expected KindMismatch, got {err:?}"
    );

    Ok(())
}

#[sqlx::test]
async fn resolve_path_rejects_wrong_subtree(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);
    let org_a = create_org(&store, "org-a").await;
    let org_b = create_org(&store, "org-b").await;
    let widget_b = create_child(&store, org_b.uid, "Widget", "w1", vec![]).await;

    let err = store
        .resolve_path(&[
            PathSegment::Uid {
                api_version: API_VERSION_V1ALPHA1.to_string(),
                kind: ORGANIZATION_KIND.to_string(),
                uid: org_a.uid,
            },
            PathSegment::Uid {
                api_version: "example.dev/v1".to_string(),
                kind: "Widget".to_string(),
                uid: widget_b.uid,
            },
        ])
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::ParentNotFound));

    Ok(())
}

#[sqlx::test]
async fn resolve_path_returns_tombstoned_rows(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);
    let org = create_org(&store, "acme").await;
    let widget = create_child(
        &store,
        org.uid,
        "Widget",
        "w1",
        vec!["controller.example.com/cleanup".to_string()],
    )
    .await;
    store
        .delete(widget.uid, PropagationPolicy::Cascade)
        .await
        .unwrap();

    let chain = store
        .resolve_path(&[
            PathSegment::Name {
                api_version: API_VERSION_V1ALPHA1.to_string(),
                kind: ORGANIZATION_KIND.to_string(),
                name: "acme".to_string(),
            },
            PathSegment::Name {
                api_version: "example.dev/v1".to_string(),
                kind: "Widget".to_string(),
                name: "w1".to_string(),
            },
        ])
        .await
        .unwrap();
    assert_eq!(chain.len(), 2);
    assert!(chain[1].deletion_timestamp.is_some());

    Ok(())
}

#[sqlx::test]
async fn resolve_path_empty_returns_error(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);
    let err = store.resolve_path(&[]).await.unwrap_err();
    assert!(matches!(err, StoreError::EmptyPath));
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Orphan discovery / reparent
// ---------------------------------------------------------------------------------------------

#[sqlx::test]
async fn list_orphans_returns_children_of_tombstoned_parent(
    pool: sqlx::PgPool,
) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);
    let dying_parent = create_org(&store, "dying").await;
    store
        .update_controller_finalizers(
            dying_parent.uid,
            "controller.example.com",
            &["controller.example.com/cleanup".to_string()],
            &[],
        )
        .await
        .unwrap();
    let child_dying = create_child(&store, dying_parent.uid, "Widget", "w1", vec![]).await;

    let alive_parent = create_org(&store, "alive").await;
    let _child_alive = create_child(&store, alive_parent.uid, "Widget", "w2", vec![]).await;

    store
        .delete(dying_parent.uid, PropagationPolicy::Cascade)
        .await
        .unwrap();

    let orphans = store.list_orphans(None).await.unwrap();
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].uid, child_dying.uid);

    let scoped = store.list_orphans(Some(dying_parent.uid)).await.unwrap();
    assert_eq!(scoped.len(), 1);

    let none_scoped = store.list_orphans(Some(alive_parent.uid)).await.unwrap();
    assert!(none_scoped.is_empty());

    Ok(())
}

#[sqlx::test]
async fn reparent_moves_resource(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);
    let org_a = create_org(&store, "org-a").await;
    let org_b = create_org(&store, "org-b").await;
    let widget = create_child(&store, org_a.uid, "Widget", "w1", vec![]).await;

    let moved = store
        .reparent(widget.uid, Some(org_b.uid), ResourceScope::Organization)
        .await
        .unwrap();
    assert_eq!(moved.parent_uid, Some(org_b.uid));

    let under_a = store
        .list("example.dev/v1", "Widget", Some(org_a.uid))
        .await
        .unwrap();
    assert!(under_a.is_empty());
    let under_b = store
        .list("example.dev/v1", "Widget", Some(org_b.uid))
        .await
        .unwrap();
    assert_eq!(under_b.len(), 1);
    assert_eq!(under_b[0].uid, widget.uid);

    Ok(())
}

#[sqlx::test]
async fn reparent_to_root(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);
    let org_a = create_org(&store, "org-a").await;
    let child_org = store
        .create(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: ORGANIZATION_KIND.to_string(),
            name: "child".to_string(),
            parent_uid: Some(org_a.uid),
            annotations: BTreeMap::new(),
            finalizers: vec![],
            spec: json!({"displayName": "Child"}),
            validator: None,
        })
        .await
        .unwrap();

    let moved = store
        .reparent(child_org.uid, None, ResourceScope::Root)
        .await
        .unwrap();
    assert_eq!(moved.parent_uid, None);

    Ok(())
}

#[sqlx::test]
async fn reparent_rejects_cycle(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);
    let org = create_org(&store, "root").await;
    let child_org = store
        .create(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: ORGANIZATION_KIND.to_string(),
            name: "child".to_string(),
            parent_uid: Some(org.uid),
            annotations: BTreeMap::new(),
            finalizers: vec![],
            spec: json!({"displayName": "Child"}),
            validator: None,
        })
        .await
        .unwrap();

    let err = store
        .reparent(org.uid, Some(org.uid), ResourceScope::Organization)
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::ReparentCycle));

    let err = store
        .reparent(org.uid, Some(child_org.uid), ResourceScope::Organization)
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::ReparentCycle));

    Ok(())
}

#[sqlx::test]
async fn reparent_rejects_root_scoped_resource_under_parent(
    pool: sqlx::PgPool,
) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);
    let org_a = create_org(&store, "org-a").await;
    let org_b = create_org(&store, "org-b").await;

    let err = store
        .reparent(org_a.uid, Some(org_b.uid), ResourceScope::Root)
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Validation(_)),
        "expected scope validation error, got {err:?}"
    );

    let unchanged = store.get(org_a.uid).await.unwrap().unwrap();
    assert_eq!(unchanged.parent_uid, None);

    Ok(())
}

#[sqlx::test]
async fn reparent_rejects_org_scoped_resource_to_root(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);
    let org = create_org(&store, "org").await;
    let widget = create_child(&store, org.uid, "Widget", "w1", vec![]).await;

    let err = store
        .reparent(widget.uid, None, ResourceScope::Organization)
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Validation(_)),
        "expected scope validation error, got {err:?}"
    );

    let unchanged = store.get(widget.uid).await.unwrap().unwrap();
    assert_eq!(unchanged.parent_uid, Some(org.uid));

    Ok(())
}

#[sqlx::test]
async fn reparent_rejects_org_scoped_resource_under_non_org(
    pool: sqlx::PgPool,
) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);
    let org = create_org(&store, "org").await;
    let widget = create_child(&store, org.uid, "Widget", "w1", vec![]).await;
    let other_widget = create_child(&store, org.uid, "Widget", "w2", vec![]).await;

    let err = store
        .reparent(
            widget.uid,
            Some(other_widget.uid),
            ResourceScope::Organization,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Validation(_)),
        "expected scope validation error, got {err:?}"
    );

    let unchanged = store.get(widget.uid).await.unwrap().unwrap();
    assert_eq!(unchanged.parent_uid, Some(org.uid));

    Ok(())
}

#[sqlx::test]
async fn reparent_respects_uniqueness(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);
    let org_a = create_org(&store, "org-a").await;
    let org_b = create_org(&store, "org-b").await;
    let _existing = create_child(&store, org_b.uid, "Widget", "shared", vec![]).await;
    let moving = create_child(&store, org_a.uid, "Widget", "shared", vec![]).await;

    let err = store
        .reparent(moving.uid, Some(org_b.uid), ResourceScope::Organization)
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::NameConflict));

    Ok(())
}
