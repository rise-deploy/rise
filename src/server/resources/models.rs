//! Wire types for the generic resource HTTP API.
//!
//! Request bodies (`CreateResourceRequest`, `UpdateResourceRequest`) come from
//! `rise_resource_api`; this module holds the response envelope, the small
//! per-endpoint payloads (status, finalizers, reparent), and the conversion
//! from a stored `ResourceRow` to the wire `Resource` envelope.

use std::collections::BTreeMap;

use rise_resource_api::{Resource, ResourceMetadata};
use rise_resource_store::ResourceRow;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Outcome of a controller status update — what was previously
/// `status.controllers[<id>]` is replaced with the request body.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ControllerStatusUpdate {
    /// New status payload for this controller's slot under `status.controllers`.
    pub status: serde_json::Value,
}

/// Body of a controller finalizer update — adds and removes are applied in a
/// single transaction. Both lists may be empty.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ControllerFinalizerUpdate {
    #[serde(default)]
    pub add: Vec<String>,
    #[serde(default)]
    pub remove: Vec<String>,
}

/// Body of a reparent request. `new_parent_uid = None` reparents to root.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReparentRequest {
    /// New parent UID, or null to move to root scope.
    #[serde(default)]
    pub new_parent_uid: Option<Uuid>,
}

/// Wrapper for the generic-resource list endpoint response.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceList {
    pub api_version: String,
    pub kind: String,
    pub items: Vec<Resource>,
}

/// Convert a stored row into the wire envelope.
///
/// We don't use `ResourceRow::to_resource` here because that helper requires
/// typed spec/status. The generic API returns the raw JSON values verbatim, so
/// callers can round-trip arbitrary external resources.
pub fn row_to_resource(row: &ResourceRow) -> Resource {
    let annotations: BTreeMap<String, String> = serde_json::from_value(row.metadata.clone())
        .unwrap_or_else(|e| {
            tracing::warn!(
                uid = %row.uid,
                kind = %row.kind,
                name = %row.name,
                error = %e,
                "resource metadata is not a string map — annotations will be empty"
            );
            BTreeMap::new()
        });

    let spec: BTreeMap<String, serde_json::Value> = match &row.spec {
        serde_json::Value::Object(map) => map.clone().into_iter().collect(),
        _ => BTreeMap::new(),
    };
    let status: BTreeMap<String, serde_json::Value> = match &row.status {
        serde_json::Value::Object(map) => map.clone().into_iter().collect(),
        _ => BTreeMap::new(),
    };

    Resource {
        api_version: row.api_version.clone(),
        kind: row.kind.clone(),
        metadata: ResourceMetadata {
            name: row.name.clone(),
            uid: Some(row.uid),
            revision: Some(row.revision),
            discriminator: Some(row.discriminator.clone()),
            annotations,
            finalizers: row.finalizers.clone(),
            deletion_timestamp: row.deletion_timestamp,
        },
        spec,
        status,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;

    fn row(spec: serde_json::Value, status: serde_json::Value) -> ResourceRow {
        ResourceRow {
            uid: Uuid::new_v4(),
            api_version: "rise.dev/v1alpha1".into(),
            kind: "Organization".into(),
            parent_uid: None,
            name: "acme".into(),
            discriminator: "abcd1234".into(),
            metadata: json!({"team": "platform"}),
            spec,
            status,
            revision: 3,
            finalizers: vec!["controller.example.com/cleanup".into()],
            deletion_timestamp: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn row_to_resource_round_trips_spec_and_status() {
        let r = row(json!({"displayName": "Acme"}), json!({"controllers": {}}));
        let resource = row_to_resource(&r);

        assert_eq!(resource.api_version, "rise.dev/v1alpha1");
        assert_eq!(resource.kind, "Organization");
        assert_eq!(resource.metadata.name, "acme");
        assert_eq!(resource.metadata.revision, Some(3));
        assert_eq!(resource.metadata.discriminator.as_deref(), Some("abcd1234"));
        assert_eq!(
            resource
                .metadata
                .annotations
                .get("team")
                .map(String::as_str),
            Some("platform")
        );
        assert_eq!(resource.spec.get("displayName"), Some(&json!("Acme")));
        assert!(resource.status.contains_key("controllers"));
    }

    #[test]
    fn row_to_resource_treats_non_object_spec_as_empty() {
        // The DB-level CHECK constraint forbids non-object spec/status, so this
        // is a defensive fallback rather than a regular code path.
        let r = row(json!("not an object"), json!(null));
        let resource = row_to_resource(&r);
        assert!(resource.spec.is_empty());
        assert!(resource.status.is_empty());
    }
}
