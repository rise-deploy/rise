use chrono::{DateTime, Utc};
use rise_resource_api::{OwnerReference, ResourceRow};
use uuid::Uuid;

/// SQLX-only representation of a `resources` row. The public contract uses the
/// storage-neutral `rise_resource_api::ResourceRow` and never depends on SQLX.
#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct PgResourceRow {
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
    pub owner_references: sqlx::types::Json<Vec<OwnerReference>>,
    pub deletion_timestamp: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<PgResourceRow> for ResourceRow {
    fn from(row: PgResourceRow) -> Self {
        Self {
            uid: row.uid,
            api_version: row.api_version,
            kind: row.kind,
            parent_uid: row.parent_uid,
            name: row.name,
            discriminator: row.discriminator,
            metadata: row.metadata,
            spec: row.spec,
            status: row.status,
            revision: row.revision,
            finalizers: row.finalizers,
            owner_references: row.owner_references.0,
            deletion_timestamp: row.deletion_timestamp,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn pg_row_conversion_maps_every_field() {
        let created_at = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let updated_at = Utc.timestamp_opt(1_700_000_100, 0).unwrap();
        let deletion_timestamp = Some(Utc.timestamp_opt(1_700_000_200, 0).unwrap());
        let uid = Uuid::from_u128(42);
        let db = PgResourceRow {
            uid,
            api_version: "example.dev/v1".into(),
            kind: "Widget".into(),
            parent_uid: Some(Uuid::from_u128(7)),
            name: "widget-a".into(),
            discriminator: "abc123de".into(),
            metadata: serde_json::json!({"owner": "team-a"}),
            spec: serde_json::json!({"size": 3}),
            status: serde_json::json!({"ready": true}),
            revision: 9,
            finalizers: vec!["example.dev/cleanup".into()],
            owner_references: sqlx::types::Json(vec![]),
            deletion_timestamp,
            created_at,
            updated_at,
        };

        let api: ResourceRow = db.into();
        assert_eq!(api.uid, uid);
        assert_eq!(api.api_version, "example.dev/v1");
        assert_eq!(api.kind, "Widget");
        assert_eq!(api.parent_uid, Some(Uuid::from_u128(7)));
        assert_eq!(api.name, "widget-a");
        assert_eq!(api.discriminator, "abc123de");
        assert_eq!(api.metadata, serde_json::json!({"owner": "team-a"}));
        assert_eq!(api.spec, serde_json::json!({"size": 3}));
        assert_eq!(api.status, serde_json::json!({"ready": true}));
        assert_eq!(api.revision, 9);
        assert_eq!(api.finalizers, vec!["example.dev/cleanup"]);
        assert!(api.owner_references.is_empty());
        assert_eq!(api.deletion_timestamp, deletion_timestamp);
        assert_eq!(api.created_at, created_at);
        assert_eq!(api.updated_at, updated_at);
    }
}
