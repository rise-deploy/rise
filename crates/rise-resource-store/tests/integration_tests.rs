use rise_resource_api::{API_VERSION_V1ALPHA1, ORGANIZATION_KIND, RESOURCE_DEFINITION_KIND};
use rise_resource_store::{
    CreateResourceParams, DeleteOutcome, PgResourceStore, ResourceStore, StoreError,
    UpdateResourceParams,
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

    let list = store.list(ORGANIZATION_KIND, None).await.unwrap();
    assert_eq!(list.len(), 3);
    // Returned in name order
    assert_eq!(list[0].name, "alpha");
    assert_eq!(list[1].name, "beta");
    assert_eq!(list[2].name, "gamma");

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

    let outcome = store.delete(row.uid).await.unwrap();
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

    let outcome = store.delete(row.uid).await.unwrap();
    let marked = match outcome {
        DeleteOutcome::MarkedForDeletion(r) => *r,
        DeleteOutcome::Deleted => panic!("expected MarkedForDeletion"),
    };
    assert!(marked.deletion_timestamp.is_some());
    assert_eq!(marked.finalizers.len(), 1);

    Ok(())
}

#[sqlx::test]
async fn delete_with_children_rejected(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    let parent = store
        .create(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: ORGANIZATION_KIND.to_string(),
            name: "parent-org".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            spec: json!({"displayName": "Parent"}),
            validator: None,
        })
        .await
        .unwrap();

    store
        .create(CreateResourceParams {
            api_version: "example.dev/v1".to_string(),
            kind: "Widget".to_string(),
            name: "widget-a".to_string(),
            parent_uid: Some(parent.uid),
            annotations: BTreeMap::new(),
            finalizers: vec![],
            spec: json!({}),
            validator: None,
        })
        .await
        .unwrap();

    let err = store.delete(parent.uid).await.unwrap_err();
    assert!(matches!(err, StoreError::HasChildren));

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
    let outcome = store.delete(row.uid).await.unwrap();
    assert!(matches!(outcome, DeleteOutcome::MarkedForDeletion(_)));

    // Controller removes its finalizer
    let without_finalizer = store
        .update_controller_finalizers(row.uid, controller, &[], &[finalizer.to_string()])
        .await
        .unwrap();
    assert!(without_finalizer.finalizers.is_empty());

    // Second delete should now succeed
    let outcome2 = store.delete(row.uid).await.unwrap();
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
        .get_by_name(ORGANIZATION_KIND, "lookup-org", None)
        .await
        .unwrap();
    assert!(found.is_some());
    assert_eq!(found.unwrap().name, "lookup-org");

    let not_found = store
        .get_by_name(ORGANIZATION_KIND, "nonexistent", None)
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
    let outcome1 = store.delete(row.uid).await.unwrap();
    assert!(matches!(outcome1, DeleteOutcome::MarkedForDeletion(_)));

    // Second delete while finalizers are still present: re-marks (idempotent)
    let outcome2 = store.delete(row.uid).await.unwrap();
    assert!(matches!(outcome2, DeleteOutcome::MarkedForDeletion(_)));

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
