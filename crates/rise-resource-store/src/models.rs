use chrono::{DateTime, Utc};
use rise_resource_api::{Resource, ResourceMetadata};
use serde::de::DeserializeOwned;
use std::collections::BTreeMap;
use uuid::Uuid;

use crate::error::StoreError;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ResourceRow {
    pub uid: Uuid,
    pub api_version: String,
    pub kind: String,
    pub parent_uid: Option<Uuid>,
    pub name: String,
    pub discriminator: String,
    pub metadata: serde_json::Value,
    pub spec: serde_json::Value,
    pub status: serde_json::Value,
    pub revision: i64,
    pub finalizers: Vec<String>,
    pub deletion_timestamp: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl ResourceRow {
    pub fn to_resource<TSpec, TStatus>(&self) -> Result<Resource<TSpec, TStatus>, StoreError>
    where
        TSpec: Default + DeserializeOwned,
        TStatus: Default + DeserializeOwned,
    {
        let spec: TSpec = serde_json::from_value(self.spec.clone())
            .map_err(|e| StoreError::Validation(format!("invalid spec: {e}")))?;
        let status: TStatus = serde_json::from_value(self.status.clone())
            .map_err(|e| StoreError::Validation(format!("invalid status: {e}")))?;

        let annotations: BTreeMap<String, String> =
            serde_json::from_value(self.metadata.clone()).unwrap_or_default();

        Ok(Resource {
            api_version: self.api_version.clone(),
            kind: self.kind.clone(),
            metadata: ResourceMetadata {
                name: self.name.clone(),
                uid: Some(self.uid),
                revision: Some(self.revision),
                discriminator: Some(self.discriminator.clone()),
                annotations,
                finalizers: self.finalizers.clone(),
                deletion_timestamp: self.deletion_timestamp,
            },
            spec,
            status,
        })
    }
}
