use std::collections::BTreeMap;
use std::sync::Arc;
use uuid::Uuid;

use rise_resource_api::ResourceScope;

use crate::error::StoreError;
use crate::models::ResourceRow;
use crate::validation::SpecValidator;

/// Reserved finalizer prefix for store-managed finalizers. Controllers cannot add or remove
/// finalizers in this namespace via `update_controller_finalizers`.
pub const SYSTEM_FINALIZER_PREFIX: &str = "system.rise.dev/";

/// Finalizer added to a parent resource when it is deleted with `PropagationPolicy::Cascade`
/// and still has children. Removed by the store once the subtree has drained.
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
    pub revision: i64,
    pub annotations: BTreeMap<String, String>,
    pub finalizers: Vec<String>,
    pub spec: serde_json::Value,
    pub validator: Option<Arc<dyn SpecValidator>>,
}

/// Controls how child resources are handled when a parent is deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PropagationPolicy {
    /// Default. Stamp `deletion_timestamp` on the parent and its immediate children, and attach a
    /// `system.rise.dev/cascade-deletion` finalizer to the parent. A background GC sweep (via
    /// `try_collect`) drives the rest of the subtree to completion bottom-up.
    #[default]
    Cascade,

    /// Detach all immediate children (`parent_uid = NULL`) and then delete the parent normally.
    /// Children continue to exist as root-level resources. This is an admin/break-glass operation;
    /// it should be gated at the API layer.
    Orphan,
}

/// One segment of a resource path. Always carries the kind so the response shape and ancestor
/// integrity can be verified without a round-trip.
#[derive(Debug, Clone)]
pub enum PathSegment {
    /// Address by name within the parent scope (root if first segment).
    Name {
        api_version: String,
        kind: String,
        name: String,
    },
    /// Address by UID. The kind is still required and checked against the stored row; mismatches
    /// surface as `StoreError::KindMismatch`.
    Uid {
        api_version: String,
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

    async fn update(
        &self,
        uid: Uuid,
        params: UpdateResourceParams,
    ) -> Result<ResourceRow, StoreError>;

    /// Delete (or mark for deletion) a resource. `policy` controls child handling.
    ///
    /// - `Cascade`: stamps `deletion_timestamp` on the parent and its immediate children, and
    ///   attaches `system.rise.dev/cascade-deletion` to the parent if children exist. The actual
    ///   removal of the parent row happens when the subtree has drained and all finalizers are
    ///   gone, via `try_collect`.
    /// - `Orphan`: clears `parent_uid` on immediate children, then deletes the parent normally
    ///   (marked if it carries finalizers, hard-deleted otherwise). Admin gate is the caller's
    ///   responsibility.
    async fn delete(
        &self,
        uid: Uuid,
        policy: PropagationPolicy,
    ) -> Result<DeleteOutcome, StoreError>;

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

    /// List children whose parent is currently tombstoned (`deletion_timestamp IS NOT NULL`).
    /// Optionally scoped to a single parent. Useful for break-glass discovery of in-progress
    /// teardowns.
    async fn list_orphans(&self, parent_uid: Option<Uuid>) -> Result<Vec<ResourceRow>, StoreError>;

    /// Atomically move a resource while preserving the collection's declared scope. Rejects cycles
    /// and respects the partial unique indexes on name/discriminator at the destination scope.
    async fn reparent(
        &self,
        uid: Uuid,
        new_parent_uid: Option<Uuid>,
        scope: ResourceScope,
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
