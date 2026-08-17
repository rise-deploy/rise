use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use uuid::Uuid;

use crate::{OwnerReference, Resource, ResourceKind, ResourceMetadata, ValidationError};

/// Storage-neutral row returned by [`crate::ResourceStore`].
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceRow {
    pub uid: Uuid,
    pub api_version: String,
    pub kind: String,
    pub parent_uid: Option<Uuid>,
    pub name: String,
    pub discriminator: String,
    pub labels: BTreeMap<String, String>,
    pub metadata: serde_json::Value,
    pub spec: serde_json::Value,
    pub status: serde_json::Value,
    pub revision: i64,
    pub finalizers: Vec<String>,
    pub owner_references: Vec<OwnerReference>,
    pub deletion_timestamp: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl ResourceRow {
    /// Normalize this row's versioned storage identity to its canonical,
    /// version-independent kind.
    pub fn resource_kind(&self) -> Result<ResourceKind, ValidationError> {
        ResourceKind::from_api_route(&self.api_version, &self.kind)
    }

    /// Convert the storage-neutral row into a typed API resource.
    ///
    /// This is a pure representation conversion, so malformed stored JSON is
    /// reported directly as [`ValidationError`].
    pub fn to_resource<TSpec, TStatus>(&self) -> Result<Resource<TSpec, TStatus>, ValidationError>
    where
        TSpec: Default + DeserializeOwned,
        TStatus: Default + DeserializeOwned,
    {
        let spec = serde_json::from_value(self.spec.clone())
            .map_err(|error| ValidationError::new(format!("invalid spec: {error}")))?;
        let status = serde_json::from_value(self.status.clone())
            .map_err(|error| ValidationError::new(format!("invalid status: {error}")))?;
        let annotations: BTreeMap<String, String> =
            serde_json::from_value(self.metadata.clone())
                .map_err(|error| ValidationError::new(format!("invalid metadata: {error}")))?;

        Ok(Resource {
            api_version: self.api_version.clone(),
            kind: self.kind.clone(),
            metadata: ResourceMetadata {
                name: self.name.clone(),
                uid: Some(self.uid),
                revision: Some(self.revision),
                discriminator: Some(self.discriminator.clone()),
                labels: self.labels.clone(),
                annotations,
                finalizers: self.finalizers.clone(),
                owner_references: self.owner_references.clone(),
                deletion_timestamp: self.deletion_timestamp,
            },
            spec,
            status,
        })
    }
}
