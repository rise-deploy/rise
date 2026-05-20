use std::collections::BTreeMap;
use std::sync::Arc;
use uuid::Uuid;

use rise_resource_api::{ResourceParentRef, ResourceScope};

use crate::error::StoreError;
use crate::models::ResourceRow;
use crate::validation::SpecValidator;

/// Reserved finalizer prefix for store-managed finalizers. Controllers cannot add or remove
/// finalizers in this namespace via `update_controller_finalizers`.
pub const SYSTEM_FINALIZER_PREFIX: &str = "system.rise.dev/";

/// Finalizer added to a parent resource when it is deleted while it still has children.
/// Removed by the store once the subtree has drained.
pub const CASCADE_DELETION_FINALIZER: &str = "system.rise.dev/cascade-deletion";

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
    pub api_version: Option<String>,
    pub revision: i64,
    pub annotations: BTreeMap<String, String>,
    pub finalizers: Vec<String>,
    pub spec: serde_json::Value,
    pub validator: Option<Arc<dyn SpecValidator>>,
}

/// One segment of a resource path. Always carries the kind so the response shape and ancestor
/// integrity can be verified without a round-trip.
#[derive(Debug, Clone)]
pub enum PathSegment {
    /// Address by name within the parent scope (root if first segment).
    Name {
        api_versions: Vec<String>,
        kind: String,
        name: String,
    },
    /// Address by UID. The kind is still required and checked against the stored row; mismatches
    /// surface as `StoreError::KindMismatch`.
    Uid {
        api_versions: Vec<String>,
        kind: String,
        uid: Uuid,
    },
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
    pub storage_api_version: String,
    pub served_api_versions: Vec<String>,
    pub kind: String,
    pub scope: ResourceScope,
    pub parent: Option<ResourceParentRef>,
    pub spec_validator: Arc<dyn SpecValidator>,
    pub allowed_status_controller_ids: Vec<String>,
}

#[async_trait::async_trait]
pub trait ResourceStore: Send + Sync {
    async fn create(&self, params: CreateResourceParams) -> Result<ResourceRow, StoreError>;

    async fn get(&self, uid: Uuid) -> Result<Option<ResourceRow>, StoreError>;

    async fn get_by_name(
        &self,
        api_version: &str,
        kind: &str,
        name: &str,
        parent_uid: Option<Uuid>,
    ) -> Result<Option<ResourceRow>, StoreError>;

    async fn list(
        &self,
        api_version: &str,
        kind: &str,
        parent_uid: Option<Uuid>,
    ) -> Result<Vec<ResourceRow>, StoreError>;

    async fn list_versions(
        &self,
        api_versions: &[String],
        kind: &str,
        parent_uid: Option<Uuid>,
    ) -> Result<Vec<ResourceRow>, StoreError>;

    async fn update(
        &self,
        uid: Uuid,
        params: UpdateResourceParams,
    ) -> Result<ResourceRow, StoreError>;

    /// Delete (or mark for deletion) a resource, cascading to its subtree.
    ///
    /// Stamps `deletion_timestamp` on the resource and its immediate children, and attaches
    /// `system.rise.dev/cascade-deletion` to the resource if children exist. The row is removed
    /// once the subtree has drained and all finalizers are gone, via `try_collect`; a childless
    /// resource with no finalizers is hard-deleted immediately.
    async fn delete(&self, uid: Uuid) -> Result<DeleteOutcome, StoreError>;

    /// GC sweep entry point. Idempotent.
    ///
    /// - If the row is not tombstoned, returns `MarkedForDeletion` with the current row unchanged.
    ///   No mutations are performed. (The GC normally only invokes this on rows that came back
    ///   from `list_pending_collection`, so non-tombstoned rows are an unexpected race; returning
    ///   the row unchanged lets the caller log/observe and move on without raising an error.)
    /// - If tombstoned with children: stamps any still-unstamped children (continues the fan-out),
    ///   ensures `system.rise.dev/cascade-deletion` is set, returns `MarkedForDeletion`.
    /// - If tombstoned without children: removes `system.rise.dev/cascade-deletion` if present;
    ///   if no other finalizers remain, hard-deletes and returns `Deleted`; otherwise returns
    ///   `MarkedForDeletion`.
    async fn try_collect(&self, uid: Uuid) -> Result<DeleteOutcome, StoreError>;

    /// List rows with a `deletion_timestamp`, oldest first. The caller (GC worker) iterates this
    /// and calls `try_collect` on each.
    async fn list_pending_collection(&self, limit: i64) -> Result<Vec<ResourceRow>, StoreError>;

    /// Resolve a path of segments to the full ancestor chain. The leaf is the last element.
    /// Tombstoned rows are returned (callers decide how to handle them).
    async fn resolve_path(&self, segments: &[PathSegment]) -> Result<Vec<ResourceRow>, StoreError>;

    /// Atomically move a resource. The destination is validated against the resource's own
    /// declared parent type: a root-scoped resource (built-in `Organization`/`ResourceDefinition`,
    /// or any `ResourceDefinition` with `scope = Root`) must move to root (`new_parent_uid =
    /// None`); a resource whose `ResourceDefinition` declares a `parent` must move under a row of
    /// the declared parent's API group + kind (the version is ignored, so an older served
    /// version is accepted). Rejects cycles and surfaces name/discriminator collisions at the
    /// destination scope as `NameConflict`.
    async fn reparent(
        &self,
        uid: Uuid,
        new_parent_uid: Option<Uuid>,
    ) -> Result<ResourceRow, StoreError>;

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

    async fn resolve_collection_version(
        &self,
        group: &str,
        version: &str,
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
