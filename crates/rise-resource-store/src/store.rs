use std::collections::BTreeMap;
use std::sync::Arc;
use uuid::Uuid;

use rise_resource_api::ResourceScope;

use crate::error::StoreError;
use crate::models::ResourceRow;
use crate::validation::SpecValidator;

pub struct CreateResourceParams {
    pub api_version: String,
    pub kind: String,
    pub name: String,
    pub parent_uid: Option<Uuid>,
    pub annotations: BTreeMap<String, String>,
    pub finalizers: Vec<String>,
    pub spec: serde_json::Value,
    pub validator: Option<Arc<dyn SpecValidator>>,
}

pub struct UpdateResourceParams {
    pub revision: i64,
    pub annotations: BTreeMap<String, String>,
    pub finalizers: Vec<String>,
    pub spec: serde_json::Value,
    pub validator: Option<Arc<dyn SpecValidator>>,
}

pub enum DeleteOutcome {
    Deleted,
    MarkedForDeletion(Box<ResourceRow>),
}

impl std::fmt::Debug for DeleteOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Deleted => write!(f, "Deleted"),
            Self::MarkedForDeletion(_) => write!(f, "MarkedForDeletion"),
        }
    }
}

pub struct CollectionInfo {
    pub api_version: String,
    pub kind: String,
    pub scope: ResourceScope,
    pub spec_validator: Arc<dyn SpecValidator>,
    pub allowed_status_controller_ids: Vec<String>,
}

#[async_trait::async_trait]
pub trait ResourceStore: Send + Sync {
    async fn create(&self, params: CreateResourceParams) -> Result<ResourceRow, StoreError>;

    async fn get(&self, uid: Uuid) -> Result<Option<ResourceRow>, StoreError>;

    async fn get_by_name(
        &self,
        kind: &str,
        name: &str,
        parent_uid: Option<Uuid>,
    ) -> Result<Option<ResourceRow>, StoreError>;

    async fn list(
        &self,
        kind: &str,
        parent_uid: Option<Uuid>,
    ) -> Result<Vec<ResourceRow>, StoreError>;

    async fn update(
        &self,
        uid: Uuid,
        params: UpdateResourceParams,
    ) -> Result<ResourceRow, StoreError>;

    async fn delete(&self, uid: Uuid) -> Result<DeleteOutcome, StoreError>;

    async fn update_controller_status(
        &self,
        uid: Uuid,
        controller_id: &str,
        status_value: serde_json::Value,
    ) -> Result<ResourceRow, StoreError>;

    async fn update_controller_finalizers(
        &self,
        uid: Uuid,
        controller_id: &str,
        add: &[String],
        remove: &[String],
    ) -> Result<ResourceRow, StoreError>;

    async fn resolve_collection(
        &self,
        collection: &str,
    ) -> Result<Option<CollectionInfo>, StoreError>;

    async fn register_resource_definition(
        &self,
        params: CreateResourceParams,
    ) -> Result<ResourceRow, StoreError>;

    /// Update a ResourceDefinition resource. Keeps the `resource_definitions` projection table in
    /// sync with the `resources` row and enforces that identity fields (group, kind, plural, scope)
    /// are immutable once set.
    async fn update_resource_definition(
        &self,
        uid: Uuid,
        params: UpdateResourceParams,
    ) -> Result<ResourceRow, StoreError>;
}
