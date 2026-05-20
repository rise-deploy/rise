use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use uuid::Uuid;

use rise_resource_api::{
    validate_controller_id, validate_resource_name, ResourceDefinitionSpec, ResourceScope,
    API_VERSION_V1ALPHA1, ORGANIZATION_COLLECTION, ORGANIZATION_KIND,
    RESOURCE_DEFINITION_COLLECTION, RESOURCE_DEFINITION_KIND,
};
use sqlx::{PgPool, Row};

use crate::discriminator;
use crate::error::StoreError;
use crate::models::ResourceRow;
use crate::store::{
    CollectionInfo, CreateResourceParams, DeleteOutcome, PathSegment, PropagationPolicy,
    ResourceStore, UpdateResourceParams, CASCADE_DELETION_FINALIZER, SYSTEM_FINALIZER_PREFIX,
};
use crate::validation::{
    JsonSchemaValidator, NoOpValidator, OrganizationValidator, ResourceDefinitionValidator,
    SpecValidator,
};

pub struct PgResourceStore {
    pool: PgPool,
    /// Cache of compiled JSON schema validators keyed by collection plural name.
    /// Populated on first resolve_collection call; invalidated on register/update.
    schema_cache: RwLock<HashMap<String, Arc<dyn SpecValidator>>>,
}

impl PgResourceStore {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            schema_cache: RwLock::new(HashMap::new()),
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

    fn is_discriminator_conflict(err: &sqlx::Error) -> bool {
        if let sqlx::Error::Database(db) = err {
            let c = db.constraint().unwrap_or("");
            return c == "resources_child_discriminator_unique"
                || c == "resources_root_discriminator_unique";
        }
        false
    }

    fn builtin_collection_info(collection: &str) -> Option<CollectionInfo> {
        match collection {
            ORGANIZATION_COLLECTION => Some(CollectionInfo {
                api_version: API_VERSION_V1ALPHA1.to_string(),
                storage_api_version: API_VERSION_V1ALPHA1.to_string(),
                served_api_versions: vec![API_VERSION_V1ALPHA1.to_string()],
                kind: ORGANIZATION_KIND.to_string(),
                scope: ResourceScope::Root,
                parent: None,
                spec_validator: Arc::new(OrganizationValidator),
                allowed_status_controller_ids: vec![],
            }),
            RESOURCE_DEFINITION_COLLECTION => Some(CollectionInfo {
                api_version: API_VERSION_V1ALPHA1.to_string(),
                storage_api_version: API_VERSION_V1ALPHA1.to_string(),
                served_api_versions: vec![API_VERSION_V1ALPHA1.to_string()],
                kind: RESOURCE_DEFINITION_KIND.to_string(),
                scope: ResourceScope::Root,
                parent: None,
                spec_validator: Arc::new(ResourceDefinitionValidator),
                allowed_status_controller_ids: vec![],
            }),
            _ => None,
        }
    }

    /// Retry an INSERT into `resources` up to 10 times, generating a fresh discriminator on each
    /// discriminator collision. Returns the inserted row or a `StoreError`.
    async fn insert_resource_row_with_retry(
        conn: &mut sqlx::PgConnection,
        params: &CreateResourceParams,
        metadata: serde_json::Value,
    ) -> Result<ResourceRow, StoreError> {
        for _ in 0..10 {
            let discriminator = discriminator::generate();
            let result = sqlx::query_as::<_, ResourceRow>(
                r#"
                INSERT INTO resource_store.resources
                    (api_version, kind, parent_uid, name, discriminator, metadata, spec, finalizers)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                RETURNING *
                "#,
            )
            .bind(&params.api_version)
            .bind(&params.kind)
            .bind(params.parent_uid)
            .bind(&params.name)
            .bind(&discriminator)
            .bind(metadata.clone())
            .bind(&params.spec)
            .bind(&params.finalizers)
            .fetch_one(&mut *conn)
            .await;

            match result {
                Ok(row) => return Ok(row),
                Err(ref e) if Self::is_name_conflict(e) => return Err(StoreError::NameConflict),
                Err(ref e) if Self::is_discriminator_conflict(e) => continue,
                Err(e) => return Err(StoreError::Database(e)),
            }
        }

        Err(StoreError::DiscriminatorExhausted)
    }

    async fn ensure_resource_definition_has_no_instances(
        &self,
        row: &ResourceRow,
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

        // Lock the resource_definitions row for the duration of this transaction.
        // Instance writes acquire a FOR SHARE lock on the same row before inserting,
        // so they cannot slip in after this count check and before deletion commits.
        sqlx::query(
            "SELECT uid FROM resource_store.resource_definitions WHERE uid = $1 FOR UPDATE",
        )
        .bind(row.uid)
        .execute(&mut **tx)
        .await?;

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
        .await?;

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
            return Ok(());
        };

        let row = sqlx::query(
            r#"
            SELECT rd.uid
            FROM resource_store.resource_definitions rd
            WHERE rd.group_name = $1
              AND rd.kind = $2
              AND rd.versions @> $3::jsonb
            FOR SHARE
            "#,
        )
        .bind(group)
        .bind(kind)
        .bind(serde_json::json!([{ "name": version }]))
        .fetch_optional(&mut **tx)
        .await?;

        if required && row.is_none() {
            return Err(StoreError::Validation(format!(
                "no ResourceDefinition serves '{api_version}/{kind}'"
            )));
        }

        Ok(())
    }

    fn api_version(group: &str, version: &str) -> String {
        format!("{group}/{version}")
    }

    fn invalidate_schema_cache(&self, group: &str, plural: &str) {
        if let Ok(mut cache) = self.schema_cache.write() {
            cache.retain(|key, _| {
                let Some((key_group, rest)) = key.split_once('/') else {
                    return true;
                };
                let Some((_, key_plural)) = rest.rsplit_once('/') else {
                    return true;
                };
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
}

#[async_trait::async_trait]
impl ResourceStore for PgResourceStore {
    async fn create(&self, params: CreateResourceParams) -> Result<ResourceRow, StoreError> {
        validate_resource_name(&params.name).map_err(|e| StoreError::Validation(e.to_string()))?;

        if let Some(v) = &params.validator {
            v.validate_spec(&params.spec)?;
        }

        let metadata = serde_json::to_value(&params.annotations).unwrap_or_default();

        let mut tx = self.pool.begin().await?;
        if params.kind != RESOURCE_DEFINITION_KIND {
            let is_builtin =
                params.api_version == API_VERSION_V1ALPHA1 && params.kind == ORGANIZATION_KIND;
            Self::lock_matching_definition_for_write(
                &mut tx,
                &params.api_version,
                &params.kind,
                params.validator.is_some() && !is_builtin,
            )
            .await?;
        }
        let row = Self::insert_resource_row_with_retry(&mut tx, &params, metadata).await?;
        tx.commit().await?;
        Ok(row)
    }

    async fn get(&self, uid: Uuid) -> Result<Option<ResourceRow>, StoreError> {
        let row = sqlx::query_as::<_, ResourceRow>(
            "SELECT * FROM resource_store.resources WHERE uid = $1",
        )
        .bind(uid)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn get_by_name(
        &self,
        api_version: &str,
        kind: &str,
        name: &str,
        parent_uid: Option<Uuid>,
    ) -> Result<Option<ResourceRow>, StoreError> {
        let row =
            match parent_uid {
                None => sqlx::query_as::<_, ResourceRow>(
                    "SELECT * FROM resource_store.resources WHERE api_version = $1 AND kind = $2 AND name = $3 AND parent_uid IS NULL",
                )
                .bind(api_version)
                .bind(kind)
                .bind(name)
                .fetch_optional(&self.pool)
                .await?,
                Some(pid) => {
                    sqlx::query_as::<_, ResourceRow>(
                        "SELECT * FROM resource_store.resources WHERE api_version = $1 AND kind = $2 AND name = $3 AND parent_uid = $4",
                    )
                    .bind(api_version)
                    .bind(kind)
                    .bind(name)
                    .bind(pid)
                    .fetch_optional(&self.pool)
                    .await?
                }
            };
        Ok(row)
    }

    async fn list(
        &self,
        api_version: &str,
        kind: &str,
        parent_uid: Option<Uuid>,
    ) -> Result<Vec<ResourceRow>, StoreError> {
        let rows =
            match parent_uid {
                None => sqlx::query_as::<_, ResourceRow>(
                    "SELECT * FROM resource_store.resources WHERE api_version = $1 AND kind = $2 AND parent_uid IS NULL ORDER BY name",
                )
                .bind(api_version)
                .bind(kind)
                .fetch_all(&self.pool)
                .await?,
                Some(pid) => {
                    sqlx::query_as::<_, ResourceRow>(
                        "SELECT * FROM resource_store.resources WHERE api_version = $1 AND kind = $2 AND parent_uid = $3 ORDER BY name",
                    )
                    .bind(api_version)
                    .bind(kind)
                    .bind(pid)
                    .fetch_all(&self.pool)
                    .await?
                }
            };
        Ok(rows)
    }

    async fn list_versions(
        &self,
        api_versions: &[String],
        kind: &str,
        parent_uid: Option<Uuid>,
    ) -> Result<Vec<ResourceRow>, StoreError> {
        let rows =
            match parent_uid {
                None => sqlx::query_as::<_, ResourceRow>(
                    "SELECT * FROM resource_store.resources WHERE api_version = ANY($1) AND kind = $2 AND parent_uid IS NULL ORDER BY name",
                )
                .bind(api_versions)
                .bind(kind)
                .fetch_all(&self.pool)
                .await?,
                Some(pid) => {
                    sqlx::query_as::<_, ResourceRow>(
                        "SELECT * FROM resource_store.resources WHERE api_version = ANY($1) AND kind = $2 AND parent_uid = $3 ORDER BY name",
                    )
                    .bind(api_versions)
                    .bind(kind)
                    .bind(pid)
                    .fetch_all(&self.pool)
                    .await?
                }
            };
        Ok(rows)
    }

    async fn update(
        &self,
        uid: Uuid,
        params: UpdateResourceParams,
    ) -> Result<ResourceRow, StoreError> {
        // ResourceDefinitions must go through update_resource_definition to keep the
        // resource_definitions projection table in sync.
        let current_identity: Option<(String, String)> =
            sqlx::query_as("SELECT api_version, kind FROM resource_store.resources WHERE uid = $1")
                .bind(uid)
                .fetch_optional(&self.pool)
                .await?;
        let Some((current_api_version, kind)) = current_identity else {
            return Err(StoreError::NotFound);
        };
        match kind.as_str() {
            RESOURCE_DEFINITION_KIND => {
                return Err(StoreError::Validation(
                    "ResourceDefinitions must be updated through update_resource_definition"
                        .to_string(),
                ))
            }
            _ => {}
        }

        if let Some(v) = &params.validator {
            v.validate_spec(&params.spec)?;
        }

        let metadata = serde_json::to_value(&params.annotations).unwrap_or_default();

        // Include the expected revision in the WHERE clause so the update is atomic:
        // a concurrent write that already incremented the revision will cause zero rows
        // to be affected, which we detect and map to RevisionConflict.
        let mut tx = self.pool.begin().await?;
        if params.validator.is_some() {
            let target_api_version = params
                .api_version
                .as_deref()
                .unwrap_or(&current_api_version);
            let is_builtin =
                target_api_version == API_VERSION_V1ALPHA1 && kind == ORGANIZATION_KIND;
            Self::lock_matching_definition_for_write(
                &mut tx,
                target_api_version,
                &kind,
                !is_builtin,
            )
            .await?;
        }

        let updated = sqlx::query_as::<_, ResourceRow>(
            r#"
            UPDATE resource_store.resources
            SET api_version = COALESCE($1, api_version),
                metadata   = $2,
                spec       = $3,
                finalizers = $4,
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
        .fetch_optional(&mut *tx)
        .await?;

        if let Some(row) = updated {
            tx.commit().await?;
            return Ok(row);
        }

        // Zero rows affected — revision mismatch (NotFound already handled above)
        let current = self.get(uid).await?.ok_or(StoreError::NotFound)?;
        Err(StoreError::RevisionConflict {
            expected: params.revision,
            found: current.revision,
        })
    }

    async fn delete(
        &self,
        uid: Uuid,
        policy: PropagationPolicy,
    ) -> Result<DeleteOutcome, StoreError> {
        let mut tx = self.pool.begin().await?;

        // Lock the row inside the transaction so a concurrent update_controller_finalizers()
        // can't add a finalizer between our read and the hard-delete branch below.
        let row = sqlx::query_as::<_, ResourceRow>(
            "SELECT * FROM resource_store.resources WHERE uid = $1 FOR UPDATE",
        )
        .bind(uid)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(StoreError::NotFound)?;

        let child_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM resource_store.resources WHERE parent_uid = $1",
        )
        .bind(uid)
        .fetch_one(&mut *tx)
        .await?;

        match policy {
            PropagationPolicy::Cascade => {
                if child_count > 0 {
                    // Stamp immediate children that aren't already marked. A future GC sweep
                    // (try_collect) drives the fan-out down the remaining levels. Bump
                    // revision on each affected child so concurrent updates see the change.
                    sqlx::query(
                        r#"
                        UPDATE resource_store.resources
                        SET deletion_timestamp = NOW(),
                            revision = revision + 1
                        WHERE parent_uid = $1 AND deletion_timestamp IS NULL
                        "#,
                    )
                    .bind(uid)
                    .execute(&mut *tx)
                    .await?;

                    // Stamp the parent and attach the cascade finalizer (idempotent).
                    let marked = sqlx::query_as::<_, ResourceRow>(
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
                    .await?;
                    tx.commit().await?;
                    return Ok(DeleteOutcome::MarkedForDeletion(Box::new(marked)));
                }
            }
            PropagationPolicy::Orphan => {
                if child_count > 0 {
                    // Detach children. The partial unique index on
                    // (api_version, kind, name) WHERE parent_uid IS NULL may reject this if a
                    // name collides at the root scope.
                    // Bump revision so the detach is observable.
                    let result = sqlx::query(
                        r#"
                        UPDATE resource_store.resources
                        SET parent_uid = NULL,
                            revision = revision + 1
                        WHERE parent_uid = $1
                        "#,
                    )
                    .bind(uid)
                    .execute(&mut *tx)
                    .await;
                    if let Err(e) = result {
                        // A child's name or discriminator may collide at the root scope
                        // (partial unique indexes on (api_version, kind, name) and discriminator
                        // when parent_uid IS NULL). Surface both as NameConflict for parity with
                        // reparent() rather than leaking an internal Database error.
                        if Self::is_name_conflict(&e) || Self::is_discriminator_conflict(&e) {
                            return Err(StoreError::NameConflict);
                        }
                        return Err(StoreError::Database(e));
                    }
                }
            }
        }

        // No (remaining) children. Mark if finalizers, else hard-delete.
        if !row.finalizers.is_empty() {
            let marked = sqlx::query_as::<_, ResourceRow>(
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
            .await?;
            tx.commit().await?;
            return Ok(DeleteOutcome::MarkedForDeletion(Box::new(marked)));
        }

        if row.kind == RESOURCE_DEFINITION_KIND {
            self.ensure_resource_definition_has_no_instances(&row, &mut tx)
                .await?;
            sqlx::query("DELETE FROM resource_store.resource_definitions WHERE uid = $1")
                .bind(uid)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("DELETE FROM resource_store.resources WHERE uid = $1")
            .bind(uid)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        Ok(DeleteOutcome::Deleted)
    }

    async fn try_collect(&self, uid: Uuid) -> Result<DeleteOutcome, StoreError> {
        let mut tx = self.pool.begin().await?;

        let row = sqlx::query_as::<_, ResourceRow>(
            "SELECT * FROM resource_store.resources WHERE uid = $1 FOR UPDATE",
        )
        .bind(uid)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(StoreError::NotFound)?;

        // Not tombstoned: nothing to collect, return current state.
        if row.deletion_timestamp.is_none() {
            return Ok(DeleteOutcome::MarkedForDeletion(Box::new(row)));
        }

        let child_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM resource_store.resources WHERE parent_uid = $1",
        )
        .bind(uid)
        .fetch_one(&mut *tx)
        .await?;

        if child_count > 0 {
            // Fan out: stamp any unmarked children. Ensure the cascade finalizer is present
            // (in case the row was tombstoned by some other path that didn't add it). Bump
            // revision on every row we mutate so observers see the change.
            sqlx::query(
                r#"
                UPDATE resource_store.resources
                SET deletion_timestamp = NOW(),
                    revision = revision + 1
                WHERE parent_uid = $1 AND deletion_timestamp IS NULL
                "#,
            )
            .bind(uid)
            .execute(&mut *tx)
            .await?;

            let row = sqlx::query_as::<_, ResourceRow>(
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
            .await?;
            tx.commit().await?;
            return Ok(DeleteOutcome::MarkedForDeletion(Box::new(row)));
        }

        // No children. Drop the cascade finalizer if present. Only bump revision when the
        // finalizer was actually removed so idempotent re-calls don't churn the version.
        let row = sqlx::query_as::<_, ResourceRow>(
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
        .await?;

        if !row.finalizers.is_empty() {
            tx.commit().await?;
            return Ok(DeleteOutcome::MarkedForDeletion(Box::new(row)));
        }

        // Clear: hard-delete.
        if row.kind == RESOURCE_DEFINITION_KIND {
            self.ensure_resource_definition_has_no_instances(&row, &mut tx)
                .await?;
            sqlx::query("DELETE FROM resource_store.resource_definitions WHERE uid = $1")
                .bind(uid)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("DELETE FROM resource_store.resources WHERE uid = $1")
            .bind(uid)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        Ok(DeleteOutcome::Deleted)
    }

    async fn list_pending_collection(&self, limit: i64) -> Result<Vec<ResourceRow>, StoreError> {
        let rows = sqlx::query_as::<_, ResourceRow>(
            r#"
            SELECT * FROM resource_store.resources
            WHERE deletion_timestamp IS NOT NULL
            ORDER BY deletion_timestamp ASC
            LIMIT $1
            "#,
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn resolve_path(&self, segments: &[PathSegment]) -> Result<Vec<ResourceRow>, StoreError> {
        if segments.is_empty() {
            return Err(StoreError::EmptyPath);
        }

        let mut tx = self.pool.begin().await?;
        let mut chain: Vec<ResourceRow> = Vec::with_capacity(segments.len());
        let mut current_parent: Option<Uuid> = None;

        for (idx, segment) in segments.iter().enumerate() {
            let row = match segment {
                PathSegment::Name {
                    api_versions,
                    kind,
                    name,
                } => {
                    let row: Option<ResourceRow> = match current_parent {
                        None => sqlx::query_as::<_, ResourceRow>(
                            "SELECT * FROM resource_store.resources WHERE api_version = ANY($1) AND kind = $2 AND name = $3 AND parent_uid IS NULL",
                        )
                        .bind(api_versions)
                        .bind(kind)
                        .bind(name)
                        .fetch_optional(&mut *tx)
                        .await?,
                        Some(pid) => sqlx::query_as::<_, ResourceRow>(
                            "SELECT * FROM resource_store.resources WHERE api_version = ANY($1) AND kind = $2 AND name = $3 AND parent_uid = $4",
                        )
                        .bind(api_versions)
                        .bind(kind)
                        .bind(name)
                        .bind(pid)
                        .fetch_optional(&mut *tx)
                        .await?,
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
                    let row: ResourceRow = match sqlx::query_as::<_, ResourceRow>(
                        "SELECT * FROM resource_store.resources WHERE uid = $1",
                    )
                    .bind(uid)
                    .fetch_optional(&mut *tx)
                    .await?
                    {
                        Some(r) => r,
                        None if idx + 1 == segments.len() => return Err(StoreError::NotFound),
                        None => return Err(StoreError::ParentNotFound),
                    };
                    if row.kind != *kind || !api_versions.iter().any(|v| v == &row.api_version) {
                        return Err(StoreError::KindMismatch {
                            expected: format!("one of {:?}/{kind}", api_versions),
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
            chain.push(row);
        }

        tx.commit().await?;
        Ok(chain)
    }

    async fn list_orphans(&self, parent_uid: Option<Uuid>) -> Result<Vec<ResourceRow>, StoreError> {
        let rows = match parent_uid {
            None => {
                sqlx::query_as::<_, ResourceRow>(
                    r#"
                SELECT c.*
                FROM resource_store.resources c
                JOIN resource_store.resources p ON c.parent_uid = p.uid
                WHERE p.deletion_timestamp IS NOT NULL
                ORDER BY c.kind, c.name
                "#,
                )
                .fetch_all(&self.pool)
                .await?
            }
            Some(pid) => {
                sqlx::query_as::<_, ResourceRow>(
                    r#"
                SELECT c.*
                FROM resource_store.resources c
                JOIN resource_store.resources p ON c.parent_uid = p.uid
                WHERE p.deletion_timestamp IS NOT NULL AND p.uid = $1
                ORDER BY c.kind, c.name
                "#,
                )
                .bind(pid)
                .fetch_all(&self.pool)
                .await?
            }
        };
        Ok(rows)
    }

    async fn list_unparented_versions(
        &self,
        api_versions: &[String],
        kind: &str,
    ) -> Result<Vec<ResourceRow>, StoreError> {
        let rows = sqlx::query_as::<_, ResourceRow>(
            r#"
            SELECT * FROM resource_store.resources
            WHERE api_version = ANY($1)
              AND kind = $2
              AND parent_uid IS NULL
            ORDER BY name
            "#,
        )
        .bind(api_versions)
        .bind(kind)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn reparent(
        &self,
        uid: Uuid,
        new_parent_uid: Option<Uuid>,
        scope: ResourceScope,
    ) -> Result<ResourceRow, StoreError> {
        let mut tx = self.pool.begin().await?;

        // Lock the target row.
        let target = sqlx::query_as::<_, ResourceRow>(
            "SELECT * FROM resource_store.resources WHERE uid = $1 FOR UPDATE",
        )
        .bind(uid)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(StoreError::NotFound)?;

        match scope {
            ResourceScope::Root => {
                if new_parent_uid.is_some() {
                    return Err(StoreError::Validation(
                        "root-scoped resources cannot be reparented under another resource"
                            .to_string(),
                    ));
                }
            }
            ResourceScope::Organization => {
                let Some(new_pid) = new_parent_uid else {
                    return Err(StoreError::Validation(
                        "organization-scoped resources must stay under an Organization".to_string(),
                    ));
                };

                let parent = sqlx::query_as::<_, ResourceRow>(
                    "SELECT * FROM resource_store.resources WHERE uid = $1",
                )
                .bind(new_pid)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(StoreError::ParentNotFound)?;

                if parent.kind != ORGANIZATION_KIND || parent.api_version != API_VERSION_V1ALPHA1 {
                    return Err(StoreError::Validation(format!(
                        "organization-scoped resources can only be reparented under a built-in \
                         Organization (rise.dev/v1alpha1/Organization), got '{}/{}'",
                        parent.api_version, parent.kind
                    )));
                }
                if parent.parent_uid.is_some() {
                    return Err(StoreError::Validation(
                        "organization-scoped resources can only be reparented under a root-level \
                         Organization"
                            .to_string(),
                    ));
                }
            }
        }

        if target.parent_uid == new_parent_uid {
            tx.commit().await?;
            return Ok(target);
        }

        if let Some(new_pid) = new_parent_uid {
            if new_pid == uid {
                return Err(StoreError::ReparentCycle);
            }

            // Verify the new parent exists and isn't an ancestor in our own subtree.
            // Walk up from the new parent; if we hit `uid`, it would be a cycle.
            let is_descendant: bool = sqlx::query_scalar(
                r#"
                WITH RECURSIVE ancestors AS (
                    SELECT uid, parent_uid FROM resource_store.resources WHERE uid = $1
                    UNION ALL
                    SELECT r.uid, r.parent_uid
                    FROM resource_store.resources r
                    JOIN ancestors a ON r.uid = a.parent_uid
                )
                SELECT EXISTS (SELECT 1 FROM ancestors WHERE uid = $2)
                "#,
            )
            .bind(new_pid)
            .bind(uid)
            .fetch_one(&mut *tx)
            .await?;

            if is_descendant {
                return Err(StoreError::ReparentCycle);
            }
        }

        let result = sqlx::query_as::<_, ResourceRow>(
            r#"
            UPDATE resource_store.resources
            SET parent_uid = $2,
                revision = revision + 1
            WHERE uid = $1
            RETURNING *
            "#,
        )
        .bind(uid)
        .bind(new_parent_uid)
        .fetch_one(&mut *tx)
        .await;

        let row = match result {
            Ok(r) => r,
            Err(e) if Self::is_name_conflict(&e) => return Err(StoreError::NameConflict),
            Err(e) if Self::is_discriminator_conflict(&e) => return Err(StoreError::NameConflict),
            Err(e) => return Err(StoreError::Database(e)),
        };

        tx.commit().await?;
        Ok(row)
    }

    async fn update_controller_status(
        &self,
        uid: Uuid,
        controller_id: &str,
        status_value: serde_json::Value,
    ) -> Result<ResourceRow, StoreError> {
        validate_controller_id(controller_id).map_err(|e| StoreError::Validation(e.to_string()))?;

        let row = sqlx::query_as::<_, ResourceRow>(
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
        .await?
        .ok_or(StoreError::NotFound)?;

        Ok(row)
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
        let mut tx = self.pool.begin().await?;

        let current = sqlx::query_as::<_, ResourceRow>(
            "SELECT * FROM resource_store.resources WHERE uid = $1 FOR UPDATE",
        )
        .bind(uid)
        .fetch_optional(&mut *tx)
        .await?
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

        let row = sqlx::query_as::<_, ResourceRow>(
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
        .await?;

        tx.commit().await?;

        Ok(row)
    }

    async fn resolve_collection(
        &self,
        collection: &str,
    ) -> Result<Option<CollectionInfo>, StoreError> {
        // Built-ins take priority
        if let Some(info) = Self::builtin_collection_info(collection) {
            return Ok(Some(info));
        }

        let row = sqlx::query(
            r#"
            SELECT rd.uid, rd.group_name, rd.kind, rd.scope, rd.versions,
                   rd.allowed_status_controller_ids, r.spec
            FROM resource_store.resource_definitions rd
            JOIN resource_store.resources r ON r.uid = rd.uid
            WHERE rd.plural = $1
            "#,
        )
        .bind(collection)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };

        let versions: Vec<rise_resource_api::ResourceDefinitionVersion> =
            serde_json::from_value(row.try_get("versions").map_err(StoreError::Database)?)
                .map_err(|e| {
                    StoreError::Validation(format!("invalid versions in ResourceDefinition: {e}"))
                })?;

        let group_name: String = row.try_get("group_name").map_err(StoreError::Database)?;
        let storage_version = versions
            .iter()
            .find(|v| v.storage)
            .map(|v| v.name.clone())
            .unwrap_or_else(|| "v1".to_string());

        self.resolve_collection_version(&group_name, &storage_version, collection)
            .await
    }

    async fn resolve_collection_version(
        &self,
        group: &str,
        version: &str,
        collection: &str,
    ) -> Result<Option<CollectionInfo>, StoreError> {
        if group == "rise.dev" && version == "v1alpha1" {
            return Ok(Self::builtin_collection_info(collection));
        }

        let cache_key = format!("{group}/{version}/{collection}");
        let cached_validator = self
            .schema_cache
            .read()
            .ok()
            .and_then(|c| c.get(&cache_key).cloned());

        let row = sqlx::query(
            r#"
            SELECT rd.uid, rd.group_name, rd.kind, rd.scope, rd.versions,
                   rd.allowed_status_controller_ids, r.spec
            FROM resource_store.resource_definitions rd
            JOIN resource_store.resources r ON r.uid = rd.uid
            WHERE rd.group_name = $1 AND rd.plural = $2
            "#,
        )
        .bind(group)
        .bind(collection)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };

        let spec: ResourceDefinitionSpec = serde_json::from_value(
            row.try_get("spec").map_err(StoreError::Database)?,
        )
        .map_err(|e| StoreError::Validation(format!("invalid ResourceDefinition spec: {e}")))?;

        let versions = spec.versions.clone();
        let requested = versions
            .iter()
            .find(|v| v.name == version)
            .ok_or_else(|| StoreError::Validation(format!("version '{version}' is not defined")))?;
        if !requested.served {
            return Err(StoreError::Validation(format!(
                "version '{version}' is not served"
            )));
        }

        let storage_version = versions
            .iter()
            .find(|v| v.storage)
            .map(|v| v.name.clone())
            .unwrap_or_else(|| "v1".to_string());
        let storage_api_version = Self::api_version(group, &storage_version);
        let served_api_versions: Vec<String> = versions
            .iter()
            .filter(|v| v.served)
            .map(|v| Self::api_version(group, &v.name))
            .collect();
        let api_version = Self::api_version(group, version);

        let allowed: Vec<String> = row
            .try_get("allowed_status_controller_ids")
            .map_err(StoreError::Database)?;

        let scope: ResourceScope = serde_json::from_value(
            row.try_get("scope").map_err(StoreError::Database)?,
        )
        .map_err(|e| StoreError::Validation(format!("invalid scope in ResourceDefinition: {e}")))?;
        let kind: String = row.try_get("kind").map_err(StoreError::Database)?;

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
            kind,
            scope,
            parent: spec.parent,
            spec_validator: schema_validator,
            allowed_status_controller_ids: allowed,
        }))
    }

    async fn register_resource_definition(
        &self,
        params: CreateResourceParams,
    ) -> Result<ResourceRow, StoreError> {
        // Validate spec format and reserved-name rules first so plural/group are sound.
        ResourceDefinitionValidator.validate_spec(&params.spec)?;

        // Parse once; safe to unwrap because validate_spec succeeded above
        let spec: ResourceDefinitionSpec = serde_json::from_value(params.spec.clone())
            .expect("spec parseable: ResourceDefinitionValidator.validate_spec succeeded");

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

        let mut tx = self.pool.begin().await?;

        let metadata = serde_json::to_value(&params.annotations).unwrap_or_default();

        let resource_row = Self::insert_resource_row_with_retry(&mut tx, &params, metadata).await?;

        // Insert into the resource_definitions projection table
        let scope_val = serde_json::to_value(&spec.scope).unwrap_or_default();
        let versions_val = serde_json::to_value(&spec.versions).unwrap_or_default();

        sqlx::query(
            r#"
            INSERT INTO resource_store.resource_definitions
                (uid, group_name, kind, plural, scope, versions, allowed_status_controller_ids)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            "#,
        )
        .bind(resource_row.uid)
        .bind(&spec.group)
        .bind(&spec.kind)
        .bind(&spec.plural)
        .bind(scope_val)
        .bind(versions_val)
        .bind(&spec.allowed_status_controller_ids)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            if let sqlx::Error::Database(ref db) = e {
                let constraint = db.constraint().unwrap_or("");
                if constraint == "resource_definitions_plural_unique" {
                    return StoreError::Validation(format!(
                        "a ResourceDefinition with plural '{}' already exists",
                        spec.plural
                    ));
                }
                if constraint == "resource_definitions_group_kind_unique" {
                    return StoreError::Validation(format!(
                        "a ResourceDefinition for group '{}' kind '{}' already exists",
                        spec.group, spec.kind
                    ));
                }
            }
            StoreError::Database(e)
        })?;

        tx.commit().await?;

        self.invalidate_schema_cache(&spec.group, &spec.plural);

        Ok(resource_row)
    }

    async fn update_resource_definition(
        &self,
        uid: Uuid,
        params: UpdateResourceParams,
    ) -> Result<ResourceRow, StoreError> {
        // Validate the new spec
        ResourceDefinitionValidator.validate_spec(&params.spec)?;

        let new_spec: ResourceDefinitionSpec = serde_json::from_value(params.spec.clone())
            .expect("spec parseable: ResourceDefinitionValidator.validate_spec succeeded");

        // Validate every declared JSON schema compiles. Symmetric with register.
        Self::validate_version_schemas(&new_spec, "")?;

        let mut tx = self.pool.begin().await?;

        // Lock the row and fetch current state
        let current = sqlx::query_as::<_, ResourceRow>(
            "SELECT * FROM resource_store.resources WHERE uid = $1 FOR UPDATE",
        )
        .bind(uid)
        .fetch_optional(&mut *tx)
        .await?
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
            || new_spec.scope != old_spec.scope
            || new_spec.parent != old_spec.parent
        {
            return Err(StoreError::Validation(
                "ResourceDefinition identity fields (group, kind, plural, scope, parent) are immutable"
                    .to_string(),
            ));
        }

        let metadata = serde_json::to_value(&params.annotations).unwrap_or_default();

        // Update the resources row with optimistic concurrency
        let updated = sqlx::query_as::<_, ResourceRow>(
            r#"
            UPDATE resource_store.resources
            SET api_version = COALESCE($1, api_version),
                metadata   = $2,
                spec       = $3,
                finalizers = $4,
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
        .fetch_optional(&mut *tx)
        .await?;

        let row = match updated {
            Some(r) => r,
            None => {
                return Err(StoreError::RevisionConflict {
                    expected: params.revision,
                    found: current.revision,
                })
            }
        };

        // Sync the projection table (mutable fields only: versions and allowed controllers)
        let versions_val = serde_json::to_value(&new_spec.versions).unwrap_or_default();
        sqlx::query(
            r#"
            UPDATE resource_store.resource_definitions
            SET versions = $1, allowed_status_controller_ids = $2
            WHERE uid = $3
            "#,
        )
        .bind(versions_val)
        .bind(&new_spec.allowed_status_controller_ids)
        .bind(uid)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        self.invalidate_schema_cache(&new_spec.group, &new_spec.plural);

        Ok(row)
    }
}

fn is_controller_finalizer(finalizer: &str, controller_id: &str) -> bool {
    finalizer == controller_id || finalizer.starts_with(&format!("{controller_id}/"))
}
