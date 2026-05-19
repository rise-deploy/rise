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
    CollectionInfo, CreateResourceParams, DeleteOutcome, ResourceStore, UpdateResourceParams,
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
                kind: ORGANIZATION_KIND.to_string(),
                scope: ResourceScope::Root,
                spec_validator: Arc::new(OrganizationValidator),
                allowed_status_controller_ids: vec![],
            }),
            RESOURCE_DEFINITION_COLLECTION => Some(CollectionInfo {
                api_version: API_VERSION_V1ALPHA1.to_string(),
                kind: RESOURCE_DEFINITION_KIND.to_string(),
                scope: ResourceScope::Root,
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
                INSERT INTO resources
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
}

#[async_trait::async_trait]
impl ResourceStore for PgResourceStore {
    async fn create(&self, params: CreateResourceParams) -> Result<ResourceRow, StoreError> {
        validate_resource_name(&params.name).map_err(|e| StoreError::Validation(e.to_string()))?;

        if let Some(v) = &params.validator {
            v.validate_spec(&params.spec)?;
        }

        let metadata = serde_json::to_value(&params.annotations).unwrap_or_default();

        let mut conn = self.pool.acquire().await.map_err(StoreError::Database)?;
        Self::insert_resource_row_with_retry(&mut conn, &params, metadata).await
    }

    async fn get(&self, uid: Uuid) -> Result<Option<ResourceRow>, StoreError> {
        let row = sqlx::query_as::<_, ResourceRow>("SELECT * FROM resources WHERE uid = $1")
            .bind(uid)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn get_by_name(
        &self,
        kind: &str,
        name: &str,
        parent_uid: Option<Uuid>,
    ) -> Result<Option<ResourceRow>, StoreError> {
        let row =
            match parent_uid {
                None => sqlx::query_as::<_, ResourceRow>(
                    "SELECT * FROM resources WHERE kind = $1 AND name = $2 AND parent_uid IS NULL",
                )
                .bind(kind)
                .bind(name)
                .fetch_optional(&self.pool)
                .await?,
                Some(pid) => {
                    sqlx::query_as::<_, ResourceRow>(
                        "SELECT * FROM resources WHERE kind = $1 AND name = $2 AND parent_uid = $3",
                    )
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
        kind: &str,
        parent_uid: Option<Uuid>,
    ) -> Result<Vec<ResourceRow>, StoreError> {
        let rows =
            match parent_uid {
                None => sqlx::query_as::<_, ResourceRow>(
                    "SELECT * FROM resources WHERE kind = $1 AND parent_uid IS NULL ORDER BY name",
                )
                .bind(kind)
                .fetch_all(&self.pool)
                .await?,
                Some(pid) => {
                    sqlx::query_as::<_, ResourceRow>(
                        "SELECT * FROM resources WHERE kind = $1 AND parent_uid = $2 ORDER BY name",
                    )
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
        let kind: Option<String> = sqlx::query_scalar("SELECT kind FROM resources WHERE uid = $1")
            .bind(uid)
            .fetch_optional(&self.pool)
            .await?;
        match kind.as_deref() {
            None => return Err(StoreError::NotFound),
            Some(RESOURCE_DEFINITION_KIND) => {
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
        let updated = sqlx::query_as::<_, ResourceRow>(
            r#"
            UPDATE resources
            SET metadata   = $1,
                spec       = $2,
                finalizers = $3,
                revision   = revision + 1,
                updated_at = NOW()
            WHERE uid = $4 AND revision = $5
            RETURNING *
            "#,
        )
        .bind(metadata)
        .bind(&params.spec)
        .bind(&params.finalizers)
        .bind(uid)
        .bind(params.revision)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(row) = updated {
            return Ok(row);
        }

        // Zero rows affected — revision mismatch (NotFound already handled above)
        let current = self.get(uid).await?.ok_or(StoreError::NotFound)?;
        Err(StoreError::RevisionConflict {
            expected: params.revision,
            found: current.revision,
        })
    }

    async fn delete(&self, uid: Uuid) -> Result<DeleteOutcome, StoreError> {
        let row = self.get(uid).await?.ok_or(StoreError::NotFound)?;

        // Reject if children exist in the resources table
        let child_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM resources WHERE parent_uid = $1")
                .bind(uid)
                .fetch_one(&self.pool)
                .await?;
        if child_count > 0 {
            return Err(StoreError::HasChildren);
        }

        // Finalizers present → mark for deletion instead of deleting
        if !row.finalizers.is_empty() {
            let marked = sqlx::query_as::<_, ResourceRow>(
                r#"
                UPDATE resources
                SET deletion_timestamp = NOW(), updated_at = NOW()
                WHERE uid = $1
                RETURNING *
                "#,
            )
            .bind(uid)
            .fetch_one(&self.pool)
            .await?;
            return Ok(DeleteOutcome::MarkedForDeletion(Box::new(marked)));
        }

        // Hard delete in a transaction. Delete from the resource_definitions projection
        // table first (ON DELETE RESTRICT prevents deleting the resources row otherwise).
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM resource_definitions WHERE uid = $1")
            .bind(uid)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM resources WHERE uid = $1")
            .bind(uid)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        Ok(DeleteOutcome::Deleted)
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
            UPDATE resources
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

        // All modified finalizers must be prefixed by this controller's ID
        for f in add.iter().chain(remove.iter()) {
            if !is_controller_finalizer(f, controller_id) {
                return Err(StoreError::Validation(format!(
                    "finalizer '{f}' is not owned by controller '{controller_id}'"
                )));
            }
        }

        // Use SELECT FOR UPDATE inside a transaction to serialise concurrent finalizer
        // mutations from different controllers on the same resource, preventing lost updates.
        let mut tx = self.pool.begin().await?;

        let current =
            sqlx::query_as::<_, ResourceRow>("SELECT * FROM resources WHERE uid = $1 FOR UPDATE")
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
            UPDATE resources
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

        // Check for a pre-compiled validator to avoid re-compiling JSON schema on every call
        let cached_validator = self
            .schema_cache
            .read()
            .ok()
            .and_then(|c| c.get(collection).cloned());

        let row = sqlx::query(
            r#"
            SELECT rd.uid, rd.group_name, rd.kind, rd.scope, rd.versions,
                   rd.allowed_status_controller_ids
            FROM resource_definitions rd
            WHERE rd.plural = $1
            "#,
        )
        .bind(collection)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };

        let scope: ResourceScope = serde_json::from_value(
            row.try_get("scope").map_err(StoreError::Database)?,
        )
        .map_err(|e| StoreError::Validation(format!("invalid scope in ResourceDefinition: {e}")))?;

        let versions: Vec<rise_resource_api::ResourceDefinitionVersion> =
            serde_json::from_value(row.try_get("versions").map_err(StoreError::Database)?)
                .map_err(|e| {
                    StoreError::Validation(format!("invalid versions in ResourceDefinition: {e}"))
                })?;

        let allowed: Vec<String> = row
            .try_get("allowed_status_controller_ids")
            .map_err(StoreError::Database)?;

        let group_name: String = row.try_get("group_name").map_err(StoreError::Database)?;
        let kind: String = row.try_get("kind").map_err(StoreError::Database)?;

        let storage_version = versions
            .iter()
            .find(|v| v.storage)
            .map(|v| v.name.clone())
            .unwrap_or_else(|| "v1".to_string());

        let api_version = format!("{group_name}/{storage_version}");

        // Use the cached validator, or compile one and store it in the cache
        let schema_validator: Arc<dyn SpecValidator> = match cached_validator {
            Some(v) => v,
            None => {
                let v: Arc<dyn SpecValidator> = versions
                    .iter()
                    .find(|v| v.storage)
                    .and_then(|v| v.schema.clone())
                    .map(|s| {
                        JsonSchemaValidator::new(s)
                            .map(|v| Arc::new(v) as Arc<dyn SpecValidator>)
                            .unwrap_or_else(|_| Arc::new(NoOpValidator))
                    })
                    .unwrap_or_else(|| Arc::new(NoOpValidator));
                if let Ok(mut cache) = self.schema_cache.write() {
                    cache.insert(collection.to_string(), v.clone());
                }
                v
            }
        };

        Ok(Some(CollectionInfo {
            api_version,
            kind,
            scope,
            spec_validator: schema_validator,
            allowed_status_controller_ids: allowed,
        }))
    }

    async fn register_resource_definition(
        &self,
        params: CreateResourceParams,
    ) -> Result<ResourceRow, StoreError> {
        // ResourceDefinition names follow the {plural}.{group} convention so dots are allowed,
        // unlike instance resource names. Reject empty segments (e.g. "widgets..dev").
        if params.name.is_empty()
            || params.name.starts_with('.')
            || params.name.ends_with('.')
            || params.name.contains("..")
        {
            return Err(StoreError::Validation(
                "ResourceDefinition name must be a non-empty dot-separated string with no empty segments"
                    .to_string(),
            ));
        }

        // Validate spec format and reserved-name rules
        ResourceDefinitionValidator.validate_spec(&params.spec)?;

        // Parse once; safe to unwrap because validate_spec succeeded above
        let spec: ResourceDefinitionSpec = serde_json::from_value(params.spec.clone())
            .expect("spec parseable: ResourceDefinitionValidator.validate_spec succeeded");

        let mut tx = self.pool.begin().await?;

        let metadata = serde_json::to_value(&params.annotations).unwrap_or_default();

        let resource_row = Self::insert_resource_row_with_retry(&mut tx, &params, metadata).await?;

        // Insert into the resource_definitions projection table
        let scope_val = serde_json::to_value(&spec.scope).unwrap_or_default();
        let versions_val = serde_json::to_value(&spec.versions).unwrap_or_default();

        sqlx::query(
            r#"
            INSERT INTO resource_definitions
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

        // Evict any stale cached validator for this plural
        if let Ok(mut cache) = self.schema_cache.write() {
            cache.remove(&spec.plural);
        }

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

        let mut tx = self.pool.begin().await?;

        // Lock the row and fetch current state
        let current =
            sqlx::query_as::<_, ResourceRow>("SELECT * FROM resources WHERE uid = $1 FOR UPDATE")
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
        {
            return Err(StoreError::Validation(
                "ResourceDefinition identity fields (group, kind, plural, scope) are immutable"
                    .to_string(),
            ));
        }

        let metadata = serde_json::to_value(&params.annotations).unwrap_or_default();

        // Update the resources row with optimistic concurrency
        let updated = sqlx::query_as::<_, ResourceRow>(
            r#"
            UPDATE resources
            SET metadata   = $1,
                spec       = $2,
                finalizers = $3,
                revision   = revision + 1,
                updated_at = NOW()
            WHERE uid = $4 AND revision = $5
            RETURNING *
            "#,
        )
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
            UPDATE resource_definitions
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

        // Evict stale cached validator for this collection
        if let Ok(mut cache) = self.schema_cache.write() {
            cache.remove(&new_spec.plural);
        }

        Ok(row)
    }
}

fn is_controller_finalizer(finalizer: &str, controller_id: &str) -> bool {
    finalizer == controller_id || finalizer.starts_with(&format!("{controller_id}/"))
}
