use rise_resource_api::{
    CreateResourceParams, DeleteOutcome, OwnerReference, PathSegment, ResourceRow, ResourceStore,
    StoreError, UpdateResourceParams, API_VERSION_V1ALPHA1, CASCADE_DELETION_FINALIZER,
    ORGANIZATION_KIND, RESOURCE_DEFINITION_KIND,
};
use rise_resource_store_postgres::PgResourceStore;
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
        owner_references: vec![],
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
async fn generic_create_rejects_resource_definitions(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    let error = store
        .create(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: RESOURCE_DEFINITION_KIND.to_string(),
            name: "users.rise.dev".to_string(),
            spec: json!({
                "group": "rise.dev",
                "kind": "User",
                "plural": "users",
                "versions": [{"name": "v1", "served": true, "storage": true}],
                "allowedStatusControllerIds": []
            }),
            ..Default::default()
        })
        .await
        .unwrap_err();

    assert!(matches!(error, StoreError::Validation(message) if
        message.contains("register_resource_definition")));

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
                owner_references: vec![],
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

    // Register ResourceDefinitions for both groups before creating instances.
    for (group, plural) in [
        ("alpha.example.dev", "alpha-widgets"),
        ("beta.example.dev", "beta-widgets"),
    ] {
        store
            .register_resource_definition(CreateResourceParams {
                api_version: API_VERSION_V1ALPHA1.to_string(),
                kind: RESOURCE_DEFINITION_KIND.to_string(),
                name: format!("{plural}.{group}"),
                parent_uid: None,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                owner_references: vec![],
                spec: json!({
                    "group": group,
                    "kind": "Widget",
                    "plural": plural,
                    "versions": [{"name": "v1", "served": true, "storage": true}],
                    "allowedStatusControllerIds": []
                }),
                validator: None,
            })
            .await
            .unwrap();
    }

    let first = store
        .create(CreateResourceParams {
            api_version: "alpha.example.dev/v1".to_string(),
            kind: "Widget".to_string(),
            name: "shared".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            owner_references: vec![],
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
            owner_references: vec![],
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
            owner_references: vec![],
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
                api_version: None,
                revision: 1,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                owner_references: vec![],
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
            owner_references: vec![],
            spec: json!({"displayName": "My Org"}),
            validator: None,
        })
        .await
        .unwrap();

    let err = store
        .update(
            row.uid,
            UpdateResourceParams {
                api_version: None,
                revision: 99, // wrong
                annotations: BTreeMap::new(),
                finalizers: vec![],
                owner_references: vec![],
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
        owner_references: vec![],
        spec: json!({"displayName": "Org"}),
        validator: None,
    };

    store.create(params()).await.unwrap();
    let err = store.create(params()).await.unwrap_err();
    assert!(matches!(err, StoreError::NameConflict));

    Ok(())
}

#[sqlx::test]
async fn duplicate_name_returns_conflict_for_rd_backed_kind(
    pool: sqlx::PgPool,
) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);
    register_example_widget_rd(&store).await;

    let params = || CreateResourceParams {
        api_version: "example.dev/v1".to_string(),
        kind: "Widget".to_string(),
        name: "same-name".to_string(),
        parent_uid: None,
        annotations: BTreeMap::new(),
        finalizers: vec![],
        owner_references: vec![],
        spec: json!({}),
        validator: None,
    };

    store.create(params()).await.unwrap();
    let err = store.create(params()).await.unwrap_err();
    assert!(
        matches!(err, StoreError::NameConflict),
        "expected NameConflict, got {err:?}"
    );

    Ok(())
}

#[sqlx::test]
async fn create_rejects_non_storage_version_at_root(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);
    register_example_widget_rd(&store).await;

    let err = store
        .create(CreateResourceParams {
            api_version: "example.dev/v2".to_string(),
            kind: "Widget".to_string(),
            name: "non-storage".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            owner_references: vec![],
            spec: json!({}),
            validator: None,
        })
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Validation(_)),
        "expected Validation for non-storage create, got {err:?}"
    );

    Ok(())
}

#[sqlx::test]
async fn create_rejects_non_storage_version_in_child_scope(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);
    register_example_widget_rd(&store).await;
    let parent = create_org(&store, "parent-org").await;

    let err = store
        .create(CreateResourceParams {
            api_version: "example.dev/v2".to_string(),
            kind: "Widget".to_string(),
            name: "non-storage".to_string(),
            parent_uid: Some(parent.uid),
            annotations: BTreeMap::new(),
            finalizers: vec![],
            owner_references: vec![],
            spec: json!({}),
            validator: None,
        })
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Validation(_)),
        "expected Validation for non-storage create, got {err:?}"
    );

    Ok(())
}

#[sqlx::test]
async fn update_api_version_collision_returns_name_conflict(
    pool: sqlx::PgPool,
) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    // Register ResourceDefinitions for both groups before creating instances.
    for (group, plural) in [
        ("alpha.example.dev", "alpha-widgets"),
        ("beta.example.dev", "beta-widgets"),
    ] {
        store
            .register_resource_definition(CreateResourceParams {
                api_version: API_VERSION_V1ALPHA1.to_string(),
                kind: RESOURCE_DEFINITION_KIND.to_string(),
                name: format!("{plural}.{group}"),
                parent_uid: None,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                owner_references: vec![],
                spec: json!({
                    "group": group,
                    "kind": "Widget",
                    "plural": plural,
                    "versions": [{"name": "v1", "served": true, "storage": true}],
                    "allowedStatusControllerIds": []
                }),
                validator: None,
            })
            .await
            .unwrap();
    }

    // Two resources with the same kind + name but in different groups — allowed at root.
    let moving = store
        .create(CreateResourceParams {
            api_version: "alpha.example.dev/v1".to_string(),
            kind: "Widget".to_string(),
            name: "shared".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            owner_references: vec![],
            spec: json!({}),
            validator: None,
        })
        .await
        .unwrap();
    store
        .create(CreateResourceParams {
            api_version: "beta.example.dev/v1".to_string(),
            kind: "Widget".to_string(),
            name: "shared".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            owner_references: vec![],
            spec: json!({}),
            validator: None,
        })
        .await
        .unwrap();

    // Updating `moving` into the beta group collides on (group, kind, name). The partial unique
    // index violation must surface as NameConflict (409), not a raw Database error (500).
    let err = store
        .update(
            moving.uid,
            UpdateResourceParams {
                api_version: Some("beta.example.dev/v1".to_string()),
                revision: moving.revision,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                owner_references: vec![],
                spec: json!({}),
                validator: None,
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::NameConflict),
        "expected NameConflict, got {err:?}"
    );

    Ok(())
}

#[sqlx::test]
async fn update_rejects_non_storage_version(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);
    register_example_widget_rd(&store).await;

    let widget = store
        .create(CreateResourceParams {
            api_version: "example.dev/v1".to_string(),
            kind: "Widget".to_string(),
            name: "w1".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            owner_references: vec![],
            spec: json!({}),
            validator: None,
        })
        .await
        .unwrap();

    // v2 is served-only (storage: false) — migrating a row to a non-storage
    // version must be rejected.
    let err = store
        .update(
            widget.uid,
            UpdateResourceParams {
                api_version: Some("example.dev/v2".to_string()),
                revision: widget.revision,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                owner_references: vec![],
                spec: json!({}),
                validator: None,
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Validation(_)),
        "expected Validation for non-storage update target, got {err:?}"
    );

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
            owner_references: vec![],
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
            owner_references: vec![],
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
            owner_references: vec![],
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
            owner_references: vec![],
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
            owner_references: vec![],
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
async fn resolve_collection_by_kind_resolves_builtins_and_rds(
    pool: sqlx::PgPool,
) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    // Built-in kinds resolve without a `resource_definitions` row.
    let org = store
        .resolve_collection_by_kind("rise.dev", ORGANIZATION_KIND)
        .await
        .unwrap()
        .expect("Organization resolves by kind");
    assert_eq!(org.kind, ORGANIZATION_KIND);

    let rd = store
        .resolve_collection_by_kind("rise.dev", RESOURCE_DEFINITION_KIND)
        .await
        .unwrap()
        .expect("ResourceDefinition resolves by kind");
    assert_eq!(rd.kind, RESOURCE_DEFINITION_KIND);

    // An unknown kind resolves to `None`.
    assert!(store
        .resolve_collection_by_kind("rise.dev", "Nonexistent")
        .await
        .unwrap()
        .is_none());

    // A registered ResourceDefinition resolves by `(group, kind)`.
    store
        .register_resource_definition(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: RESOURCE_DEFINITION_KIND.to_string(),
            name: "widgets.example.dev".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            owner_references: vec![],
            spec: json!({
                "group": "example.dev",
                "kind": "Widget",
                "plural": "widgets",
                "versions": [{"name": "v1", "served": true, "storage": true}],
                "allowedStatusControllerIds": []
            }),
            validator: None,
        })
        .await
        .unwrap();

    let widget = store
        .resolve_collection_by_kind("example.dev", "Widget")
        .await
        .unwrap()
        .expect("Widget RD resolves by kind");
    assert_eq!(widget.kind, "Widget");
    assert_eq!(widget.api_version, "example.dev/v1");

    // The same kind under a different group does not resolve.
    assert!(store
        .resolve_collection_by_kind("other.dev", "Widget")
        .await
        .unwrap()
        .is_none());

    // A collection whose storage version is not served still resolves by kind:
    // the fallback picks a served version since ancestors are not
    // version-addressed.
    store
        .register_resource_definition(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: RESOURCE_DEFINITION_KIND.to_string(),
            name: "gauges.example.dev".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            owner_references: vec![],
            spec: json!({
                "group": "example.dev",
                "kind": "Gauge",
                "plural": "gauges",
                "versions": [
                    {"name": "v1", "served": false, "storage": true},
                    {"name": "v2", "served": true, "storage": false}
                ],
                "allowedStatusControllerIds": []
            }),
            validator: None,
        })
        .await
        .unwrap();

    let gauge = store
        .resolve_collection_by_kind("example.dev", "Gauge")
        .await
        .unwrap()
        .expect("Gauge RD with a non-served storage version resolves by kind");
    assert_eq!(gauge.kind, "Gauge");

    Ok(())
}

#[sqlx::test]
async fn register_resource_definition(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    let spec = json!({
        "group": "example.dev",
        "kind": "Widget",
        "plural": "widgets",
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
            owner_references: vec![],
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
            owner_references: vec![],
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
            owner_references: vec![],
            spec: json!({}),
            validator: None,
        })
        .await
        .unwrap();

    let err = store.delete(definition.uid).await.unwrap_err();
    assert!(
        matches!(err, StoreError::Validation(_)),
        "expected validation error, got {err:?}"
    );
    assert!(store.resolve_collection("widgets").await.unwrap().is_some());

    Ok(())
}

#[sqlx::test]
async fn delete_resource_definition_rejects_instances_in_any_served_version(
    pool: sqlx::PgPool,
) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    let spec_v1_storage = json!({
        "group": "example.dev",
        "kind": "Widget",
        "plural": "widgets",
        "versions": [
            {"name": "v1", "served": true, "storage": true},
            {"name": "v2", "served": true, "storage": false}
        ],
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
            owner_references: vec![],
            spec: spec_v1_storage,
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
            owner_references: vec![],
            spec: json!({}),
            validator: None,
        })
        .await
        .unwrap();

    // Kubernetes-style storage semantics: rows are written at the storage
    // version. After the RD promotes v2 to storage, the existing v1 row still
    // counts as an instance in a declared version and must block RD deletion.
    let spec_v2_storage = json!({
        "group": "example.dev",
        "kind": "Widget",
        "plural": "widgets",
        "versions": [
            {"name": "v1", "served": true, "storage": false},
            {"name": "v2", "served": true, "storage": true}
        ],
        "allowedStatusControllerIds": []
    });
    let definition = store
        .update_resource_definition(
            definition.uid,
            UpdateResourceParams {
                api_version: None,
                revision: definition.revision,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                owner_references: vec![],
                spec: spec_v2_storage,
                validator: None,
            },
        )
        .await
        .unwrap();

    let err = store.delete(definition.uid).await.unwrap_err();
    assert!(matches!(err, StoreError::Validation(_)));

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
            owner_references: vec![],
            spec,
            validator: None,
        })
        .await
        .unwrap_err();

    assert!(matches!(err, StoreError::Validation(_)));

    Ok(())
}

#[sqlx::test]
async fn register_rejects_resource_definition_as_a_custom_kind(
    pool: sqlx::PgPool,
) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    let error = store
        .register_resource_definition(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: RESOURCE_DEFINITION_KIND.to_string(),
            name: "customdefinitions.other.example".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            owner_references: vec![],
            spec: json!({
                "group": "other.example",
                "kind": "ResourceDefinition",
                "plural": "customdefinitions",
                "versions": [{"name": "v1", "served": true, "storage": true}],
                "allowedStatusControllerIds": []
            }),
            validator: None,
        })
        .await
        .unwrap_err();

    assert!(matches!(error, StoreError::Validation(message) if
        message.contains("resource-definition system")));

    Ok(())
}

#[sqlx::test]
async fn update_allows_an_unchanged_legacy_reserved_definition(
    pool: sqlx::PgPool,
) -> sqlx::Result<()> {
    // Model a definition created before `rise.dev/User` and `users` were reserved. Upgrading must
    // not freeze unrelated schema updates while identity activation remains deferred.
    let uid: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO resource_store.resources
            (api_version, kind, name, discriminator, spec)
        VALUES
            ('rise.dev/v1alpha1', 'ResourceDefinition', 'users.rise.dev', 'legacy00', $1)
        RETURNING uid
        "#,
    )
    .bind(json!({
        "group": "rise.dev",
        "kind": "User",
        "plural": "users",
        "versions": [{"name": "v1", "served": true, "storage": true}],
        "allowedStatusControllerIds": []
    }))
    .fetch_one(&pool)
    .await?;

    let store = PgResourceStore::new(pool);
    let updated = store
        .update_resource_definition(
            uid,
            UpdateResourceParams {
                api_version: None,
                revision: 1,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                owner_references: vec![],
                spec: json!({
                    "group": "rise.dev",
                    "kind": "User",
                    "plural": "users",
                    "versions": [{"name": "v1", "served": true, "storage": true}],
                    "allowedStatusControllerIds": ["schema-controller"]
                }),
                validator: None,
            },
        )
        .await
        .unwrap();

    assert_eq!(updated.revision, 2);
    assert_eq!(updated.spec["kind"], "User");

    Ok(())
}

#[sqlx::test]
async fn register_resource_definition_rejects_zero_served_versions(
    pool: sqlx::PgPool,
) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    // A storage version that is not served, with no other served version,
    // leaves the collection unaddressable via any URL.
    let spec = json!({
        "group": "example.dev",
        "kind": "Widget",
        "plural": "widgets",
        "versions": [{"name": "v1", "served": false, "storage": true}],
        "allowedStatusControllerIds": []
    });

    let err = store
        .register_resource_definition(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: RESOURCE_DEFINITION_KIND.to_string(),
            name: "widgets.example.dev".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            owner_references: vec![],
            spec,
            validator: None,
        })
        .await
        .unwrap_err();

    assert!(
        matches!(err, StoreError::Validation(_)),
        "expected Validation error for zero served versions, got {err:?}"
    );

    Ok(())
}

#[sqlx::test]
async fn register_resource_definition_root_and_parent_validation(
    pool: sqlx::PgPool,
) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    // No `parent` declared — registers as a root-scoped collection.
    let root = json!({
        "group": "example.dev",
        "kind": "Widget",
        "plural": "widgets",
        "versions": [{"name": "v1", "served": true, "storage": true}],
        "allowedStatusControllerIds": []
    });
    store
        .register_resource_definition(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: RESOURCE_DEFINITION_KIND.to_string(),
            name: "widgets.example.dev".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            owner_references: vec![],
            spec: root,
            validator: None,
        })
        .await
        .unwrap();

    // A malformed `parent` (apiVersion not '<group>/<version>') is rejected.
    let bad_parent = json!({
        "group": "example.dev",
        "kind": "Gadget",
        "plural": "gadgets",
        "parent": {"apiVersion": "no-slash", "kind": ORGANIZATION_KIND},
        "versions": [{"name": "v1", "served": true, "storage": true}],
        "allowedStatusControllerIds": []
    });
    let err = store
        .register_resource_definition(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: RESOURCE_DEFINITION_KIND.to_string(),
            name: "gadgets.example.dev".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            owner_references: vec![],
            spec: bad_parent,
            validator: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Validation(_)));

    Ok(())
}

#[sqlx::test]
async fn register_resource_definition_rejects_parent_cycles(
    pool: sqlx::PgPool,
) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    // Build RD params for `kind`/`plural` (group `cycle.dev`, version `v1`),
    // optionally declaring a parent kind in the same group.
    let rd = |kind: &str, plural: &str, parent_kind: Option<&str>| {
        let mut spec = json!({
            "group": "cycle.dev",
            "kind": kind,
            "plural": plural,
            "versions": [{"name": "v1", "served": true, "storage": true}],
            "allowedStatusControllerIds": []
        });
        if let Some(pk) = parent_kind {
            spec["parent"] = json!({"apiVersion": "cycle.dev/v1", "kind": pk});
        }
        CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: RESOURCE_DEFINITION_KIND.to_string(),
            name: format!("{plural}.cycle.dev"),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            owner_references: vec![],
            spec,
            validator: None,
        }
    };

    // A ResourceDefinition that declares itself as its own parent is rejected.
    let err = store
        .register_resource_definition(rd("Selfish", "selfishes", Some("Selfish")))
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Validation(_)),
        "self-parent must be rejected: {err:?}"
    );

    // `Alpha` → `Beta` with `Beta` not yet registered: a dangling parent is allowed.
    store
        .register_resource_definition(rd("Alpha", "alphas", Some("Beta")))
        .await
        .unwrap();

    // Registering `Beta` → `Alpha` now closes the cycle `Alpha→Beta→Alpha`.
    let err = store
        .register_resource_definition(rd("Beta", "betas", Some("Alpha")))
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Validation(_)),
        "a mutual parent cycle must be rejected: {err:?}"
    );

    // A deep but acyclic chain registers cleanly — the walk must not false-positive.
    store
        .register_resource_definition(rd("Tier0", "tier0s", None))
        .await
        .unwrap();
    store
        .register_resource_definition(rd("Tier1", "tier1s", Some("Tier0")))
        .await
        .unwrap();
    store
        .register_resource_definition(rd("Tier2", "tier2s", Some("Tier1")))
        .await
        .unwrap();

    Ok(())
}

#[sqlx::test]
async fn register_resource_definition_rejects_duplicate_identity(
    pool: sqlx::PgPool,
) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    let rd = |group: &str, kind: &str, plural: &str| CreateResourceParams {
        api_version: API_VERSION_V1ALPHA1.to_string(),
        kind: RESOURCE_DEFINITION_KIND.to_string(),
        name: format!("{plural}.{group}"),
        parent_uid: None,
        annotations: BTreeMap::new(),
        finalizers: vec![],
        owner_references: vec![],
        spec: json!({
            "group": group,
            "kind": kind,
            "plural": plural,
            "versions": [{"name": "v1", "served": true, "storage": true}],
            "allowedStatusControllerIds": []
        }),
        validator: None,
    };

    store
        .register_resource_definition(rd("example.dev", "Widget", "widgets"))
        .await
        .unwrap();

    // The `resource_definitions` projection is a view; identity uniqueness is
    // enforced by partial unique indexes on `resources`. A duplicate plural
    // (different group) must surface as a Validation error, not a raw 500.
    let err = store
        .register_resource_definition(rd("other.dev", "Gizmo", "widgets"))
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Validation(_)),
        "duplicate plural must be a validation error: {err:?}"
    );

    // A duplicate (group, kind) with a different plural is likewise rejected.
    let err = store
        .register_resource_definition(rd("example.dev", "Widget", "widgetz"))
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Validation(_)),
        "duplicate group+kind must be a validation error: {err:?}"
    );

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
            owner_references: vec![],
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
async fn get_by_name_and_list_span_all_versions_of_a_group(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);
    register_example_widget_rd(&store).await;

    // A row stored at one version of a group...
    let created = store
        .create(CreateResourceParams {
            api_version: "example.dev/v1".to_string(),
            kind: "Widget".to_string(),
            name: "w1".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            owner_references: vec![],
            spec: json!({}),
            validator: None,
        })
        .await
        .unwrap();

    // ...is found by `get_by_name` / `list` keyed on *another* version of the
    // same group — retrieval spans all api versions of the group.
    let found = store
        .get_by_name("example.dev/v2", "Widget", "w1", None)
        .await
        .unwrap()
        .expect("widget found via a different version of the same group");
    assert_eq!(found.uid, created.uid);

    let listed = store.list("example.dev/v2", "Widget", None).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].uid, created.uid);

    // A different group does not match.
    assert!(store
        .get_by_name("other.dev/v1", "Widget", "w1", None)
        .await
        .unwrap()
        .is_none());

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
            owner_references: vec![],
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
    use rise_resource_store_postgres::OrganizationValidator;
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
            owner_references: vec![],
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
            owner_references: vec![],
            spec: json!({"displayName": "Org"}),
            validator: None,
        })
        .await
        .unwrap();

    // First delete: marks for deletion
    let outcome1 = store.delete(row.uid).await.unwrap();
    let first_ts = match outcome1 {
        DeleteOutcome::MarkedForDeletion(r) => r.deletion_timestamp,
        DeleteOutcome::Deleted => panic!("expected MarkedForDeletion"),
    };
    assert!(first_ts.is_some());

    // Second delete while finalizers are still present: re-marks (idempotent) and preserves
    // the original deletion_timestamp.
    let outcome2 = store.delete(row.uid).await.unwrap();
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
            owner_references: vec![],
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
                api_version: None,
                revision: row.revision,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                owner_references: vec![],
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
async fn added_version_serves_existing_instances_without_rewriting(
    pool: sqlx::PgPool,
) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    let spec_v1 = json!({
        "group": "example.dev",
        "kind": "Widget",
        "plural": "widgets",
        "versions": [{"name": "v1", "served": true, "storage": true}],
        "allowedStatusControllerIds": []
    });

    let rd = store
        .register_resource_definition(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: RESOURCE_DEFINITION_KIND.to_string(),
            name: "widgets.example.dev".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            owner_references: vec![],
            spec: spec_v1,
            validator: None,
        })
        .await
        .unwrap();

    let widget = store
        .create(CreateResourceParams {
            api_version: "example.dev/v1".to_string(),
            kind: "Widget".to_string(),
            name: "w1".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            owner_references: vec![],
            spec: json!({}),
            validator: None,
        })
        .await
        .unwrap();

    let spec_v2 = json!({
        "group": "example.dev",
        "kind": "Widget",
        "plural": "widgets",
        "versions": [
            {"name": "v1", "served": true, "storage": false},
            {"name": "v2", "served": true, "storage": true}
        ],
        "allowedStatusControllerIds": []
    });

    store
        .update_resource_definition(
            rd.uid,
            UpdateResourceParams {
                api_version: None,
                revision: rd.revision,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                owner_references: vec![],
                spec: spec_v2,
                validator: None,
            },
        )
        .await
        .unwrap();

    let info = store
        .resolve_collection_version("example.dev", "v2", "widgets")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(info.api_version, "example.dev/v2");
    assert_eq!(info.storage_api_version, "example.dev/v2");

    let rows = store
        .list_versions(&info.served_api_versions, &info.kind, None)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].uid, widget.uid);
    assert_eq!(rows[0].api_version, "example.dev/v1");

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
            owner_references: vec![],
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
        "versions": [{"name": "v1", "served": true, "storage": true}],
        "allowedStatusControllerIds": []
    });

    let err = store
        .update_resource_definition(
            row.uid,
            UpdateResourceParams {
                api_version: None,
                revision: row.revision,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                owner_references: vec![],
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
async fn update_resource_definition_rejects_removing_version_with_instances(
    pool: sqlx::PgPool,
) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    let spec = json!({
        "group": "example.dev",
        "kind": "Widget",
        "plural": "widgets",
        "versions": [
            {"name": "v1", "served": true, "storage": true},
            {"name": "v2", "served": true, "storage": false}
        ],
        "allowedStatusControllerIds": []
    });

    let rd = store
        .register_resource_definition(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: RESOURCE_DEFINITION_KIND.to_string(),
            name: "widgets.example.dev".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            owner_references: vec![],
            spec,
            validator: None,
        })
        .await
        .unwrap();

    // Instances are created at the storage version (v1).
    store
        .create(CreateResourceParams {
            api_version: "example.dev/v1".to_string(),
            kind: "Widget".to_string(),
            name: "w1".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            owner_references: vec![],
            spec: json!({}),
            validator: None,
        })
        .await
        .unwrap();

    // Dropping v1 (where the instance lives) must be rejected, even though
    // promoting v2 to the storage version is itself valid.
    let drop_v1 = json!({
        "group": "example.dev",
        "kind": "Widget",
        "plural": "widgets",
        "versions": [{"name": "v2", "served": true, "storage": true}],
        "allowedStatusControllerIds": []
    });
    let err = store
        .update_resource_definition(
            rd.uid,
            UpdateResourceParams {
                api_version: None,
                revision: rd.revision,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                owner_references: vec![],
                spec: drop_v1,
                validator: None,
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Validation(_)),
        "expected Validation error for removing a version with instances, got {err:?}"
    );

    // Dropping v2, which has no instances, is allowed. The failed update above
    // rolled back, so the revision is unchanged.
    let drop_v2 = json!({
        "group": "example.dev",
        "kind": "Widget",
        "plural": "widgets",
        "versions": [{"name": "v1", "served": true, "storage": true}],
        "allowedStatusControllerIds": []
    });
    store
        .update_resource_definition(
            rd.uid,
            UpdateResourceParams {
                api_version: None,
                revision: rd.revision,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                owner_references: vec![],
                spec: drop_v2,
                validator: None,
            },
        )
        .await
        .expect("dropping a version with no instances should succeed");

    Ok(())
}

#[sqlx::test]
async fn update_resource_definition_rejects_parent_change(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    let spec = json!({
        "group": "example.dev",
        "kind": "Widget",
        "plural": "widgets",
        "parent": {"apiVersion": API_VERSION_V1ALPHA1, "kind": ORGANIZATION_KIND},
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
            owner_references: vec![],
            spec,
            validator: None,
        })
        .await
        .unwrap();

    let changed_parent_spec = json!({
        "group": "example.dev",
        "kind": "Widget",
        "plural": "widgets",
        "parent": {"apiVersion": API_VERSION_V1ALPHA1, "kind": "Project"},
        "versions": [{"name": "v1", "served": true, "storage": true}],
        "allowedStatusControllerIds": []
    });

    let err = store
        .update_resource_definition(
            row.uid,
            UpdateResourceParams {
                api_version: None,
                revision: row.revision,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                owner_references: vec![],
                spec: changed_parent_spec,
                validator: None,
            },
        )
        .await
        .unwrap_err();

    assert!(
        matches!(err, StoreError::Validation(_)),
        "expected Validation error for parent change, got {err:?}"
    );

    Ok(())
}

#[sqlx::test]
async fn register_resource_definition_rejects_invalid_non_storage_schema(
    pool: sqlx::PgPool,
) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    let spec = json!({
        "group": "example.dev",
        "kind": "Widget",
        "plural": "widgets",
        "versions": [
            {"name": "v1", "served": true, "storage": true},
            {"name": "v2", "served": true, "storage": false, "schema": {"type": "not-a-json-schema-type"}}
        ],
        "allowedStatusControllerIds": []
    });

    let err = store
        .register_resource_definition(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: RESOURCE_DEFINITION_KIND.to_string(),
            name: "widgets.example.dev".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            owner_references: vec![],
            spec,
            validator: None,
        })
        .await
        .unwrap_err();

    assert!(
        matches!(err, StoreError::Validation(_)),
        "expected invalid schema validation error, got {err:?}"
    );

    Ok(())
}

#[sqlx::test]
async fn update_resource_definition_invalidates_schema_cache(
    pool: sqlx::PgPool,
) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    let spec_v1 = json!({
        "group": "example.dev",
        "kind": "Widget",
        "plural": "widgets",
        "versions": [{
            "name": "v1",
            "served": true,
            "storage": true,
            "schema": {
                "type": "object",
                "required": ["color"],
                "properties": {"color": {"type": "string"}}
            }
        }],
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
            owner_references: vec![],
            spec: spec_v1,
            validator: None,
        })
        .await
        .unwrap();

    let cached_info = store
        .resolve_collection_version("example.dev", "v1", "widgets")
        .await
        .unwrap()
        .unwrap();
    cached_info
        .spec_validator
        .validate_spec(&json!({"color": "blue"}))
        .unwrap();

    let spec_v2 = json!({
        "group": "example.dev",
        "kind": "Widget",
        "plural": "widgets",
        "versions": [{
            "name": "v1",
            "served": true,
            "storage": true,
            "schema": {
                "type": "object",
                "required": ["size"],
                "properties": {"size": {"type": "number"}}
            }
        }],
        "allowedStatusControllerIds": []
    });

    store
        .update_resource_definition(
            row.uid,
            UpdateResourceParams {
                api_version: None,
                revision: row.revision,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                owner_references: vec![],
                spec: spec_v2,
                validator: None,
            },
        )
        .await
        .unwrap();

    let refreshed_info = store
        .resolve_collection_version("example.dev", "v1", "widgets")
        .await
        .unwrap()
        .unwrap();

    let err = refreshed_info
        .spec_validator
        .validate_spec(&json!({"color": "blue"}))
        .unwrap_err();
    assert!(
        err.to_string().contains("spec validation failed"),
        "expected refreshed schema to reject old shape, got {err:?}"
    );
    refreshed_info
        .spec_validator
        .validate_spec(&json!({"size": 3}))
        .unwrap();

    Ok(())
}

#[sqlx::test]
async fn update_rejects_resource_definition(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);

    let spec = json!({
        "group": "example.dev",
        "kind": "Gadget",
        "plural": "gadgets",
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
            owner_references: vec![],
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
                api_version: None,
                revision: row.revision,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                owner_references: vec![],
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
// Cascade deletion
// ---------------------------------------------------------------------------------------------

async fn create_org(store: &PgResourceStore, name: &str) -> ResourceRow {
    store
        .create(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: ORGANIZATION_KIND.to_string(),
            name: name.to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            owner_references: vec![],
            spec: json!({"displayName": name}),
            validator: None,
        })
        .await
        .unwrap()
}

fn owner_reference(row: &ResourceRow) -> OwnerReference {
    OwnerReference::new(&row.api_version, &row.kind, &row.name, row.uid).unwrap()
}

async fn create_owned_org(
    store: &PgResourceStore,
    name: &str,
    owner_references: Vec<OwnerReference>,
    finalizers: Vec<String>,
) -> ResourceRow {
    store
        .create(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: ORGANIZATION_KIND.to_string(),
            name: name.to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers,
            owner_references,
            spec: json!({"displayName": name}),
            validator: None,
        })
        .await
        .unwrap()
}

#[sqlx::test]
async fn owner_references_are_persisted_and_replaceable(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);
    let owner = create_org(&store, "owner").await;
    let dependent =
        create_owned_org(&store, "dependent", vec![owner_reference(&owner)], vec![]).await;

    let fetched = store.get(dependent.uid).await.unwrap().unwrap();
    assert_eq!(fetched.owner_references, vec![owner_reference(&owner)]);
    let resource: rise_resource_api::Resource = fetched.to_resource().unwrap();
    assert_eq!(
        resource.metadata.owner_references,
        vec![owner_reference(&owner)]
    );

    let updated = store
        .update(
            dependent.uid,
            UpdateResourceParams {
                api_version: None,
                revision: fetched.revision,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                owner_references: vec![],
                spec: fetched.spec,
                validator: None,
            },
        )
        .await
        .unwrap();
    assert!(updated.owner_references.is_empty());

    assert!(matches!(
        store.delete(owner.uid).await.unwrap(),
        DeleteOutcome::Deleted
    ));
    assert!(store.get(dependent.uid).await.unwrap().is_some());

    Ok(())
}

#[sqlx::test]
async fn owner_deletion_cascades_and_waits_for_dependent_finalizers(
    pool: sqlx::PgPool,
) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);
    let owner = create_org(&store, "owner").await;
    let cleanup_finalizer = "controller.example.com/cleanup".to_string();
    let dependent = create_owned_org(
        &store,
        "dependent",
        vec![owner_reference(&owner)],
        vec![cleanup_finalizer.clone()],
    )
    .await;

    let marked_owner = match store.delete(owner.uid).await.unwrap() {
        DeleteOutcome::MarkedForDeletion(row) => row,
        DeleteOutcome::Deleted => panic!("owner with a dependent must remain until cascade drains"),
    };
    assert!(marked_owner
        .finalizers
        .contains(&CASCADE_DELETION_FINALIZER.to_string()));
    assert!(store
        .get(dependent.uid)
        .await
        .unwrap()
        .unwrap()
        .deletion_timestamp
        .is_some());
    assert!(matches!(
        store.try_collect(owner.uid).await.unwrap(),
        DeleteOutcome::MarkedForDeletion(_)
    ));

    store
        .update_controller_finalizers(
            dependent.uid,
            "controller.example.com",
            &[],
            &[cleanup_finalizer],
        )
        .await
        .unwrap();
    assert!(matches!(
        store.try_collect(dependent.uid).await.unwrap(),
        DeleteOutcome::Deleted
    ));
    assert!(matches!(
        store.try_collect(owner.uid).await.unwrap(),
        DeleteOutcome::Deleted
    ));

    Ok(())
}

#[sqlx::test]
async fn owner_references_reject_stale_identity_and_duplicates(
    pool: sqlx::PgPool,
) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);
    let owner = create_org(&store, "owner").await;
    let wrong_name =
        OwnerReference::new(&owner.api_version, &owner.kind, "different-name", owner.uid).unwrap();

    let error = store
        .create(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: ORGANIZATION_KIND.to_string(),
            name: "dependent".to_string(),
            owner_references: vec![wrong_name],
            spec: json!({"displayName": "dependent"}),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::Validation(message) if message.contains("does not match")));

    let reference = owner_reference(&owner);
    let error = store
        .create(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: ORGANIZATION_KIND.to_string(),
            name: "duplicate-dependent".to_string(),
            owner_references: vec![reference.clone(), reference],
            spec: json!({"displayName": "duplicate-dependent"}),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(
        matches!(error, StoreError::Validation(message) if message.contains("duplicate owner reference"))
    );

    Ok(())
}

#[sqlx::test]
async fn owner_references_reject_lifecycle_cycles(pool: sqlx::PgPool) -> sqlx::Result<()> {
    let store = PgResourceStore::new(pool);
    let owner = create_org(&store, "owner").await;
    let dependent =
        create_owned_org(&store, "dependent", vec![owner_reference(&owner)], vec![]).await;

    let error = store
        .update(
            owner.uid,
            UpdateResourceParams {
                api_version: None,
                revision: owner.revision,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                owner_references: vec![owner_reference(&dependent)],
                spec: owner.spec.clone(),
                validator: None,
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, StoreError::Validation(message) if message.contains("lifecycle cycle"))
    );

    let structural_child =
        create_child(&store, owner.uid, "Widget", "structural-child", vec![]).await;
    let current_owner = store.get(owner.uid).await.unwrap().unwrap();
    let error = store
        .update(
            owner.uid,
            UpdateResourceParams {
                api_version: None,
                revision: current_owner.revision,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                owner_references: vec![owner_reference(&structural_child)],
                spec: current_owner.spec,
                validator: None,
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, StoreError::Validation(message) if message.contains("lifecycle cycle"))
    );

    Ok(())
}

/// Register the `example.dev/Widget` ResourceDefinition with versions `v1` (storage) and `v2`
/// (served). Required before creating any non-builtin `example.dev/Widget` resource instances,
/// since `create` acquires a FOR SHARE lock on the matching RD to prevent concurrent deletions
/// from orphaning newly-created instances.
///
/// Idempotent: silently ignores a `NameConflict` error so it can be called multiple times in
/// the same test (e.g. once per `create_child` call).
async fn register_example_widget_rd(store: &PgResourceStore) {
    let result = store
        .register_resource_definition(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: RESOURCE_DEFINITION_KIND.to_string(),
            name: "widgets.example.dev".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            owner_references: vec![],
            spec: json!({
                "group": "example.dev",
                "kind": "Widget",
                "plural": "widgets",
                "versions": [
                    {"name": "v1", "served": true, "storage": true},
                    {"name": "v2", "served": true, "storage": false}
                ],
                "allowedStatusControllerIds": []
            }),
            validator: None,
        })
        .await;
    match result {
        Ok(_) | Err(StoreError::NameConflict) => {}
        Err(e) => panic!("register_example_widget_rd failed: {e:?}"),
    }
}

async fn create_child(
    store: &PgResourceStore,
    parent: Uuid,
    kind: &str,
    name: &str,
    finalizers: Vec<String>,
) -> ResourceRow {
    // Ensure the ResourceDefinition exists so that the FOR SHARE lock in `create` can find it.
    register_example_widget_rd(store).await;
    store
        .create(CreateResourceParams {
            api_version: "example.dev/v1".to_string(),
            kind: kind.to_string(),
            name: name.to_string(),
            parent_uid: Some(parent),
            annotations: BTreeMap::new(),
            finalizers,
            owner_references: vec![],
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

    let outcome = store.delete(parent.uid).await.unwrap();
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

    let first = match store.delete(parent.uid).await.unwrap() {
        DeleteOutcome::MarkedForDeletion(r) => r.deletion_timestamp,
        _ => panic!("expected MarkedForDeletion"),
    };
    let second = match store.delete(parent.uid).await.unwrap() {
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
    store.delete(parent.uid).await.unwrap();

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
    store.delete(dying.uid).await.unwrap();

    let pending = store.list_pending_collection(100).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].uid, dying.uid);
    assert!(pending.iter().all(|r| r.uid != alive.uid));

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
                api_versions: vec![API_VERSION_V1ALPHA1.to_string()],
                kind: ORGANIZATION_KIND.to_string(),
                name: "acme".to_string(),
            },
            PathSegment::Name {
                api_versions: vec!["example.dev/v1".to_string()],
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
                api_versions: vec![API_VERSION_V1ALPHA1.to_string()],
                kind: ORGANIZATION_KIND.to_string(),
                uid: org.uid,
            },
            PathSegment::Name {
                api_versions: vec!["example.dev/v1".to_string()],
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
            api_versions: vec!["example.dev/v1".to_string()],
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
                api_versions: vec![API_VERSION_V1ALPHA1.to_string()],
                kind: ORGANIZATION_KIND.to_string(),
                uid: org_a.uid,
            },
            PathSegment::Uid {
                api_versions: vec!["example.dev/v1".to_string()],
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
    store.delete(widget.uid).await.unwrap();

    let chain = store
        .resolve_path(&[
            PathSegment::Name {
                api_versions: vec![API_VERSION_V1ALPHA1.to_string()],
                kind: ORGANIZATION_KIND.to_string(),
                name: "acme".to_string(),
            },
            PathSegment::Name {
                api_versions: vec!["example.dev/v1".to_string()],
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
