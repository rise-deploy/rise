use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use uuid::Uuid;

use rise_resource_api::{
    validate_controller_id, validate_labels, validate_resource_name, CollectionInfo,
    CreateResourceParams, DeleteOutcome, DeletionBlocker, DeletionBlockerRelationship,
    DeletionBlockerReport, NoOpValidator, OwnerReference, PathSegment, ResourceDefinitionSpec,
    ResourceKind, ResourceRow, ResourceStore, SpecValidator, StoreError, UpdateResourceParams,
    API_VERSION_V1ALPHA1, CASCADE_DELETION_FINALIZER, MAX_PARENT_CHAIN_DEPTH, ORGANIZATION_KIND,
    RESOURCE_DEFINITION_KIND, SYSTEM_FINALIZER_PREFIX,
};
use sqlx::{PgPool, Row};

use crate::admission::BuiltInAdmission;
use crate::builtin::{BuiltInRegistration, BuiltInRegistry};
use crate::discriminator;
use crate::models::PgResourceRow;
use crate::validation::{
    validate_new_resource_definition_identity, JsonSchemaValidator, ResourceDefinitionValidator,
};

pub struct PgResourceStore {
    pool: PgPool,
    /// Cache of compiled JSON schema validators keyed by collection plural name.
    /// Populated on first resolve_collection call; invalidated on register/update.
    schema_cache: RwLock<HashMap<String, Arc<dyn SpecValidator>>>,
    /// Routing table for the typed built-in kinds. Built-ins have no
    /// `resource_definitions` row, so resolution for them goes through this
    /// registry rather than the database. See [`crate::builtin`].
    builtins: Arc<BuiltInRegistry>,
}

#[derive(Debug, Clone, Copy)]
enum CascadeRelationship {
    StructuralParent,
    OwnerReference { block_owner_deletion: bool },
}

#[derive(Debug)]
struct TombstonedDependent {
    relationship: CascadeRelationship,
    uid: Uuid,
    api_version: String,
    kind: String,
    name: String,
}

#[derive(Debug, sqlx::FromRow)]
struct TombstonedDependentRow {
    uid: Uuid,
    api_version: String,
    kind: String,
    name: String,
    block_owner_deletion: bool,
}

#[derive(Debug, sqlx::FromRow)]
struct PgDeletionBlocker {
    structural: bool,
    api_version: String,
    kind: String,
    name: String,
    uid: Uuid,
    deletion_timestamp: Option<chrono::DateTime<chrono::Utc>>,
    finalizers: Vec<String>,
}

/// Just the immutable routing identity of a row.
#[derive(sqlx::FromRow)]
struct PgResourceIdentity {
    api_version: String,
    kind: String,
}

impl PgResourceStore {
    /// Construct a store backed by `pool` with the canonical structural and
    /// identity built-in registry. The common case.
    pub fn new(pool: PgPool) -> Self {
        Self::with_builtin_registry(pool, Arc::new(BuiltInRegistry::defaults()))
    }

    /// Construct a store with an explicit built-in registry. Intended for
    /// tests that want a stripped-down or augmented set of built-ins, and for
    /// future phases that introduce additional built-ins. Production code should keep using
    /// [`Self::new`], which threads through [`BuiltInRegistry::defaults`].
    pub fn with_builtin_registry(pool: PgPool, builtins: Arc<BuiltInRegistry>) -> Self {
        Self {
            pool,
            schema_cache: RwLock::new(HashMap::new()),
            builtins,
        }
    }

    fn is_name_conflict(err: &sqlx::Error) -> bool {
        if let sqlx::Error::Database(db) = err {
            let c = db.constraint().unwrap_or("");
            return c == "resources_child_kind_name_unique"
                || c == "resources_root_kind_name_unique";
        }
        false
    }

    /// Map a database error to a `StoreError`, translating the failures that
    /// are the caller's fault before falling back to an opaque backend error.
    ///
    /// PostgreSQL stores neither `text` nor `jsonb` containing U+0000, but JSON
    /// permits it and `serde_json` carries it happily, so a well-formed request
    /// can reach the write and be rejected there. That is a bad request, not an
    /// internal error. Screening for it here rather than walking every payload
    /// before the write keeps the cost on the error path and covers every
    /// statement, including ones added later.
    fn db_error(err: sqlx::Error) -> StoreError {
        if let sqlx::Error::Database(db) = &err {
            // 22021 character_not_in_repertoire — a NUL byte in a text value.
            // 22P05 untranslatable_character — an escaped NUL in a jsonb value.
            if matches!(db.code().as_deref(), Some("22021") | Some("22P05")) {
                return StoreError::Validation(
                    "values must not contain U+0000, which PostgreSQL cannot store".into(),
                );
            }
        }
        StoreError::backend(err)
    }

    fn is_discriminator_conflict(err: &sqlx::Error) -> bool {
        if let sqlx::Error::Database(db) = err {
            let c = db.constraint().unwrap_or("");
            return c == "resources_child_discriminator_unique"
                || c == "resources_root_discriminator_unique";
        }
        false
    }

    /// Map a `ResourceDefinition` identity unique-index violation to a
    /// validation error. These partial indexes live on the `resources` table
    /// (the `resource_definitions` projection is a view), so a duplicate plural
    /// or `(group, kind)` surfaces on the `resources` INSERT.
    fn rd_uniqueness_conflict(err: &sqlx::Error) -> Option<StoreError> {
        let sqlx::Error::Database(db) = err else {
            return None;
        };
        match db.constraint().unwrap_or("") {
            "resource_definitions_plural_unique" => Some(StoreError::Validation(
                "a ResourceDefinition with this plural already exists".into(),
            )),
            "resource_definitions_group_kind_unique" => Some(StoreError::Validation(
                "a ResourceDefinition for this group and kind already exists".into(),
            )),
            _ => None,
        }
    }

    fn identity_uniqueness_conflict(err: &sqlx::Error) -> Option<StoreError> {
        let sqlx::Error::Database(db) = err else {
            return None;
        };
        (db.constraint() == Some("user_identities_issuer_subject_unique")).then(|| {
            StoreError::Conflict(
                "a live UserIdentity with this issuer and subject already exists".into(),
            )
        })
    }

    /// Resolve the exact immutable built-in write registration. A request for
    /// another version of a reserved group/kind fails closed instead of
    /// falling through to an external ResourceDefinition.
    fn builtin_for_write(
        &self,
        api_version: &str,
        kind: &str,
    ) -> Result<Option<&crate::builtin::BuiltInRegistration>, StoreError> {
        let Some((group, _)) = api_version.split_once('/') else {
            return Ok(None);
        };
        let Some(registration) = self.builtins.lookup_by_group_kind(group, kind) else {
            return Ok(None);
        };
        if self.builtins.lookup_exact(api_version, kind).is_none() {
            return Err(StoreError::Validation(format!(
                "built-in {group}/{kind} must use apiVersion '{}'",
                registration.api_version
            )));
        }
        Ok(Some(registration))
    }

    fn canonical_owner_references(
        references: &[OwnerReference],
    ) -> Result<Vec<OwnerReference>, StoreError> {
        let mut references = references.to_vec();
        references.sort_by_key(OwnerReference::uid);
        for pair in references.windows(2) {
            if pair[0].uid() == pair[1].uid() {
                return Err(StoreError::Validation(format!(
                    "duplicate owner reference UID '{}'",
                    pair[0].uid()
                )));
            }
        }
        Ok(references)
    }

    /// Subresource writes do not go through the create/update preflight, so
    /// they screen their own caller-supplied values. PostgreSQL stores neither
    /// `text` nor `jsonb` containing U+0000; without this the write surfaces as
    /// an opaque backend error instead of a rejected request.
    /// The writer identity becomes a `status.controllers` object key.
    /// These kinds still have deletion admission outside the generic store.
    /// Letting them become cross-tree dependents would allow owner-driven GC to
    /// bypass those guards.
    fn ensure_owner_references_supported(
        dependent_api_version: &str,
        dependent_kind: &str,
        references: &[OwnerReference],
    ) -> Result<(), StoreError> {
        let has_external_deletion_admission = dependent_kind == RESOURCE_DEFINITION_KIND
            || (dependent_api_version == API_VERSION_V1ALPHA1
                && dependent_kind == ORGANIZATION_KIND);
        if !references.is_empty() && has_external_deletion_admission {
            return Err(StoreError::Validation(format!(
                "owner references are not supported on {dependent_kind} resources"
            )));
        }
        Ok(())
    }

    async fn lock_owner_reference_graph(conn: &mut sqlx::PgConnection) -> Result<(), StoreError> {
        sqlx::query(
            "SELECT pg_advisory_xact_lock(hashtext('resource_owner_reference_cycle')::bigint)",
        )
        .execute(conn)
        .await
        .map_err(Self::db_error)?;
        Ok(())
    }

    async fn validate_owner_references(
        conn: &mut sqlx::PgConnection,
        dependent_uid: Uuid,
        references: &[OwnerReference],
        check_cycles: bool,
    ) -> Result<Vec<OwnerReference>, StoreError> {
        let references = Self::canonical_owner_references(references)?;
        if references.is_empty() {
            // Removing edges cannot create a cycle. Skipping the global graph
            // lock also keeps the overwhelmingly common unowned write path
            // fully concurrent.
            return Ok(references);
        }

        // The caller holds the owner-reference graph lock before any other
        // resource locks. That ordering closes races where concurrent,
        // individually acyclic writes would form a cycle together.

        let owner_uids: Vec<Uuid> = references.iter().map(OwnerReference::uid).collect();
        if !owner_uids.is_empty() {
            let owners = sqlx::query_as::<
                _,
                (
                    Uuid,
                    String,
                    String,
                    String,
                    Option<chrono::DateTime<chrono::Utc>>,
                ),
            >(
                r#"
                SELECT uid, api_version, kind, name, deletion_timestamp
                FROM resource_store.resources
                WHERE uid = ANY($1)
                ORDER BY uid
                FOR UPDATE
                "#,
            )
            .bind(&owner_uids)
            .fetch_all(&mut *conn)
            .await
            .map_err(Self::db_error)?;

            let owners_by_uid: HashMap<_, _> =
                owners.into_iter().map(|owner| (owner.0, owner)).collect();
            for reference in &references {
                let Some((_, api_version, kind, name, deletion_timestamp)) =
                    owners_by_uid.get(&reference.uid())
                else {
                    return Err(StoreError::Validation(format!(
                        "owner reference UID '{}' does not identify a live resource",
                        reference.uid()
                    )));
                };
                if deletion_timestamp.is_some() {
                    return Err(StoreError::Validation(format!(
                        "owner reference UID '{}' is pending deletion",
                        reference.uid()
                    )));
                }
                let stored_kind = ResourceKind::from_api_route(api_version, kind)?;
                if stored_kind != reference.resource_kind()? || name != reference.name() {
                    return Err(StoreError::Validation(format!(
                        "owner reference UID '{}' does not match {}/{} named '{}'",
                        reference.uid(),
                        reference.api_version(),
                        reference.kind(),
                        reference.name()
                    )));
                }
            }

            if check_cycles {
                let creates_cycle: bool = sqlx::query_scalar(
                    r#"
                WITH RECURSIVE reachable(uid) AS (
                    SELECT $1::uuid
                    UNION
                    SELECT dependent.uid
                    FROM reachable current
                    JOIN resource_store.resources dependent
                      ON dependent.parent_uid = current.uid
                      OR dependent.owner_references @> jsonb_build_array(
                          jsonb_build_object('uid', current.uid::text)
                      )
                )
                SELECT EXISTS(
                    SELECT 1 FROM reachable WHERE uid = ANY($2)
                )
                "#,
                )
                .bind(dependent_uid)
                .bind(&owner_uids)
                .fetch_one(&mut *conn)
                .await
                .map_err(Self::db_error)?;
                if creates_cycle {
                    return Err(StoreError::Validation(
                        "owner references would create a lifecycle cycle".into(),
                    ));
                }
            }
        }

        Ok(references)
    }

    /// Verify the complete built-in structural chain against immutable exact
    /// registrations. This makes placement authoritative for every built-in,
    /// including Organization and test-only augmented registrations.
    async fn validate_builtin_placement(
        &self,
        conn: &mut sqlx::PgConnection,
        registration: &BuiltInRegistration,
        mut parent_uid: Option<Uuid>,
    ) -> Result<(), StoreError> {
        let dependent_kind = registration.kind;
        let mut current = registration;
        loop {
            let Some(expected_parent) = &current.parent else {
                if parent_uid.is_some() {
                    return Err(StoreError::Validation(format!(
                        "{dependent_kind} has a malformed built-in parent chain; {} must be root-scoped",
                        current.kind
                    )));
                }
                return Ok(());
            };
            let Some(expected_uid) = parent_uid else {
                return Err(StoreError::Validation(format!(
                    "{dependent_kind} requires a {} parent",
                    expected_parent.kind
                )));
            };
            let parent = sqlx::query_as::<_, PgResourceRow>(
                "SELECT * FROM resource_store.resources WHERE uid = $1 FOR SHARE",
            )
            .bind(expected_uid)
            .fetch_optional(&mut *conn)
            .await
            .map_err(Self::db_error)?
            .ok_or_else(|| {
                StoreError::Validation(format!(
                    "{dependent_kind} parent must identify a live {} {}",
                    expected_parent.api_version, expected_parent.kind
                ))
            })?;
            if parent.api_version != expected_parent.api_version
                || parent.kind != expected_parent.kind
                || parent.deletion_timestamp.is_some()
            {
                return Err(StoreError::Validation(format!(
                    "{dependent_kind} parent must identify a live {} {}",
                    expected_parent.api_version, expected_parent.kind
                )));
            }
            current = self
                .builtins
                .lookup_exact(&expected_parent.api_version, &expected_parent.kind)
                .ok_or_else(|| {
                    StoreError::Validation(format!(
                        "{dependent_kind} has an unregistered built-in parent {} {}",
                        expected_parent.api_version, expected_parent.kind
                    ))
                })?;
            parent_uid = parent.parent_uid;
        }
    }

    /// Tombstone every immediate lifecycle dependent that has not already
    /// entered deletion. Structural children are updated first, so a resource
    /// that is both a structural child and an owner-reference dependent emits
    /// one event with the structural relationship.
    async fn tombstone_immediate_dependents(
        conn: &mut sqlx::PgConnection,
        owner_uid: Uuid,
    ) -> Result<Vec<TombstonedDependent>, StoreError> {
        let structural = sqlx::query_as::<_, TombstonedDependentRow>(
            r#"
            UPDATE resource_store.resources
            SET deletion_timestamp = NOW(),
                revision = revision + 1
            WHERE parent_uid = $1
              AND deletion_timestamp IS NULL
            RETURNING uid, api_version, kind, name,
                      true AS block_owner_deletion
            "#,
        )
        .bind(owner_uid)
        .fetch_all(&mut *conn)
        .await
        .map_err(Self::db_error)?;

        let referenced = sqlx::query_as::<_, TombstonedDependentRow>(
            r#"
            UPDATE resource_store.resources
            SET deletion_timestamp = NOW(),
                revision = revision + 1
            WHERE owner_references @> jsonb_build_array(
                    jsonb_build_object('uid', $1::text)
                )
              AND deletion_timestamp IS NULL
            RETURNING uid, api_version, kind, name,
                      owner_references @> jsonb_build_array(
                          jsonb_build_object(
                              'uid', $1::text,
                              'blockOwnerDeletion', true
                          )
                      ) AS block_owner_deletion
            "#,
        )
        .bind(owner_uid)
        .fetch_all(&mut *conn)
        .await
        .map_err(Self::db_error)?;

        let mut tombstoned = Vec::with_capacity(structural.len() + referenced.len());
        tombstoned.extend(structural.into_iter().map(|row| TombstonedDependent {
            relationship: CascadeRelationship::StructuralParent,
            uid: row.uid,
            api_version: row.api_version,
            kind: row.kind,
            name: row.name,
        }));
        tombstoned.extend(referenced.into_iter().map(|row| TombstonedDependent {
            relationship: CascadeRelationship::OwnerReference {
                block_owner_deletion: row.block_owner_deletion,
            },
            uid: row.uid,
            api_version: row.api_version,
            kind: row.kind,
            name: row.name,
        }));
        Ok(tombstoned)
    }

    /// Structural children always block collection. Cross-tree dependents only
    /// block when the matching owner reference opts into foreground behavior.
    async fn has_blocking_dependents(
        conn: &mut sqlx::PgConnection,
        owner_uid: Uuid,
    ) -> Result<bool, StoreError> {
        sqlx::query_scalar(
            r#"
            SELECT EXISTS(
                SELECT 1
                FROM resource_store.resources
                WHERE parent_uid = $1
            ) OR EXISTS(
                SELECT 1
                FROM resource_store.resources
                WHERE owner_references @> jsonb_build_array(
                    jsonb_build_object(
                        'uid', $1::text,
                        'blockOwnerDeletion', true
                    )
                )
            )
            "#,
        )
        .bind(owner_uid)
        .fetch_one(&mut *conn)
        .await
        .map_err(Self::db_error)
    }

    /// Best-effort emit one structured lifecycle log after the transaction that
    /// first tombstoned each dependent has committed.
    fn log_tombstoned_dependents(owner: &PgResourceRow, dependents: Vec<TombstonedDependent>) {
        for dependent in dependents {
            let (reason, relationship, block_owner_deletion) = match dependent.relationship {
                CascadeRelationship::StructuralParent => {
                    ("parent_deleted", "structuralChild", true)
                }
                CascadeRelationship::OwnerReference {
                    block_owner_deletion,
                } => ("owner_deleted", "ownerReference", block_owner_deletion),
            };
            tracing::info!(
                target: "rise::audit",
                actor = "resource-store",
                reason,
                relationship,
                owner_uid = %owner.uid,
                owner_api_version = %owner.api_version,
                owner_kind = %owner.kind,
                owner_name = %owner.name,
                dependent_uid = %dependent.uid,
                dependent_api_version = %dependent.api_version,
                dependent_kind = %dependent.kind,
                dependent_name = %dependent.name,
                block_owner_deletion,
                "resource.deletion_cascaded"
            );
        }
    }

    /// Project this store's built-in registry into a `CollectionInfo` for
    /// `collection`. Returns `None` when `collection` is not a registered
    /// built-in; the caller falls through to the `resource_definitions`
    /// projection table.
    fn builtin_collection_info(&self, collection: &str) -> Option<CollectionInfo> {
        self.builtins
            .lookup_collection(collection)
            .map(|reg| reg.collection_info())
    }

    /// Retry an INSERT into `resources` up to 10 times, generating a fresh discriminator on each
    /// discriminator collision. Returns the inserted row or a `StoreError`.
    ///
    /// Each attempt is wrapped in a SAVEPOINT so that a discriminator-conflict error does not abort
    /// the surrounding Postgres transaction — without this, the first conflict would put the
    /// connection into an aborted-transaction state, making all subsequent retry attempts fail with
    /// "current transaction is aborted" rather than a discriminator conflict.
    async fn insert_resource_row_with_retry(
        conn: &mut sqlx::PgConnection,
        uid: Uuid,
        params: &CreateResourceParams,
        metadata: serde_json::Value,
    ) -> Result<ResourceRow, StoreError> {
        let owner_references = Self::canonical_owner_references(&params.owner_references)?;
        for _ in 0..10 {
            let discriminator = discriminator::generate();

            sqlx::query("SAVEPOINT sp_discriminator_retry")
                .execute(&mut *conn)
                .await
                .map_err(Self::db_error)?;

            let result = sqlx::query_as::<_, PgResourceRow>(
                r#"
                INSERT INTO resource_store.resources
                    (uid, api_version, kind, parent_uid, name, discriminator, labels, metadata,
                     spec, finalizers, owner_references)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
                RETURNING *
                "#,
            )
            .bind(uid)
            .bind(&params.api_version)
            .bind(&params.kind)
            .bind(params.parent_uid)
            .bind(&params.name)
            .bind(&discriminator)
            .bind(sqlx::types::Json(&params.labels))
            .bind(metadata.clone())
            .bind(&params.spec)
            .bind(&params.finalizers)
            .bind(serde_json::to_value(&owner_references).unwrap_or_default())
            .fetch_one(&mut *conn)
            .await;

            match result {
                Ok(row) => {
                    sqlx::query("RELEASE SAVEPOINT sp_discriminator_retry")
                        .execute(&mut *conn)
                        .await
                        .map_err(Self::db_error)?;
                    return Ok(row.into());
                }
                Err(ref e) if Self::is_name_conflict(e) => {
                    sqlx::query("ROLLBACK TO SAVEPOINT sp_discriminator_retry")
                        .execute(&mut *conn)
                        .await
                        .map_err(Self::db_error)?;
                    return Err(StoreError::NameConflict);
                }
                Err(ref e) if Self::is_discriminator_conflict(e) => {
                    sqlx::query("ROLLBACK TO SAVEPOINT sp_discriminator_retry")
                        .execute(&mut *conn)
                        .await
                        .map_err(Self::db_error)?;
                    continue;
                }
                Err(e) => {
                    sqlx::query("ROLLBACK TO SAVEPOINT sp_discriminator_retry")
                        .execute(&mut *conn)
                        .await
                        .map_err(Self::db_error)?;
                    return Err(Self::rd_uniqueness_conflict(&e)
                        .or_else(|| Self::identity_uniqueness_conflict(&e))
                        .unwrap_or_else(|| Self::db_error(e)));
                }
            }
        }

        Err(StoreError::DiscriminatorExhausted)
    }

    async fn ensure_resource_definition_has_no_instances(
        &self,
        row: &PgResourceRow,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<(), StoreError> {
        let spec: ResourceDefinitionSpec =
            serde_json::from_value(row.spec.clone()).map_err(|e| {
                StoreError::Validation(format!("stored ResourceDefinition spec is invalid: {e}"))
            })?;
        let api_versions: Vec<String> = spec
            .versions
            .iter()
            .map(|version| format!("{}/{}", spec.group, version.name))
            .collect();

        // Lock the ResourceDefinition's `resources` row for the duration of this
        // transaction. Instance writes acquire a FOR SHARE lock on the same row
        // before inserting, so they cannot slip in after this count check and
        // before deletion commits.
        sqlx::query("SELECT uid FROM resource_store.resources WHERE uid = $1 FOR UPDATE")
            .bind(row.uid)
            .execute(&mut **tx)
            .await
            .map_err(Self::db_error)?;

        let instance_count: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(*)
            FROM resource_store.resources
            WHERE kind = $1
              AND api_version = ANY($2)
            "#,
        )
        .bind(&spec.kind)
        .bind(&api_versions)
        .fetch_one(&mut **tx)
        .await
        .map_err(Self::db_error)?;

        if instance_count > 0 {
            return Err(StoreError::Validation(format!(
                "cannot delete ResourceDefinition '{}' while {instance_count} instance(s) exist",
                row.name
            )));
        }

        Ok(())
    }

    async fn lock_matching_definition_for_write(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        api_version: &str,
        kind: &str,
        required: bool,
    ) -> Result<(), StoreError> {
        let Some((group, version)) = api_version.split_once('/') else {
            if required {
                return Err(StoreError::Validation(format!(
                    "api_version '{api_version}' is not in '<group>/<version>' form"
                )));
            }
            return Ok(());
        };

        // Lock the matching ResourceDefinition's `resources` row directly — the
        // `resource_definitions` projection is a view and cannot be locked.
        // The JSONB containment check uses `storage: true` because `api_version` here is
        // always the storage version (the HTTP layer already translated any served→storage
        // before calling the store). A storage version need not be served.
        let row = sqlx::query(
            r#"
            SELECT uid
            FROM resource_store.resources
            WHERE kind = $1
              AND spec->>'group' = $2
              AND spec->>'kind' = $3
              AND spec->'versions' @> $4::jsonb
              AND deletion_timestamp IS NULL
            FOR SHARE
            "#,
        )
        .bind(RESOURCE_DEFINITION_KIND)
        .bind(group)
        .bind(kind)
        .bind(serde_json::json!([{ "name": version, "storage": true }]))
        .fetch_optional(&mut **tx)
        .await
        .map_err(Self::db_error)?;

        if required && row.is_none() {
            return Err(StoreError::Validation(format!(
                "no ResourceDefinition declares '{api_version}' as storage version for kind '{kind}'"
            )));
        }

        Ok(())
    }

    fn api_version(group: &str, version: &str) -> String {
        format!("{group}/{version}")
    }

    /// The API group of an `api_version` string (`"group/version"` → `"group"`). Falls back to
    /// the whole string if there is no `/`, which keeps malformed rows comparable rather than
    /// silently matching everything.
    fn group_of(api_version: &str) -> &str {
        api_version.split_once('/').map_or(api_version, |(g, _)| g)
    }

    fn invalidate_schema_cache(&self, group: &str, plural: &str) {
        if let Ok(mut cache) = self.schema_cache.write() {
            cache.retain(|key, _| {
                // Cache keys are formatted as "{group}/{version}/{plural}" by
                // resolve_collection_version. Split into exactly three parts so that
                // a malformed key (e.g. a version string containing '/') doesn't cause
                // split_once/rsplit_once to extract the wrong component.
                let mut parts = key.splitn(3, '/');
                let key_group = parts.next().unwrap_or("");
                let _key_version = parts.next();
                let key_plural = parts.next().unwrap_or("");
                // If there are fewer than 3 parts the key is malformed; evict it
                // so it cannot accumulate indefinitely.
                if _key_version.is_none() {
                    return false;
                }
                key_group != group || key_plural != plural
            });
        }
    }

    fn validate_version_schemas(
        spec: &ResourceDefinitionSpec,
        context: &str,
    ) -> Result<(), StoreError> {
        for version in &spec.versions {
            if let Some(schema) = version.schema.clone() {
                JsonSchemaValidator::new(schema).map_err(|e| {
                    StoreError::Validation(format!(
                        "ResourceDefinition {context} version '{}' has an invalid JSON schema: {e}",
                        version.name
                    ))
                })?;
            }
        }
        Ok(())
    }

    /// Read only the `parent` ref of a registered `ResourceDefinition` using
    /// `conn`. Reading through the caller's transaction ensures the cycle check
    /// sees a consistent snapshot under the advisory lock rather than relying on
    /// the pool's read isolation.
    async fn fetch_rd_parent_ref(
        conn: &mut sqlx::PgConnection,
        group: &str,
        kind: &str,
    ) -> Result<Option<rise_resource_api::ResourceParentRef>, StoreError> {
        let row = sqlx::query(
            r#"
            SELECT r.spec
            FROM resource_store.resource_definitions rd
            JOIN resource_store.resources r ON r.uid = rd.uid
            WHERE rd.group_name = $1 AND rd.kind = $2
            "#,
        )
        .bind(group)
        .bind(kind)
        .fetch_optional(&mut *conn)
        .await
        .map_err(Self::db_error)?;

        let Some(row) = row else {
            return Ok(None);
        };

        let spec: ResourceDefinitionSpec =
            serde_json::from_value(row.try_get("spec").map_err(Self::db_error)?).map_err(|e| {
                StoreError::Validation(format!(
                    "invalid spec in ResourceDefinition '{group}/{kind}': {e}"
                ))
            })?;
        Ok(spec.parent)
    }

    /// Reject a `ResourceDefinition` whose declared `parent` chain forms a
    /// cycle. The chain is walked via `fetch_rd_parent_ref` using `conn` so the
    /// reads are part of the same transaction that holds the advisory lock,
    /// ensuring a consistent view.
    ///
    /// The caller must hold the `resource_definitions_parent_cycle` advisory lock
    /// (via `SELECT pg_advisory_xact_lock`) for the duration of the enclosing
    /// transaction so that two concurrent registrations cannot both pass the cycle
    /// check before either commits.
    async fn ensure_no_parent_cycle(
        &self,
        spec: &ResourceDefinitionSpec,
        conn: &mut sqlx::PgConnection,
    ) -> Result<(), StoreError> {
        let mut current = spec.parent.clone();
        let mut depth = 0usize;
        while let Some(parent) = current {
            depth += 1;
            if depth > MAX_PARENT_CHAIN_DEPTH {
                return Err(StoreError::Validation(format!(
                    "ResourceDefinition '{}/{}' parent chain exceeds the maximum \
                     depth of {MAX_PARENT_CHAIN_DEPTH}",
                    spec.group, spec.kind
                )));
            }
            let parent_group = Self::group_of(&parent.api_version);
            if parent_group == spec.group && parent.kind == spec.kind {
                return Err(StoreError::Validation(format!(
                    "ResourceDefinition '{}/{}' declares a parent chain that \
                     cycles back to itself",
                    spec.group, spec.kind
                )));
            }
            current = Self::fetch_rd_parent_ref(conn, parent_group, &parent.kind).await?;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl ResourceStore for PgResourceStore {
    async fn create(&self, mut params: CreateResourceParams) -> Result<ResourceRow, StoreError> {
        if params.kind == RESOURCE_DEFINITION_KIND {
            return Err(StoreError::Validation(
                "ResourceDefinitions must be created through register_resource_definition"
                    .to_string(),
            ));
        }

        validate_resource_name(&params.name).map_err(|e| StoreError::Validation(e.to_string()))?;
        validate_labels(&params.labels).map_err(|e| StoreError::Validation(e.to_string()))?;
        Self::ensure_owner_references_supported(
            &params.api_version,
            &params.kind,
            &params.owner_references,
        )?;

        // Pure validation and canonicalization happen before opening a
        // transaction. Contextual checks below still own all database reads
        // and locks and persist this single canonical value.
        let builtin = self.builtin_for_write(&params.api_version, &params.kind)?;
        if let Some(registration) = builtin {
            if let Some(admission) =
                BuiltInAdmission::for_kind(registration.api_version, registration.kind)
            {
                params.spec = admission.canonicalize(&params.spec)?;
            }
            registration.spec_validator.validate_spec(&params.spec)?;
        }
        if let Some(validator) = &params.validator {
            validator.validate_spec(&params.spec)?;
        }

        let mut tx = self.pool.begin().await.map_err(Self::db_error)?;

        // Allocate the dependent UID before insertion so owner validation
        // happens before the dependent row is created. A fresh UID cannot
        // already be reachable, so creates skip the recursive cycle walk. They
        // still take the graph lock before owner/parent rows to preserve the
        // global lock order shared with owner-reference updates.
        let uid = Uuid::new_v4();
        if !params.owner_references.is_empty() {
            Self::lock_owner_reference_graph(&mut tx).await?;
        }
        let owner_references =
            Self::validate_owner_references(&mut tx, uid, &params.owner_references, false).await?;
        params.owner_references = owner_references.clone();

        if let Some(registration) = builtin {
            self.validate_builtin_placement(&mut tx, registration, params.parent_uid)
                .await?;
            if let Some(admission) =
                BuiltInAdmission::for_kind(registration.api_version, registration.kind)
            {
                admission
                    .admit_create_context(&mut tx, &self.builtins, &mut params)
                    .await?;
            }
        }

        // Always require the lock (and therefore verify the RD exists) unless this is a
        // builtin kind. Without a lock, a concurrent RD hard-deletion could race with the
        // insert, and callers with validator=None would bypass the RD existence check.
        Self::lock_matching_definition_for_write(
            &mut tx,
            &params.api_version,
            &params.kind,
            builtin.is_none(),
        )
        .await?;
        let metadata = serde_json::to_value(&params.annotations).unwrap_or_default();
        let row = Self::insert_resource_row_with_retry(&mut tx, uid, &params, metadata).await?;
        tx.commit().await.map_err(Self::db_error)?;
        let mut row = row;
        row.owner_references = owner_references;
        Ok(row)
    }

    async fn get(&self, uid: Uuid) -> Result<Option<ResourceRow>, StoreError> {
        let row = sqlx::query_as::<_, PgResourceRow>(
            "SELECT * FROM resource_store.resources WHERE uid = $1",
        )
        .bind(uid)
        .fetch_optional(&self.pool)
        .await
        .map_err(Self::db_error)?;
        Ok(row.map(Into::into))
    }

    async fn get_by_name(
        &self,
        api_version: &str,
        kind: &str,
        name: &str,
        parent_uid: Option<Uuid>,
    ) -> Result<Option<ResourceRow>, StoreError> {
        // Match on the API *group* (the part before `/`), not the full
        // `api_version`: a resource's identity is unique per `(group, kind,
        // name)` within a parent, so a lookup resolves the row regardless of
        // which declared version it is stored at.
        let row =
            match parent_uid {
                None => sqlx::query_as::<_, PgResourceRow>(
                    "SELECT * FROM resource_store.resources WHERE split_part(api_version, '/', 1) = split_part($1, '/', 1) AND kind = $2 AND name = $3 AND parent_uid IS NULL",
                )
                .bind(api_version)
                .bind(kind)
                .bind(name)
                .fetch_optional(&self.pool)
                .await
                .map_err(Self::db_error)?,
                Some(pid) => {
                    sqlx::query_as::<_, PgResourceRow>(
                        "SELECT * FROM resource_store.resources WHERE split_part(api_version, '/', 1) = split_part($1, '/', 1) AND kind = $2 AND name = $3 AND parent_uid = $4",
                    )
                    .bind(api_version)
                    .bind(kind)
                    .bind(name)
                    .bind(pid)
                    .fetch_optional(&self.pool)
                    .await
                    .map_err(Self::db_error)?
                }
            };
        Ok(row.map(Into::into))
    }

    async fn list(
        &self,
        api_version: &str,
        kind: &str,
        parent_uid: Option<Uuid>,
    ) -> Result<Vec<ResourceRow>, StoreError> {
        // Match on the API *group*, not the full `api_version` — list spans
        // every declared version of the kind (see `get_by_name`).
        let rows =
            match parent_uid {
                None => sqlx::query_as::<_, PgResourceRow>(
                    "SELECT * FROM resource_store.resources WHERE split_part(api_version, '/', 1) = split_part($1, '/', 1) AND kind = $2 AND parent_uid IS NULL ORDER BY name",
                )
                .bind(api_version)
                .bind(kind)
                .fetch_all(&self.pool)
                .await
                .map_err(Self::db_error)?,
                Some(pid) => {
                    sqlx::query_as::<_, PgResourceRow>(
                        "SELECT * FROM resource_store.resources WHERE split_part(api_version, '/', 1) = split_part($1, '/', 1) AND kind = $2 AND parent_uid = $3 ORDER BY name",
                    )
                    .bind(api_version)
                    .bind(kind)
                    .bind(pid)
                    .fetch_all(&self.pool)
                    .await
                    .map_err(Self::db_error)?
                }
            };
        Ok(rows.into_iter().map(Into::into).collect())
    }

    async fn list_versions(
        &self,
        api_versions: &[String],
        kind: &str,
        parent_uid: Option<Uuid>,
    ) -> Result<Vec<ResourceRow>, StoreError> {
        let rows =
            match parent_uid {
                None => sqlx::query_as::<_, PgResourceRow>(
                    "SELECT * FROM resource_store.resources WHERE api_version = ANY($1) AND kind = $2 AND parent_uid IS NULL ORDER BY name",
                )
                .bind(api_versions)
                .bind(kind)
                .fetch_all(&self.pool)
                .await
                .map_err(Self::db_error)?,
                Some(pid) => {
                    sqlx::query_as::<_, PgResourceRow>(
                        "SELECT * FROM resource_store.resources WHERE api_version = ANY($1) AND kind = $2 AND parent_uid = $3 ORDER BY name",
                    )
                    .bind(api_versions)
                    .bind(kind)
                    .bind(pid)
                    .fetch_all(&self.pool)
                    .await
                    .map_err(Self::db_error)?
                }
            };
        Ok(rows.into_iter().map(Into::into).collect())
    }

    async fn update(
        &self,
        uid: Uuid,
        mut params: UpdateResourceParams,
    ) -> Result<ResourceRow, StoreError> {
        // Resolve immutable routing identity and run all pure validators before
        // opening the mutation transaction or taking advisory/row locks. Only
        // the routing identity is needed here; the transaction below reads the
        // full row and is authoritative for every admission decision.
        let preflight: PgResourceIdentity =
            sqlx::query_as("SELECT api_version, kind FROM resource_store.resources WHERE uid = $1")
                .bind(uid)
                .fetch_optional(&self.pool)
                .await
                .map_err(Self::db_error)?
                .ok_or(StoreError::NotFound)?;
        if preflight.kind == RESOURCE_DEFINITION_KIND {
            return Err(StoreError::Validation(
                "ResourceDefinitions must be updated through update_resource_definition".into(),
            ));
        }
        validate_labels(&params.labels).map_err(|e| StoreError::Validation(e.to_string()))?;
        let preflight_target_api_version = params
            .api_version
            .as_deref()
            .unwrap_or(&preflight.api_version);
        let preflight_current_builtin =
            self.builtin_for_write(&preflight.api_version, &preflight.kind)?;
        let preflight_target_builtin =
            self.builtin_for_write(preflight_target_api_version, &preflight.kind)?;
        if preflight_current_builtin
            .map(|registration| (registration.api_version, registration.kind))
            != preflight_target_builtin
                .map(|registration| (registration.api_version, registration.kind))
        {
            return Err(StoreError::Validation(
                "a resource cannot migrate into or out of a built-in API identity".into(),
            ));
        }
        if let Some(registration) = preflight_current_builtin {
            if let Some(admission) =
                BuiltInAdmission::for_kind(registration.api_version, registration.kind)
            {
                params.spec = admission.canonicalize(&params.spec)?;
            }
            registration.spec_validator.validate_spec(&params.spec)?;
        }
        if let Some(validator) = &params.validator {
            validator.validate_spec(&params.spec)?;
        }

        let mut tx = self.pool.begin().await.map_err(Self::db_error)?;
        if !params.owner_references.is_empty() {
            Self::lock_owner_reference_graph(&mut tx).await?;
        }

        // Read through this transaction before taking row locks so every
        // admission decision shares one transaction. Parent/owner locks are
        // acquired before the dependent lock to match deletion's ordering.
        let snapshot = sqlx::query_as::<_, PgResourceRow>(
            "SELECT * FROM resource_store.resources WHERE uid = $1",
        )
        .bind(uid)
        .fetch_optional(&mut *tx)
        .await
        .map_err(Self::db_error)?
        .ok_or(StoreError::NotFound)?;
        if snapshot.kind == RESOURCE_DEFINITION_KIND {
            return Err(StoreError::Validation(
                "ResourceDefinitions must be updated through update_resource_definition".into(),
            ));
        }
        Self::ensure_owner_references_supported(
            &snapshot.api_version,
            &snapshot.kind,
            &params.owner_references,
        )?;

        let target_api_version = params
            .api_version
            .as_deref()
            .unwrap_or(&snapshot.api_version)
            .to_owned();
        let current_builtin = self.builtin_for_write(&snapshot.api_version, &snapshot.kind)?;
        let target_builtin = self.builtin_for_write(&target_api_version, &snapshot.kind)?;
        if current_builtin.map(|r| (r.api_version, r.kind))
            != target_builtin.map(|r| (r.api_version, r.kind))
        {
            return Err(StoreError::Validation(
                "a resource cannot migrate into or out of a built-in API identity".into(),
            ));
        }

        // Owner rows take UPDATE locks. This precedes GroupMembership's live
        // User SHARE lookup, avoiding lock-upgrade deadlocks for concurrent
        // owned membership creates/updates.
        let owner_references =
            Self::validate_owner_references(&mut tx, uid, &params.owner_references, true).await?;
        params.owner_references = owner_references.clone();

        if let Some(registration) = current_builtin {
            self.validate_builtin_placement(&mut tx, registration, snapshot.parent_uid)
                .await?;
            if let Some(admission) =
                BuiltInAdmission::for_kind(registration.api_version, registration.kind)
            {
                admission
                    .admit_update_before_dependent(&mut tx, &self.builtins, &snapshot, &mut params)
                    .await?;
            }
        }

        // Always acquire the FOR SHARE lock on the matching ResourceDefinition for
        // non-builtin external kinds, regardless of whether a validator was provided.
        // Without the lock, a concurrent hard-delete of the ResourceDefinition can
        // pass its instance-count check and commit while this update is in flight,
        // leaving the resource instance pointing at a definition that no longer exists.
        if target_builtin.is_none() {
            Self::lock_matching_definition_for_write(
                &mut tx,
                &target_api_version,
                &snapshot.kind,
                // Require the RD row to exist (and declare target_api_version as its
                // storage version) only when the caller is explicitly migrating the
                // row to a new api_version. A migration target must be a declared
                // storage version, so required=true catches non-storage targets.
                //
                // When not migrating (api_version = None), the instance may be at a
                // formerly-storage version whose RD was since deleted — still lock it
                // when present to block racing deletions, but don't error if the RD
                // is gone.
                params.api_version.is_some(),
            )
            .await?;
        }

        // The dependent is deliberately the final row lock in the admission
        // sequence: graph -> parents/owners -> current dependent.
        let current = sqlx::query_as::<_, PgResourceRow>(
            "SELECT * FROM resource_store.resources WHERE uid = $1 FOR UPDATE",
        )
        .bind(uid)
        .fetch_optional(&mut *tx)
        .await
        .map_err(Self::db_error)?
        .ok_or(StoreError::NotFound)?;
        if current.revision != params.revision {
            return Err(StoreError::RevisionConflict {
                expected: params.revision,
                found: current.revision,
            });
        }
        if current.api_version != snapshot.api_version
            || current.kind != snapshot.kind
            || current.parent_uid != snapshot.parent_uid
            || current.name != snapshot.name
        {
            return Err(StoreError::Validation(
                "resource identity changed during admission".into(),
            ));
        }

        // Re-evaluate old/new immutable-field checks against the locked row;
        // another writer may have committed between the snapshot and this lock.
        if let Some(registration) = current_builtin {
            if let Some(admission) =
                BuiltInAdmission::for_kind(registration.api_version, registration.kind)
            {
                admission.validate_update_immutable(&current, &params)?;
            }
        }
        let metadata = serde_json::to_value(&params.annotations).unwrap_or_default();

        let updated = sqlx::query_as::<_, PgResourceRow>(
            r#"
            UPDATE resource_store.resources
            SET api_version = COALESCE($1, api_version),
                metadata   = $2,
                spec       = $3,
                finalizers = $4,
                owner_references = $7,
                labels     = $8,
                revision   = revision + 1,
                updated_at = NOW()
            WHERE uid = $5 AND revision = $6
            RETURNING *
            "#,
        )
        .bind(params.api_version.as_deref())
        .bind(metadata)
        .bind(&params.spec)
        .bind(&params.finalizers)
        .bind(uid)
        .bind(params.revision)
        .bind(serde_json::to_value(&owner_references).unwrap_or_default())
        .bind(sqlx::types::Json(&params.labels))
        .fetch_optional(&mut *tx)
        .await;

        let updated = match updated {
            Ok(row) => row,
            // Changing api_version can move the row into a group where its (group, kind, name)
            // already exists. Surface the partial unique-index violation as NameConflict (409)
            // instead of leaking an internal Database error (500).
            Err(ref e) if Self::is_name_conflict(e) => return Err(StoreError::NameConflict),
            Err(e) => {
                return Err(
                    Self::identity_uniqueness_conflict(&e).unwrap_or_else(|| Self::db_error(e))
                )
            }
        };

        if let Some(row) = updated {
            tx.commit().await.map_err(Self::db_error)?;
            return Ok(row.into());
        }

        // The row is locked and revision-checked above, so zero rows here can
        // only result from an unexpected backend invariant violation.
        Err(StoreError::backend(std::io::Error::other(
            "locked resource update affected zero rows",
        )))
    }

    async fn delete(&self, uid: Uuid) -> Result<DeleteOutcome, StoreError> {
        let mut tx = self.pool.begin().await.map_err(Self::db_error)?;

        // Lock the row inside the transaction so a concurrent update_controller_finalizers()
        // can't add a finalizer between our read and the hard-delete branch below.
        let row = sqlx::query_as::<_, PgResourceRow>(
            "SELECT * FROM resource_store.resources WHERE uid = $1 FOR UPDATE",
        )
        .bind(uid)
        .fetch_optional(&mut *tx)
        .await
        .map_err(Self::db_error)?
        .ok_or(StoreError::NotFound)?;

        // ResourceDefinition deletion admission must run before any dependent
        // is tombstoned or an early blocking-dependent branch can commit.
        if row.kind == RESOURCE_DEFINITION_KIND {
            self.ensure_resource_definition_has_no_instances(&row, &mut tx)
                .await?;
        }

        let tombstoned = Self::tombstone_immediate_dependents(&mut tx, uid).await?;
        let has_blocking_dependents = Self::has_blocking_dependents(&mut tx, uid).await?;

        if has_blocking_dependents {
            // Structural children and explicitly blocking owner references keep
            // the owner visible until the collector drains them.
            let marked = sqlx::query_as::<_, PgResourceRow>(
                r#"
                UPDATE resource_store.resources
                SET deletion_timestamp = COALESCE(deletion_timestamp, NOW()),
                    finalizers = CASE
                        WHEN $2 = ANY(finalizers) THEN finalizers
                        ELSE array_append(finalizers, $2)
                    END,
                    revision = revision + 1
                WHERE uid = $1
                RETURNING *
                "#,
            )
            .bind(uid)
            .bind(CASCADE_DELETION_FINALIZER)
            .fetch_one(&mut *tx)
            .await
            .map_err(Self::db_error)?;
            tx.commit().await.map_err(Self::db_error)?;
            Self::log_tombstoned_dependents(&row, tombstoned);
            return Ok(DeleteOutcome::MarkedForDeletion(Box::new(marked.into())));
        }

        // Mark if finalizers, else hard-delete.
        if !row.finalizers.is_empty() {
            let marked = sqlx::query_as::<_, PgResourceRow>(
                r#"
                UPDATE resource_store.resources
                SET deletion_timestamp = COALESCE(deletion_timestamp, NOW()),
                    revision = revision + 1
                WHERE uid = $1
                RETURNING *
                "#,
            )
            .bind(uid)
            .fetch_one(&mut *tx)
            .await
            .map_err(Self::db_error)?;
            tx.commit().await.map_err(Self::db_error)?;
            Self::log_tombstoned_dependents(&row, tombstoned);
            return Ok(DeleteOutcome::MarkedForDeletion(Box::new(marked.into())));
        }
        // Deleting the `resources` row also removes it from the
        // `resource_definitions` view.
        sqlx::query("DELETE FROM resource_store.resources WHERE uid = $1")
            .bind(uid)
            .execute(&mut *tx)
            .await
            .map_err(Self::db_error)?;
        tx.commit().await.map_err(Self::db_error)?;
        Self::log_tombstoned_dependents(&row, tombstoned);

        Ok(DeleteOutcome::Deleted)
    }

    async fn try_collect(&self, uid: Uuid) -> Result<DeleteOutcome, StoreError> {
        let mut tx = self.pool.begin().await.map_err(Self::db_error)?;

        let row = sqlx::query_as::<_, PgResourceRow>(
            "SELECT * FROM resource_store.resources WHERE uid = $1 FOR UPDATE",
        )
        .bind(uid)
        .fetch_optional(&mut *tx)
        .await
        .map_err(Self::db_error)?
        .ok_or(StoreError::NotFound)?;

        // Not tombstoned: nothing to collect, return current state.
        if row.deletion_timestamp.is_none() {
            return Ok(DeleteOutcome::MarkedForDeletion(Box::new(row.into())));
        }

        let tombstoned = Self::tombstone_immediate_dependents(&mut tx, uid).await?;
        let has_blocking_dependents = Self::has_blocking_dependents(&mut tx, uid).await?;

        if has_blocking_dependents {
            // Fan out through every lifecycle edge, but retain the cascade
            // finalizer only for structural or explicitly blocking edges.
            let row = sqlx::query_as::<_, PgResourceRow>(
                r#"
                UPDATE resource_store.resources
                SET finalizers = CASE
                        WHEN $2 = ANY(finalizers) THEN finalizers
                        ELSE array_append(finalizers, $2)
                    END,
                    revision = CASE
                        WHEN $2 = ANY(finalizers) THEN revision
                        ELSE revision + 1
                    END
                WHERE uid = $1
                RETURNING *
                "#,
            )
            .bind(uid)
            .bind(CASCADE_DELETION_FINALIZER)
            .fetch_one(&mut *tx)
            .await
            .map_err(Self::db_error)?;
            tx.commit().await.map_err(Self::db_error)?;
            Self::log_tombstoned_dependents(&row, tombstoned);
            return Ok(DeleteOutcome::MarkedForDeletion(Box::new(row.into())));
        }

        // No children. Drop the cascade finalizer if present. Only bump revision when the
        // finalizer was actually removed so idempotent re-calls don't churn the version.
        let row = sqlx::query_as::<_, PgResourceRow>(
            r#"
            UPDATE resource_store.resources
            SET finalizers = array_remove(finalizers, $2),
                revision = CASE
                    WHEN $2 = ANY(finalizers) THEN revision + 1
                    ELSE revision
                END
            WHERE uid = $1
            RETURNING *
            "#,
        )
        .bind(uid)
        .bind(CASCADE_DELETION_FINALIZER)
        .fetch_one(&mut *tx)
        .await
        .map_err(Self::db_error)?;

        if !row.finalizers.is_empty() {
            tx.commit().await.map_err(Self::db_error)?;
            Self::log_tombstoned_dependents(&row, tombstoned);
            return Ok(DeleteOutcome::MarkedForDeletion(Box::new(row.into())));
        }

        // Clear: hard-delete.
        if row.kind == RESOURCE_DEFINITION_KIND {
            self.ensure_resource_definition_has_no_instances(&row, &mut tx)
                .await?;
        }
        // Deleting the `resources` row also removes it from the
        // `resource_definitions` view.
        sqlx::query("DELETE FROM resource_store.resources WHERE uid = $1")
            .bind(uid)
            .execute(&mut *tx)
            .await
            .map_err(Self::db_error)?;
        tx.commit().await.map_err(Self::db_error)?;
        Self::log_tombstoned_dependents(&row, tombstoned);

        Ok(DeleteOutcome::Deleted)
    }

    async fn list_pending_collection(&self, limit: i64) -> Result<Vec<ResourceRow>, StoreError> {
        let rows = sqlx::query_as::<_, PgResourceRow>(
            r#"
            SELECT * FROM resource_store.resources
            WHERE deletion_timestamp IS NOT NULL
            ORDER BY deletion_timestamp ASC
            LIMIT $1
            "#,
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(Self::db_error)?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    async fn list_deletion_blockers(&self, uid: Uuid) -> Result<DeletionBlockerReport, StoreError> {
        let mut tx = self.pool.begin().await.map_err(Self::db_error)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await
            .map_err(Self::db_error)?;

        let resource = sqlx::query_as::<_, PgResourceRow>(
            "SELECT * FROM resource_store.resources WHERE uid = $1",
        )
        .bind(uid)
        .fetch_optional(&mut *tx)
        .await
        .map_err(Self::db_error)?
        .ok_or(StoreError::NotFound)?;

        // A single query gives structural edges precedence when one dependent
        // also carries a blocking owner reference to the same resource.
        let rows = sqlx::query_as::<_, PgDeletionBlocker>(
            r#"
            SELECT true AS structural, api_version, kind, name, uid,
                   deletion_timestamp, finalizers
            FROM resource_store.resources
            WHERE parent_uid = $1

            UNION ALL

            SELECT false AS structural, api_version, kind, name, uid,
                   deletion_timestamp, finalizers
            FROM resource_store.resources
            WHERE owner_references @> jsonb_build_array(
                    jsonb_build_object(
                        'uid', $1::text,
                        'blockOwnerDeletion', true
                    )
                )
              AND parent_uid IS DISTINCT FROM $1

            ORDER BY structural DESC, api_version, kind, name, uid
            "#,
        )
        .bind(uid)
        .fetch_all(&mut *tx)
        .await
        .map_err(Self::db_error)?;

        tx.commit().await.map_err(Self::db_error)?;
        Ok(DeletionBlockerReport {
            resource: resource.into(),
            blockers: rows
                .into_iter()
                .map(|row| DeletionBlocker {
                    relationship: if row.structural {
                        DeletionBlockerRelationship::StructuralChild
                    } else {
                        DeletionBlockerRelationship::OwnerReference
                    },
                    api_version: row.api_version,
                    kind: row.kind,
                    name: row.name,
                    uid: row.uid,
                    deletion_timestamp: row.deletion_timestamp,
                    finalizers: row.finalizers,
                })
                .collect(),
        })
    }

    async fn ancestors(&self, uid: Uuid) -> Result<Vec<ResourceRow>, StoreError> {
        // One round trip rather than the per-level walk the placement and
        // path-resolution paths use: this read is on the authorization hot
        // path, where the caller knows the UID and wants the whole chain.
        //
        // `depth` counts upward from the leaf, so ordering by it descending
        // yields the root-first chain the contract promises. The chain is
        // bounded by MAX_PARENT_CHAIN_DEPTH; a row whose ancestry somehow
        // exceeds it is truncated at the root end rather than looping, and
        // parent_uid's foreign key plus the write-time cycle checks keep real
        // ancestry acyclic.
        let rows = sqlx::query_as::<_, PgResourceRow>(
            r#"
            WITH RECURSIVE chain AS (
                SELECT resource.*, 0 AS depth
                FROM resource_store.resources resource
                WHERE resource.uid = $1
                UNION ALL
                SELECT parent.*, chain.depth + 1
                FROM chain
                JOIN resource_store.resources parent ON parent.uid = chain.parent_uid
                WHERE chain.depth < $2
            )
            SELECT uid, api_version, kind, parent_uid, name, discriminator, labels, metadata,
                   spec, status, revision, finalizers, owner_references, deletion_timestamp,
                   created_at, updated_at
            FROM chain
            ORDER BY depth DESC
            "#,
        )
        .bind(uid)
        .bind(MAX_PARENT_CHAIN_DEPTH as i32)
        .fetch_all(&self.pool)
        .await
        .map_err(Self::db_error)?;

        Ok(rows.into_iter().map(Into::into).collect())
    }

    async fn resolve_path(&self, segments: &[PathSegment]) -> Result<Vec<ResourceRow>, StoreError> {
        if segments.is_empty() {
            return Err(StoreError::EmptyPath);
        }

        let mut tx = self.pool.begin().await.map_err(Self::db_error)?;
        let mut chain: Vec<ResourceRow> = Vec::with_capacity(segments.len());
        let mut current_parent: Option<Uuid> = None;

        for (idx, segment) in segments.iter().enumerate() {
            let row = match segment {
                PathSegment::Name {
                    api_versions,
                    kind,
                    name,
                } => {
                    let row: Option<PgResourceRow> = match current_parent {
                        None => sqlx::query_as::<_, PgResourceRow>(
                            "SELECT * FROM resource_store.resources WHERE api_version = ANY($1) AND kind = $2 AND name = $3 AND parent_uid IS NULL",
                        )
                        .bind(api_versions)
                        .bind(kind)
                        .bind(name)
                        .fetch_optional(&mut *tx)
                        .await
                        .map_err(Self::db_error)?,
                        Some(pid) => sqlx::query_as::<_, PgResourceRow>(
                            "SELECT * FROM resource_store.resources WHERE api_version = ANY($1) AND kind = $2 AND name = $3 AND parent_uid = $4",
                        )
                        .bind(api_versions)
                        .bind(kind)
                        .bind(name)
                        .bind(pid)
                        .fetch_optional(&mut *tx)
                        .await
                        .map_err(Self::db_error)?,
                    };
                    match row {
                        Some(r) => r,
                        None if idx + 1 == segments.len() => return Err(StoreError::NotFound),
                        None => return Err(StoreError::ParentNotFound),
                    }
                }
                PathSegment::Uid {
                    api_versions,
                    kind,
                    uid,
                } => {
                    let row: PgResourceRow = match sqlx::query_as::<_, PgResourceRow>(
                        "SELECT * FROM resource_store.resources WHERE uid = $1",
                    )
                    .bind(uid)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(Self::db_error)?
                    {
                        Some(r) => r,
                        None if idx + 1 == segments.len() => return Err(StoreError::NotFound),
                        None => return Err(StoreError::ParentNotFound),
                    };
                    if row.kind != *kind || !api_versions.iter().any(|v| v == &row.api_version) {
                        return Err(StoreError::KindMismatch {
                            expected: format!("kind {kind} in one of {api_versions:?}"),
                            got: format!("{}/{}", row.api_version, row.kind),
                        });
                    }
                    if row.parent_uid != current_parent {
                        // UID is from a different subtree.
                        return Err(StoreError::ParentNotFound);
                    }
                    row
                }
            };

            current_parent = Some(row.uid);
            chain.push(row.into());
        }

        tx.commit().await.map_err(Self::db_error)?;
        Ok(chain)
    }

    async fn update_controller_status(
        &self,
        uid: Uuid,
        controller_id: &str,
        status_value: serde_json::Value,
    ) -> Result<ResourceRow, StoreError> {
        validate_controller_id(controller_id).map_err(|e| StoreError::Validation(e.to_string()))?;

        let row = sqlx::query_as::<_, PgResourceRow>(
            r#"
            UPDATE resource_store.resources
            SET status = status || jsonb_build_object(
                    'controllers',
                    COALESCE(status->'controllers', '{}'::jsonb)
                    || jsonb_build_object($2::text, $3::jsonb)
                ),
                revision   = revision + 1,
                updated_at = NOW()
            WHERE uid = $1
            RETURNING *
            "#,
        )
        .bind(uid)
        .bind(controller_id)
        .bind(status_value)
        .fetch_optional(&self.pool)
        .await
        .map_err(Self::db_error)?
        .ok_or(StoreError::NotFound)?;

        Ok(row.into())
    }

    async fn update_controller_finalizers(
        &self,
        uid: Uuid,
        controller_id: &str,
        add: &[String],
        remove: &[String],
    ) -> Result<ResourceRow, StoreError> {
        validate_controller_id(controller_id).map_err(|e| StoreError::Validation(e.to_string()))?;

        // Store-managed finalizers (system.rise.dev/*) cannot be added or removed by controllers.
        for f in add.iter().chain(remove.iter()) {
            if f.starts_with(SYSTEM_FINALIZER_PREFIX) {
                return Err(StoreError::ReservedFinalizer(f.clone()));
            }
            if !is_controller_finalizer(f, controller_id) {
                return Err(StoreError::Validation(format!(
                    "finalizer '{f}' is not owned by controller '{controller_id}'"
                )));
            }
        }

        // Use SELECT FOR UPDATE inside a transaction to serialise concurrent finalizer
        // mutations from different controllers on the same resource, preventing lost updates.
        let mut tx = self.pool.begin().await.map_err(Self::db_error)?;

        let current = sqlx::query_as::<_, PgResourceRow>(
            "SELECT * FROM resource_store.resources WHERE uid = $1 FOR UPDATE",
        )
        .bind(uid)
        .fetch_optional(&mut *tx)
        .await
        .map_err(Self::db_error)?
        .ok_or(StoreError::NotFound)?;

        let remove_set: std::collections::HashSet<&str> =
            remove.iter().map(String::as_str).collect();
        let mut new_finalizers: Vec<String> = current
            .finalizers
            .into_iter()
            .filter(|f| !remove_set.contains(f.as_str()))
            .collect();
        for f in add {
            if !new_finalizers.contains(f) {
                new_finalizers.push(f.clone());
            }
        }

        let row = sqlx::query_as::<_, PgResourceRow>(
            r#"
            UPDATE resource_store.resources
            SET finalizers = $1, revision = revision + 1, updated_at = NOW()
            WHERE uid = $2
            RETURNING *
            "#,
        )
        .bind(&new_finalizers)
        .bind(uid)
        .fetch_one(&mut *tx)
        .await
        .map_err(Self::db_error)?;

        tx.commit().await.map_err(Self::db_error)?;

        Ok(row.into())
    }

    async fn operator_update_status(
        &self,
        uid: Uuid,
        operator: &str,
        status_value: serde_json::Value,
    ) -> Result<ResourceRow, StoreError> {
        // Store under `status.controllers.operator:<operator>` — same nested
        // structure as update_controller_status but with the operator key.
        let operator_key = format!("operator:{operator}");
        let row = sqlx::query_as::<_, PgResourceRow>(
            r#"
            UPDATE resource_store.resources
            SET status = status || jsonb_build_object(
                    'controllers',
                    COALESCE(status->'controllers', '{}'::jsonb)
                    || jsonb_build_object($2::text, $3::jsonb)
                ),
                revision   = revision + 1,
                updated_at = NOW()
            WHERE uid = $1
            RETURNING *
            "#,
        )
        .bind(uid)
        .bind(&operator_key)
        .bind(status_value)
        .fetch_optional(&self.pool)
        .await
        .map_err(Self::db_error)?
        .ok_or(StoreError::NotFound)?;

        Ok(row.into())
    }

    async fn operator_update_finalizers(
        &self,
        uid: Uuid,
        operator: &str,
        add: &[String],
        remove: &[String],
    ) -> Result<ResourceRow, StoreError> {
        // Block system-managed finalizers — operators must not clear cascade
        // deletion finalizers to prevent GC corruption.
        for f in add.iter().chain(remove.iter()) {
            if f.starts_with(SYSTEM_FINALIZER_PREFIX) {
                return Err(StoreError::ReservedFinalizer(f.clone()));
            }
        }

        let _ = operator; // operator identity is recorded in the audit log by the handler

        let mut tx = self.pool.begin().await.map_err(Self::db_error)?;

        let current = sqlx::query_as::<_, PgResourceRow>(
            "SELECT * FROM resource_store.resources WHERE uid = $1 FOR UPDATE",
        )
        .bind(uid)
        .fetch_optional(&mut *tx)
        .await
        .map_err(Self::db_error)?
        .ok_or(StoreError::NotFound)?;

        let remove_set: std::collections::HashSet<&str> =
            remove.iter().map(String::as_str).collect();
        let mut new_finalizers: Vec<String> = current
            .finalizers
            .into_iter()
            .filter(|f| !remove_set.contains(f.as_str()))
            .collect();
        for f in add {
            if !new_finalizers.contains(f) {
                new_finalizers.push(f.clone());
            }
        }

        let row = sqlx::query_as::<_, PgResourceRow>(
            r#"
            UPDATE resource_store.resources
            SET finalizers = $1, revision = revision + 1, updated_at = NOW()
            WHERE uid = $2
            RETURNING *
            "#,
        )
        .bind(&new_finalizers)
        .bind(uid)
        .fetch_one(&mut *tx)
        .await
        .map_err(Self::db_error)?;

        tx.commit().await.map_err(Self::db_error)?;

        Ok(row.into())
    }

    async fn resolve_collection(
        &self,
        collection: &str,
    ) -> Result<Option<CollectionInfo>, StoreError> {
        // Built-ins take priority
        if let Some(info) = self.builtin_collection_info(collection) {
            return Ok(Some(info));
        }

        let row = sqlx::query(
            r#"
            SELECT rd.uid, rd.group_name, rd.kind, rd.versions,
                   rd.allowed_status_controller_ids, r.spec
            FROM resource_store.resource_definitions rd
            JOIN resource_store.resources r ON r.uid = rd.uid
            WHERE rd.plural = $1
            "#,
        )
        .bind(collection)
        .fetch_optional(&self.pool)
        .await
        .map_err(Self::db_error)?;

        let Some(row) = row else {
            return Ok(None);
        };

        let versions: Vec<rise_resource_api::ResourceDefinitionVersion> =
            serde_json::from_value(row.try_get("versions").map_err(Self::db_error)?).map_err(
                |e| StoreError::Validation(format!("invalid versions in ResourceDefinition: {e}")),
            )?;

        let group_name: String = row.try_get("group_name").map_err(Self::db_error)?;
        // Mirror resolve_collection_by_kind's fallback: if the storage version is not served,
        // pick any served version (resolve_collection_version rejects non-served versions).
        let storage = versions.iter().find(|v| v.storage);
        let version = storage
            .filter(|v| v.served)
            .or_else(|| versions.iter().find(|v| v.served))
            .or(storage)
            .map(|v| v.name.clone())
            .ok_or_else(|| {
                StoreError::Validation(format!(
                    "ResourceDefinition '{collection}' declares no versions"
                ))
            })?;

        self.resolve_collection_version(&group_name, &version, collection)
            .await
    }

    async fn resolve_collection_version(
        &self,
        group: &str,
        version: &str,
        collection: &str,
    ) -> Result<Option<CollectionInfo>, StoreError> {
        // Built-in collections have no row in the resource_definitions projection and
        // use native validators, so they can't be resolved from the table. The built-in
        // registry and its api_version live in `self.builtins`; resolve against
        // it rather than re-encoding the group/version as string literals here.
        if let Some(info) = self.builtin_collection_info(collection) {
            let matches = info
                .api_version
                .split_once('/')
                .is_some_and(|(g, v)| g == group && v == version);
            if matches {
                return Ok(Some(info));
            }
            // A built-in collection requested at a version it does not serve.
            return Ok(None);
        }

        let cache_key = format!("{group}/{version}/{collection}");
        let cached_validator = self
            .schema_cache
            .read()
            .ok()
            .and_then(|c| c.get(&cache_key).cloned());

        let row = sqlx::query(
            r#"
            SELECT rd.uid, rd.group_name, rd.kind, rd.versions,
                   rd.allowed_status_controller_ids, r.spec
            FROM resource_store.resource_definitions rd
            JOIN resource_store.resources r ON r.uid = rd.uid
            WHERE rd.group_name = $1 AND rd.plural = $2
            "#,
        )
        .bind(group)
        .bind(collection)
        .fetch_optional(&self.pool)
        .await
        .map_err(Self::db_error)?;

        let Some(row) = row else {
            return Ok(None);
        };

        let spec: ResourceDefinitionSpec =
            serde_json::from_value(row.try_get("spec").map_err(Self::db_error)?).map_err(|e| {
                StoreError::Validation(format!("invalid ResourceDefinition spec: {e}"))
            })?;

        let versions = spec.versions.clone();
        // An undefined or non-served version is not addressable — treat it like
        // a missing collection (`UnknownVersion` maps to 404, as Kubernetes
        // returns for an unserved apiVersion).
        let requested = versions.iter().find(|v| v.name == version).ok_or_else(|| {
            StoreError::UnknownVersion(format!(
                "version '{version}' is not defined for collection '{collection}'"
            ))
        })?;
        if !requested.served {
            return Err(StoreError::UnknownVersion(format!(
                "version '{version}' is not served by collection '{collection}'"
            )));
        }

        let storage_version = versions
            .iter()
            .find(|v| v.storage)
            .map(|v| v.name.clone())
            .ok_or_else(|| {
                StoreError::Validation(format!(
                    "ResourceDefinition '{collection}' has no storage version"
                ))
            })?;
        let storage_api_version = Self::api_version(group, &storage_version);
        let served_api_versions: Vec<String> = versions
            .iter()
            .filter(|v| v.served)
            .map(|v| Self::api_version(group, &v.name))
            .collect();
        let declared_api_versions: Vec<String> = versions
            .iter()
            .map(|v| Self::api_version(group, &v.name))
            .collect();
        let api_version = Self::api_version(group, version);

        let allowed: Vec<String> = row
            .try_get("allowed_status_controller_ids")
            .map_err(Self::db_error)?;

        let kind: String = row.try_get("kind").map_err(Self::db_error)?;

        // Use the cached validator, or compile one and store it in the cache. A schema that
        // fails to compile is a hard error: silently falling back to NoOpValidator would let
        // invalid specs through. Registration validates compilability up front, so this
        // branch only fires for rows that bypassed validation (e.g. via direct SQL).
        let schema_validator: Arc<dyn SpecValidator> = match cached_validator {
            Some(v) => v,
            None => {
                let v: Arc<dyn SpecValidator> = match requested.schema.clone() {
                    Some(schema) => Arc::new(JsonSchemaValidator::new(schema).map_err(|e| {
                        StoreError::Validation(format!(
                            "ResourceDefinition '{collection}' version '{version}' has an invalid JSON schema: {e}"
                        ))
                    })?) as Arc<dyn SpecValidator>,
                    None => Arc::new(NoOpValidator),
                };
                if let Ok(mut cache) = self.schema_cache.write() {
                    cache.insert(cache_key, v.clone());
                }
                v
            }
        };

        Ok(Some(CollectionInfo {
            api_version,
            storage_api_version,
            served_api_versions,
            declared_api_versions,
            kind,
            parent: spec.parent,
            spec_validator: schema_validator,
            allowed_status_controller_ids: allowed,
        }))
    }

    async fn resolve_collection_by_kind(
        &self,
        group: &str,
        kind: &str,
    ) -> Result<Option<CollectionInfo>, StoreError> {
        // Built-in kinds have no `resource_definitions` row, so resolve them
        // directly through the registry. The registry's `(group, kind)` index
        // replaces what used to be a hardcoded match here, so adding a new
        // built-in does not require touching this method.
        if let Some(reg) = self.builtins.lookup_by_group_kind(group, kind) {
            return Ok(Some(reg.collection_info()));
        }

        // `(group_name, kind)` is unique (resource_definitions_group_kind_unique).
        let row = sqlx::query(
            r#"
            SELECT plural, versions
            FROM resource_store.resource_definitions
            WHERE group_name = $1 AND kind = $2
            "#,
        )
        .bind(group)
        .bind(kind)
        .fetch_optional(&self.pool)
        .await
        .map_err(Self::db_error)?;

        let Some(row) = row else {
            return Ok(None);
        };

        let plural: String = row.try_get("plural").map_err(Self::db_error)?;
        let versions: Vec<rise_resource_api::ResourceDefinitionVersion> =
            serde_json::from_value(row.try_get("versions").map_err(Self::db_error)?).map_err(
                |e| StoreError::Validation(format!("invalid versions in ResourceDefinition: {e}")),
            )?;

        // Ancestors are not version-addressed: resolve at the storage version,
        // falling back to any served version when the storage version is not
        // served (`resolve_collection_version` rejects non-served versions).
        let storage = versions.iter().find(|v| v.storage);
        let version = storage
            .filter(|v| v.served)
            .or_else(|| versions.iter().find(|v| v.served))
            .or(storage)
            .map(|v| v.name.clone())
            .ok_or_else(|| {
                StoreError::Validation(format!(
                    "ResourceDefinition '{plural}' declares no versions"
                ))
            })?;

        self.resolve_collection_version(group, &version, &plural)
            .await
    }

    async fn register_resource_definition(
        &self,
        params: CreateResourceParams,
    ) -> Result<ResourceRow, StoreError> {
        Self::ensure_owner_references_supported(
            &params.api_version,
            RESOURCE_DEFINITION_KIND,
            &params.owner_references,
        )?;
        // Validate spec format and reserved-name rules first so plural/group are sound.
        ResourceDefinitionValidator.validate_spec(&params.spec)?;

        // Parse once; safe to unwrap because validate_spec succeeded above
        let spec: ResourceDefinitionSpec = serde_json::from_value(params.spec.clone())
            .expect("spec parseable: ResourceDefinitionValidator.validate_spec succeeded");

        // Reservations apply to new definitions only. Existing installations may contain a
        // definition whose identity became reserved in a later release; keep allowing updates
        // that leave those immutable identity fields unchanged until built-in activation audits
        // and resolves such conflicts explicitly.
        validate_new_resource_definition_identity(&spec)?;

        // ResourceDefinition names follow the {plural}.{group} convention. Identity fields are
        // immutable post-creation, so an inconsistent name becomes permanent — reject upfront.
        let expected_name = format!("{}.{}", spec.plural, spec.group);
        if params.name != expected_name {
            return Err(StoreError::Validation(format!(
                "ResourceDefinition name must equal '{{plural}}.{{group}}' \
                 (expected '{expected_name}', got '{}')",
                params.name
            )));
        }

        // Validate every declared JSON schema compiles. Any served version can
        // be resolved independently later, so deferring this would make the API
        // fail only when that version is first used.
        Self::validate_version_schemas(&spec, &format!("'{}'", params.name))?;

        let mut tx = self.pool.begin().await.map_err(Self::db_error)?;
        if !params.owner_references.is_empty() {
            Self::lock_owner_reference_graph(&mut tx).await?;
        }

        // Acquire a transaction-scoped advisory lock before the cycle check so that two
        // concurrent registrations cannot both pass the check before either commits.
        // The lock is automatically released when the transaction commits or rolls back.
        sqlx::query(
            "SELECT pg_advisory_xact_lock(hashtext('resource_definitions_parent_cycle')::bigint)",
        )
        .execute(&mut *tx)
        .await
        .map_err(Self::db_error)?;

        // Reject a parent chain that would cycle back to this kind. The advisory lock above
        // serialises concurrent registrations; reads go through `tx` so they see the same
        // consistent snapshot that the lock protects.
        self.ensure_no_parent_cycle(&spec, &mut tx).await?;

        let metadata = serde_json::to_value(&params.annotations).unwrap_or_default();

        // The `resources` INSERT enforces RD identity uniqueness via the partial
        // `resource_definitions_{plural,group_kind}_unique` indexes;
        // `insert_resource_row_with_retry` maps those violations to a validation
        // error. The `resource_definitions` projection is a view, so there is no
        // second row to write.
        let resource_row =
            Self::insert_resource_row_with_retry(&mut tx, Uuid::new_v4(), &params, metadata)
                .await?;
        let owner_references = Self::validate_owner_references(
            &mut tx,
            resource_row.uid,
            &params.owner_references,
            false,
        )
        .await?;

        tx.commit().await.map_err(Self::db_error)?;

        self.invalidate_schema_cache(&spec.group, &spec.plural);

        let mut resource_row = resource_row;
        resource_row.owner_references = owner_references;
        Ok(resource_row)
    }

    async fn update_resource_definition(
        &self,
        uid: Uuid,
        params: UpdateResourceParams,
    ) -> Result<ResourceRow, StoreError> {
        Self::ensure_owner_references_supported(
            API_VERSION_V1ALPHA1,
            RESOURCE_DEFINITION_KIND,
            &params.owner_references,
        )?;
        // Validate the new spec
        ResourceDefinitionValidator.validate_spec(&params.spec)?;

        let new_spec: ResourceDefinitionSpec = serde_json::from_value(params.spec.clone())
            .expect("spec parseable: ResourceDefinitionValidator.validate_spec succeeded");

        // Validate every declared JSON schema compiles. Symmetric with register.
        Self::validate_version_schemas(&new_spec, "")?;

        let mut tx = self.pool.begin().await.map_err(Self::db_error)?;
        if !params.owner_references.is_empty() {
            Self::lock_owner_reference_graph(&mut tx).await?;
        }

        // Lock the row and fetch current state
        let current = sqlx::query_as::<_, PgResourceRow>(
            "SELECT * FROM resource_store.resources WHERE uid = $1 FOR UPDATE",
        )
        .bind(uid)
        .fetch_optional(&mut *tx)
        .await
        .map_err(Self::db_error)?
        .ok_or(StoreError::NotFound)?;

        if current.kind != RESOURCE_DEFINITION_KIND {
            return Err(StoreError::Validation(
                "resource is not a ResourceDefinition".to_string(),
            ));
        }

        // Enforce immutability of identity fields
        let old_spec: ResourceDefinitionSpec = serde_json::from_value(current.spec.clone())
            .map_err(|e| {
                StoreError::Validation(format!("stored ResourceDefinition spec is invalid: {e}"))
            })?;

        if new_spec.group != old_spec.group
            || new_spec.kind != old_spec.kind
            || new_spec.plural != old_spec.plural
            || new_spec.parent != old_spec.parent
        {
            return Err(StoreError::Validation(
                "ResourceDefinition identity fields (group, kind, plural, parent) are immutable"
                    .to_string(),
            ));
        }

        // A row can be stored at any declared version (instances are created at the
        // storage version, and `update` migrates rows to the current storage version).
        // Removing a version that still has stored instances would orphan those rows
        // from path resolution, so reject such an update. `group` is immutable, so
        // old and new group are equal.
        let removed_api_versions: Vec<String> = old_spec
            .versions
            .iter()
            .filter(|ov| !new_spec.versions.iter().any(|nv| nv.name == ov.name))
            .map(|ov| Self::api_version(&new_spec.group, &ov.name))
            .collect();
        if !removed_api_versions.is_empty() {
            // Lock the ResourceDefinition's `resources` row so concurrent
            // instance creates (which take a FOR SHARE lock on the same row via
            // lock_matching_definition_for_write) cannot slip in between this
            // count and the commit.
            sqlx::query("SELECT uid FROM resource_store.resources WHERE uid = $1 FOR UPDATE")
                .bind(uid)
                .execute(&mut *tx)
                .await
                .map_err(Self::db_error)?;

            let instance_count: i64 = sqlx::query_scalar(
                r#"
                SELECT COUNT(*)
                FROM resource_store.resources
                WHERE kind = $1
                  AND api_version = ANY($2)
                "#,
            )
            .bind(&new_spec.kind)
            .bind(&removed_api_versions)
            .fetch_one(&mut *tx)
            .await
            .map_err(Self::db_error)?;

            if instance_count > 0 {
                return Err(StoreError::Validation(format!(
                    "cannot remove version(s) from ResourceDefinition '{}' while \
                     {instance_count} instance(s) still use them",
                    current.name
                )));
            }
        }

        let metadata = serde_json::to_value(&params.annotations).unwrap_or_default();
        let owner_references =
            Self::validate_owner_references(&mut tx, uid, &params.owner_references, true).await?;

        // Update the resources row with optimistic concurrency
        let updated = sqlx::query_as::<_, PgResourceRow>(
            r#"
            UPDATE resource_store.resources
            SET api_version = COALESCE($1, api_version),
                metadata   = $2,
                spec       = $3,
                finalizers = $4,
                owner_references = $7,
                labels     = $8,
                revision   = revision + 1,
                updated_at = NOW()
            WHERE uid = $5 AND revision = $6
            RETURNING *
            "#,
        )
        .bind(params.api_version.as_deref())
        .bind(metadata)
        .bind(&params.spec)
        .bind(&params.finalizers)
        .bind(uid)
        .bind(params.revision)
        .bind(serde_json::to_value(&owner_references).unwrap_or_default())
        .bind(sqlx::types::Json(&params.labels))
        .fetch_optional(&mut *tx)
        .await;

        let updated = match updated {
            Ok(row) => row,
            // Defensively map a name-uniqueness violation (e.g. an api_version change that
            // collides on (group, kind, name)) to NameConflict rather than a raw DB error.
            Err(ref e) if Self::is_name_conflict(e) => return Err(StoreError::NameConflict),
            Err(e) => return Err(Self::db_error(e)),
        };

        let row = match updated {
            Some(r) => r,
            None => {
                return Err(StoreError::RevisionConflict {
                    expected: params.revision,
                    found: current.revision,
                })
            }
        };

        // The mutable fields (versions, allowed_status_controller_ids) live in
        // `spec`, which the `resources` UPDATE above already wrote — the
        // `resource_definitions` view reflects it with no separate sync.
        tx.commit().await.map_err(Self::db_error)?;

        self.invalidate_schema_cache(&new_spec.group, &new_spec.plural);

        Ok(row.into())
    }
}

fn is_controller_finalizer(finalizer: &str, controller_id: &str) -> bool {
    finalizer == controller_id || finalizer.starts_with(&format!("{controller_id}/"))
}

#[cfg(test)]
mod contract_tests {
    use super::*;

    fn assert_resource_store<T: ResourceStore>() {}

    #[test]
    fn pg_resource_store_implements_api_contract() {
        assert_resource_store::<PgResourceStore>();
    }
}
