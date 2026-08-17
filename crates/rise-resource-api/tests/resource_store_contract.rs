use std::error::Error as _;
use std::sync::Arc;

use rise_resource_api::{
    CollectionInfo, CreateResourceParams, DeleteOutcome, PathSegment, ResourceRow, ResourceStore,
    StoreError, UpdateResourceParams, ValidationError,
};
use uuid::Uuid;

struct FakeStore;

#[async_trait::async_trait]
impl ResourceStore for FakeStore {
    async fn create(&self, _: CreateResourceParams) -> Result<ResourceRow, StoreError> {
        Err(StoreError::NotFound)
    }
    async fn get(&self, _: Uuid) -> Result<Option<ResourceRow>, StoreError> {
        Ok(None)
    }
    async fn get_by_name(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: Option<Uuid>,
    ) -> Result<Option<ResourceRow>, StoreError> {
        Ok(None)
    }
    async fn list(
        &self,
        _: &str,
        _: &str,
        _: Option<Uuid>,
    ) -> Result<Vec<ResourceRow>, StoreError> {
        Ok(vec![])
    }
    async fn list_versions(
        &self,
        _: &[String],
        _: &str,
        _: Option<Uuid>,
    ) -> Result<Vec<ResourceRow>, StoreError> {
        Ok(vec![])
    }
    async fn update(&self, _: Uuid, _: UpdateResourceParams) -> Result<ResourceRow, StoreError> {
        Err(StoreError::NotFound)
    }
    async fn delete(&self, _: Uuid) -> Result<DeleteOutcome, StoreError> {
        Err(StoreError::NotFound)
    }
    async fn try_collect(&self, _: Uuid) -> Result<DeleteOutcome, StoreError> {
        Err(StoreError::NotFound)
    }
    async fn list_pending_collection(&self, _: i64) -> Result<Vec<ResourceRow>, StoreError> {
        Ok(vec![])
    }

    async fn list_deletion_blockers(
        &self,
        _: Uuid,
    ) -> Result<rise_resource_api::DeletionBlockerReport, StoreError> {
        Err(StoreError::NotFound)
    }
    async fn resolve_path(&self, segments: &[PathSegment]) -> Result<Vec<ResourceRow>, StoreError> {
        if segments.is_empty() {
            return Err(StoreError::EmptyPath);
        }
        Ok(vec![])
    }
    async fn ancestors(&self, _: Uuid) -> Result<Vec<ResourceRow>, StoreError> {
        Ok(vec![])
    }
    async fn label_inheriting_descendants(
        &self,
        _: Uuid,
        _: &str,
    ) -> Result<Vec<ResourceRow>, StoreError> {
        Ok(vec![])
    }
    async fn update_controller_status(
        &self,
        _: Uuid,
        _: &str,
        _: serde_json::Value,
    ) -> Result<ResourceRow, StoreError> {
        Err(StoreError::NotFound)
    }
    async fn update_controller_finalizers(
        &self,
        _: Uuid,
        _: &str,
        _: &[String],
        _: &[String],
    ) -> Result<ResourceRow, StoreError> {
        Err(StoreError::NotFound)
    }
    async fn operator_update_status(
        &self,
        _: Uuid,
        _: &str,
        _: serde_json::Value,
    ) -> Result<ResourceRow, StoreError> {
        Err(StoreError::NotFound)
    }
    async fn operator_update_finalizers(
        &self,
        _: Uuid,
        _: &str,
        _: &[String],
        _: &[String],
    ) -> Result<ResourceRow, StoreError> {
        Err(StoreError::NotFound)
    }
    async fn resolve_collection(&self, _: &str) -> Result<Option<CollectionInfo>, StoreError> {
        Ok(None)
    }
    async fn resolve_collection_version(
        &self,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<Option<CollectionInfo>, StoreError> {
        Ok(None)
    }
    async fn resolve_collection_by_kind(
        &self,
        _: &str,
        _: &str,
    ) -> Result<Option<CollectionInfo>, StoreError> {
        Ok(None)
    }
    async fn register_resource_definition(
        &self,
        _: CreateResourceParams,
    ) -> Result<ResourceRow, StoreError> {
        Err(StoreError::NotFound)
    }
    async fn update_resource_definition(
        &self,
        _: Uuid,
        _: UpdateResourceParams,
    ) -> Result<ResourceRow, StoreError> {
        Err(StoreError::NotFound)
    }
}

#[tokio::test]
async fn separate_consumer_can_implement_and_type_erase_the_api_store_contract() {
    // This is a compile/object-safety/dependency-boundary proof: this Rust
    // integration-test consumer imports no rise-resource-store-postgres or SQLX. It is
    // intentionally not a behavioral conformance suite for store backends.
    let store: Arc<dyn ResourceStore> = Arc::new(FakeStore);
    assert!(store.get(Uuid::nil()).await.unwrap().is_none());
    assert!(matches!(
        store.resolve_path(&[]).await,
        Err(StoreError::EmptyPath)
    ));
}

#[test]
fn create_params_have_empty_optional_defaults() {
    let params = CreateResourceParams::default();
    assert!(params.api_version.is_empty());
    assert!(params.kind.is_empty());
    assert!(params.name.is_empty());
    assert!(params.parent_uid.is_none());
    assert!(params.annotations.is_empty());
    assert!(params.finalizers.is_empty());
    assert!(params.owner_references.is_empty());
    assert_eq!(params.spec, serde_json::json!({}));
    assert!(params.validator.is_none());
}

#[test]
fn validation_and_backend_errors_keep_expected_display_and_source() {
    let validation = StoreError::from(ValidationError::new("bad policy syntax"));
    assert_eq!(
        validation.to_string(),
        "validation error: bad policy syntax"
    );

    let backend = StoreError::backend(std::io::Error::other("connection closed"));
    assert_eq!(backend.to_string(), "resource store backend error");
    assert_eq!(backend.source().unwrap().to_string(), "connection closed");
}

fn row(
    metadata: serde_json::Value,
    spec: serde_json::Value,
    status: serde_json::Value,
) -> ResourceRow {
    let now = chrono::Utc::now();
    ResourceRow {
        uid: Uuid::nil(),
        api_version: "example.dev/v1".into(),
        kind: "Widget".into(),
        parent_uid: None,
        name: "widget-a".into(),
        discriminator: "abc123de".into(),
        labels: Default::default(),
        metadata,
        spec,
        status,
        revision: 1,
        finalizers: vec![],
        owner_references: vec![],
        deletion_timestamp: None,
        created_at: now,
        updated_at: now,
    }
}

#[test]
fn resource_row_conversion_fails_closed_on_every_malformed_json_component() {
    let bad_spec = row(
        serde_json::json!({}),
        serde_json::json!({"count": "not-a-number"}),
        serde_json::json!({}),
    );
    assert!(bad_spec
        .to_resource::<u64, serde_json::Value>()
        .unwrap_err()
        .to_string()
        .contains("invalid spec"));

    let bad_status = row(
        serde_json::json!({}),
        serde_json::json!(1),
        serde_json::json!({"ready": "not-a-bool"}),
    );
    assert!(bad_status
        .to_resource::<u64, bool>()
        .unwrap_err()
        .to_string()
        .contains("invalid status"));

    let bad_metadata = row(
        serde_json::json!({"owner": 42}),
        serde_json::json!(1),
        serde_json::json!(true),
    );
    assert!(bad_metadata
        .to_resource::<u64, bool>()
        .unwrap_err()
        .to_string()
        .contains("invalid metadata"));
}

#[test]
fn resource_row_has_one_version_normalization_path() {
    let mut alpha = row(
        serde_json::json!({}),
        serde_json::json!(1),
        serde_json::json!(true),
    );
    alpha.api_version = "rise.dev/v1alpha1".into();
    let mut stable = alpha.clone();
    stable.api_version = "rise.dev/v1".into();
    assert_eq!(
        alpha.resource_kind().unwrap(),
        stable.resource_kind().unwrap()
    );

    let mut other_group = stable;
    other_group.api_version = "other.example/v1".into();
    assert_ne!(
        alpha.resource_kind().unwrap(),
        other_group.resource_kind().unwrap()
    );
}
