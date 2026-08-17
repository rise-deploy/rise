use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::{OwnerReference, ResourceParentRef, ResourceRow, ValidationError};

/// Reserved finalizer prefix for store-managed finalizers. Controllers cannot
/// add or remove finalizers in this namespace.
pub const SYSTEM_FINALIZER_PREFIX: &str = "system.rise.dev/";
/// Finalizer added to a resource while it still has structural children or
/// owner-reference dependents that explicitly block owner deletion. The store
/// removes it once those blocking lifecycle edges have drained.
pub const CASCADE_DELETION_FINALIZER: &str = "system.rise.dev/cascade-deletion";
/// Maximum `ResourceDefinition` parent-chain depth. Registration rejects a
/// longer or cyclic chain, and path resolution caps its walk at this bound.
pub const MAX_PARENT_CHAIN_DEPTH: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("resource not found")]
    NotFound,
    #[error("revision conflict: expected {expected}, found {found}")]
    RevisionConflict { expected: i64, found: i64 },
    #[error("a resource with this name already exists in this scope")]
    NameConflict,
    /// A uniqueness constraint other than the name was violated. Separate from
    /// [`Self::Validation`] because the request is well-formed and the caller's
    /// correct response is to reconcile with the existing resource, not to fix
    /// the payload.
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("could not generate a unique discriminator after maximum retries")]
    DiscriminatorExhausted,
    #[error("path segment kind mismatch: expected '{expected}', got '{got}'")]
    KindMismatch { expected: String, got: String },
    #[error("intermediate path segment not found")]
    ParentNotFound,
    #[error("unknown version: {0}")]
    UnknownVersion(String),
    #[error("reserved finalizer namespace: '{0}'")]
    ReservedFinalizer(String),
    #[error("path resolution requires at least one segment")]
    EmptyPath,
    #[error("validation error: {0}")]
    Validation(String),
    #[error("resource store backend error")]
    Backend {
        #[source]
        source: Box<dyn Error + Send + Sync + 'static>,
    },
}

impl StoreError {
    pub fn backend(error: impl Error + Send + Sync + 'static) -> Self {
        Self::Backend {
            source: Box::new(error),
        }
    }
}

impl From<ValidationError> for StoreError {
    fn from(error: ValidationError) -> Self {
        Self::Validation(error.to_string())
    }
}

/// Pure, local validation of one resource's spec or status representation.
///
/// Implementations must not perform registry lookups, existence checks, or
/// transactional policy validation. Those checks require a separate,
/// transaction-scoped seam introduced with the authorization engine; using
/// this trait for them would create time-of-check/time-of-use races.
pub trait SpecValidator: Send + Sync {
    fn validate_spec(&self, spec: &serde_json::Value) -> Result<(), ValidationError>;
    fn validate_status(&self, status: &serde_json::Value) -> Result<(), ValidationError>;
}

#[derive(Debug, Default)]
pub struct NoOpValidator;

impl SpecValidator for NoOpValidator {
    fn validate_spec(&self, _spec: &serde_json::Value) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_status(&self, _status: &serde_json::Value) -> Result<(), ValidationError> {
        Ok(())
    }
}

pub struct CreateResourceParams {
    /// Canonical storage API version for the row.
    ///
    /// Callers accepting a served client version must resolve the collection
    /// first and pass its `storage_api_version`. Served-version projection is
    /// an API-boundary concern; persisted rows use the storage version.
    pub api_version: String,
    pub kind: String,
    pub name: String,
    pub parent_uid: Option<Uuid>,
    pub labels: BTreeMap<String, String>,
    pub annotations: BTreeMap<String, String>,
    pub finalizers: Vec<String>,
    pub owner_references: Vec<OwnerReference>,
    pub spec: serde_json::Value,
    pub validator: Option<Arc<dyn SpecValidator>>,
}

impl Default for CreateResourceParams {
    fn default() -> Self {
        Self {
            api_version: String::new(),
            kind: String::new(),
            name: String::new(),
            parent_uid: None,
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
            finalizers: Vec::new(),
            owner_references: Vec::new(),
            spec: serde_json::Value::Object(serde_json::Map::new()),
            validator: None,
        }
    }
}

pub struct UpdateResourceParams {
    /// New canonical storage API version when migrating a row.
    ///
    /// HTTP callers translate a served request version to the collection's
    /// `storage_api_version` before invoking the store.
    pub api_version: Option<String>,
    pub revision: i64,
    pub labels: BTreeMap<String, String>,
    pub annotations: BTreeMap<String, String>,
    pub finalizers: Vec<String>,
    pub owner_references: Vec<OwnerReference>,
    pub spec: serde_json::Value,
    pub validator: Option<Arc<dyn SpecValidator>>,
}

/// One resource-path segment. Every segment carries the expected kind so
/// response shape and ancestor integrity can be checked without another lookup.
#[derive(Debug, Clone)]
pub enum PathSegment {
    /// Address by name within the current parent scope (root for the first segment).
    Name {
        api_versions: Vec<String>,
        kind: String,
        name: String,
    },
    /// Address by UID while also verifying the expected kind. Mismatches
    /// surface as [`StoreError::KindMismatch`].
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

/// Why one dependent currently prevents an owner from being collected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletionBlockerRelationship {
    /// Structural children are inherently blocking because their URL scope
    /// cannot outlive the parent.
    StructuralChild,
    /// A cross-tree owner reference with `blockOwnerDeletion: true`.
    OwnerReference,
}

/// One resource row that currently blocks collection of another resource.
#[derive(Debug, Clone)]
pub struct DeletionBlocker {
    pub relationship: DeletionBlockerRelationship,
    pub api_version: String,
    pub kind: String,
    pub name: String,
    pub uid: Uuid,
    pub deletion_timestamp: Option<DateTime<Utc>>,
    pub finalizers: Vec<String>,
}

/// One consistent diagnostic snapshot of a resource and its deletion blockers.
#[derive(Debug, Clone)]
pub struct DeletionBlockerReport {
    pub resource: ResourceRow,
    pub blockers: Vec<DeletionBlocker>,
}

impl fmt::Debug for DeleteOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Deleted => f.write_str("Deleted"),
            Self::MarkedForDeletion(_) => f.write_str("MarkedForDeletion"),
        }
    }
}

pub struct CollectionInfo {
    /// Requested/served version used for response projection. This is not the
    /// storage version.
    pub api_version: String,
    /// Canonical version persisted for new writes.
    pub storage_api_version: String,
    pub served_api_versions: Vec<String>,
    /// All declared versions, including versions that are not served. Row
    /// lookups use this set so a non-served stored version remains discoverable.
    pub declared_api_versions: Vec<String>,
    pub kind: String,
    pub parent: Option<ResourceParentRef>,
    pub spec_validator: Arc<dyn SpecValidator>,
    pub allowed_status_controller_ids: Vec<String>,
}

/// Storage-neutral persistence contract for the generic resource API.
#[async_trait::async_trait]
pub trait ResourceStore: Send + Sync {
    async fn create(&self, params: CreateResourceParams) -> Result<ResourceRow, StoreError>;
    async fn get(&self, uid: Uuid) -> Result<Option<ResourceRow>, StoreError>;
    /// Look up by name, matching the API group rather than the exact version so
    /// the row is found regardless of its declared stored version.
    async fn get_by_name(
        &self,
        api_version: &str,
        kind: &str,
        name: &str,
        parent_uid: Option<Uuid>,
    ) -> Result<Option<ResourceRow>, StoreError>;
    /// List a kind by API group across all declared stored versions.
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
    /// Replace mutable resource content using `params.revision` as an
    /// optimistic compare-and-swap guard. A stale revision returns
    /// [`StoreError::RevisionConflict`] without applying the update.
    async fn update(
        &self,
        uid: Uuid,
        params: UpdateResourceParams,
    ) -> Result<ResourceRow, StoreError>;
    /// Delete or tombstone a resource while cascading through its lifecycle DAG.
    ///
    /// The store stamps the resource, immediate structural children, and direct
    /// owner-reference dependents. It adds [`CASCADE_DELETION_FINALIZER`] while
    /// structural children or explicitly blocking owner references remain.
    /// Non-blocking owner-reference dependents are still tombstoned but do not
    /// retain the owner. A resource without blocking dependents or finalizers is
    /// deleted immediately.
    async fn delete(&self, uid: Uuid) -> Result<DeleteOutcome, StoreError>;
    /// Idempotent garbage-collection sweep for one resource.
    ///
    /// A live row is returned unchanged as `MarkedForDeletion`. A tombstoned
    /// row fans deletion out through every immediate lifecycle edge, retaining
    /// the cascade finalizer only while structural children or explicitly
    /// blocking owner-reference dependents remain. A tombstoned leaf loses the
    /// cascade finalizer and is hard-deleted only when no other finalizers remain.
    async fn try_collect(&self, uid: Uuid) -> Result<DeleteOutcome, StoreError>;
    /// List tombstoned rows oldest-first for the GC worker.
    async fn list_pending_collection(&self, limit: i64) -> Result<Vec<ResourceRow>, StoreError>;
    /// Return a consistent snapshot of the resource and the structural children
    /// or explicitly blocking owner-reference dependents that prevent its collection.
    async fn list_deletion_blockers(&self, uid: Uuid) -> Result<DeletionBlockerReport, StoreError>;
    /// Resolve a path to its root-first ancestor chain, including the leaf.
    /// Tombstoned rows are returned for the caller to interpret. Empty paths
    /// return [`StoreError::EmptyPath`].
    async fn resolve_path(&self, segments: &[PathSegment]) -> Result<Vec<ResourceRow>, StoreError>;
    /// Resolve a resource's structural ancestry by UID, root-first and
    /// including the resource itself. An unknown UID returns an empty chain.
    ///
    /// This is the primitive `effectiveLabels` resolution walks (ADR-0001
    /// §6.1): nearest-wins label inheritance is a pure function over this
    /// chain, so it belongs to the caller rather than to SQL. Like
    /// [`Self::resolve_path`], tombstoned rows are returned for the caller to
    /// interpret.
    async fn ancestors(&self, uid: Uuid) -> Result<Vec<ResourceRow>, StoreError>;
    /// Resolve the strict descendants of `uid` that inherit its effective value
    /// for one label key — the resource's *K-inheriting subtree*.
    ///
    /// A descendant that sets `label_key` itself supplies its own value under
    /// nearest-wins resolution (ADR-0001 §6.1), so it and everything beneath it
    /// are excluded: the walk stops at the first node that shadows the key.
    /// `uid` itself is never included.
    ///
    /// This is the surface ADR-0001 §6.6's write gate needs. Relabelling a
    /// resource changes the effective value on exactly this set, so the gate's
    /// before/after authority diff has to span it rather than the written
    /// resource alone. Rows come back parent-before-child and include
    /// tombstoned resources, matching [`Self::ancestors`]: a resource still
    /// draining is still addressable, and omitting it would under-report the
    /// delta.
    async fn label_inheriting_descendants(
        &self,
        uid: Uuid,
        label_key: &str,
    ) -> Result<Vec<ResourceRow>, StoreError>;
    /// Merge one allowed controller's value under `status.controllers`.
    async fn update_controller_status(
        &self,
        uid: Uuid,
        controller_id: &str,
        status_value: serde_json::Value,
    ) -> Result<ResourceRow, StoreError>;
    /// Atomically add/remove finalizers owned by an allowed controller while
    /// rejecting the reserved system namespace and other controllers' keys.
    async fn update_controller_finalizers(
        &self,
        uid: Uuid,
        controller_id: &str,
        add: &[String],
        remove: &[String],
    ) -> Result<ResourceRow, StoreError>;
    /// Force-update status as an operator, storing the value under
    /// `status.controllers.operator:<operator>`.
    async fn operator_update_status(
        &self,
        uid: Uuid,
        operator: &str,
        status_value: serde_json::Value,
    ) -> Result<ResourceRow, StoreError>;
    /// Force-update finalizers as an operator, bypassing controller ownership
    /// but still rejecting the reserved system namespace.
    async fn operator_update_finalizers(
        &self,
        uid: Uuid,
        operator: &str,
        add: &[String],
        remove: &[String],
    ) -> Result<ResourceRow, StoreError>;
    /// Resolve a served collection, including its canonical storage version,
    /// declared versions, parent shape, and pure local validator.
    async fn resolve_collection(
        &self,
        collection: &str,
    ) -> Result<Option<CollectionInfo>, StoreError>;
    /// Resolve a collection at an explicitly served group/version.
    async fn resolve_collection_version(
        &self,
        group: &str,
        version: &str,
        collection: &str,
    ) -> Result<Option<CollectionInfo>, StoreError>;
    /// Resolve by exact API group and kind for parent-chain walking.
    ///
    /// Resolution uses the storage version, falling back to a served version
    /// when storage is not served. It returns `None` when neither a built-in nor
    /// ResourceDefinition declares the pair.
    async fn resolve_collection_by_kind(
        &self,
        group: &str,
        kind: &str,
    ) -> Result<Option<CollectionInfo>, StoreError>;
    /// Atomically register a ResourceDefinition and its storage projection.
    async fn register_resource_definition(
        &self,
        params: CreateResourceParams,
    ) -> Result<ResourceRow, StoreError>;
    /// Update a ResourceDefinition while keeping its projection synchronized
    /// and enforcing immutable identity fields.
    async fn update_resource_definition(
        &self,
        uid: Uuid,
        params: UpdateResourceParams,
    ) -> Result<ResourceRow, StoreError>;
}
