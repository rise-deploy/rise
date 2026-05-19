use std::sync::Arc;
use uuid::Uuid;

use rise_resource_api::{
    validate_controller_id, ResourceDefinitionSpec, ResourceScope, API_VERSION_V1ALPHA1,
    ORGANIZATION_COLLECTION, ORGANIZATION_KIND, RESOURCE_DEFINITION_COLLECTION,
    RESOURCE_DEFINITION_KIND,
};
use sqlx::PgPool;

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
}

impl PgResourceStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
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
}

#[async_trait::async_trait]
impl ResourceStore for PgResourceStore {
    async fn create(&self, params: CreateResourceParams) -> Result<ResourceRow, StoreError> {
        rise_resource_api::validate_resource_name(&params.name)
            .map_err(|e| StoreError::Validation(e.to_string()))?;

        if let Some(v) = &params.validator {
            v.validate_spec(&params.spec)?;
        }

        let metadata = serde_json::to_value(&params.annotations).unwrap_or_default();

        // Retry the INSERT up to 10 times, generating a new discriminator on each
        // discriminator collision. This handles concurrent creators racing to the same
        // randomly generated discriminator without a pre-check TOCTOU window.
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
            .fetch_one(&self.pool)
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

        // Zero rows affected — determine why so we can return the right error
        match self.get(uid).await? {
            None => Err(StoreError::NotFound),
            Some(current) => Err(StoreError::RevisionConflict {
                expected: params.revision,
                found: current.revision,
            }),
        }
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

        let current = self.get(uid).await?.ok_or(StoreError::NotFound)?;

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
        .fetch_one(&self.pool)
        .await?;

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

        // Look up external ResourceDefinitions
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

        // Build a pre-compiled JSON schema validator from the storage version's schema
        let schema_validator: Arc<dyn SpecValidator> = versions
            .iter()
            .find(|v| v.storage)
            .and_then(|v| v.schema.clone())
            .map(|s| {
                JsonSchemaValidator::new(s)
                    .map(|v| Arc::new(v) as Arc<dyn SpecValidator>)
                    .unwrap_or_else(|_| Arc::new(NoOpValidator))
            })
            .unwrap_or_else(|| Arc::new(NoOpValidator));

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
        // Validate the spec first (checks reserved names, format, etc.)
        let validator = Arc::new(ResourceDefinitionValidator);
        validator.validate_spec(&params.spec)?;

        let spec: ResourceDefinitionSpec = serde_json::from_value(params.spec.clone())
            .map_err(|e| StoreError::Validation(format!("invalid ResourceDefinition spec: {e}")))?;

        let mut tx = self.pool.begin().await?;

        let metadata = serde_json::to_value(&params.annotations).unwrap_or_default();

        // Retry INSERT up to 10 times on discriminator collision (same as create())
        let resource_row = 'retry: {
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
                .fetch_one(&mut *tx)
                .await;

                match result {
                    Ok(row) => break 'retry row,
                    Err(ref e) if Self::is_name_conflict(e) => {
                        return Err(StoreError::NameConflict)
                    }
                    Err(ref e) if Self::is_discriminator_conflict(e) => continue,
                    Err(e) => return Err(StoreError::Database(e)),
                }
            }
            return Err(StoreError::DiscriminatorExhausted);
        };

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

        Ok(resource_row)
    }
}

fn is_controller_finalizer(finalizer: &str, controller_id: &str) -> bool {
    finalizer == controller_id || finalizer.starts_with(&format!("{controller_id}/"))
}

// Helpers to extract typed values from a sqlx::postgres::PgRow
trait PgRowExt {
    fn try_get<'r, T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>>(
        &'r self,
        column: &str,
    ) -> Result<T, sqlx::Error>;
}

impl PgRowExt for sqlx::postgres::PgRow {
    fn try_get<'r, T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>>(
        &'r self,
        column: &str,
    ) -> Result<T, sqlx::Error> {
        sqlx::Row::try_get(self, column)
    }
}
