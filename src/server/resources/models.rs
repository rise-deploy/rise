//! Wire types for the generic resource HTTP API.
//!
//! Request bodies (`CreateResourceRequest`, `UpdateResourceRequest`) come from
//! `rise_resource_api`; this module holds the response envelope, the small
//! per-endpoint payloads (status, finalizers), and the conversion from a
//! stored `ResourceRow` to the wire `Resource` envelope.

use std::collections::BTreeMap;

use rise_resource_api::{Resource, ResourceMetadata};
use rise_resource_store::ResourceRow;
use serde::{Deserialize, Serialize};

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
    row_to_resource_with_api_version(row, &row.api_version)
}

pub fn row_to_resource_with_api_version(row: &ResourceRow, api_version: &str) -> Resource {
    let annotations: BTreeMap<String, String> =
        match serde_json::from_value::<BTreeMap<String, serde_json::Value>>(row.metadata.clone()) {
            Ok(map) => map
                .into_iter()
                .map(|(k, v)| match v {
                    serde_json::Value::String(s) => (k, s),
                    // Non-string annotation values are preserved as their JSON
                    // representation so that round-trip GET → PUT does not silently
                    // erase them.  The conversion is lossless: `42` → `"42"`,
                    // `{"x":1}` → `"{\"x\":1}"`.
                    other => {
                        tracing::debug!(
                            uid = %row.uid,
                            kind = %row.kind,
                            name = %row.name,
                            key = %k,
                            value = %other,
                            "annotation value is not a string — serialising to JSON string"
                        );
                        (k, other.to_string())
                    }
                })
                .collect(),
            Err(e) => {
                tracing::warn!(
                    uid = %row.uid,
                    kind = %row.kind,
                    name = %row.name,
                    error = %e,
                    "resource metadata is not a JSON object — annotations will be empty"
                );
                BTreeMap::new()
            }
        };

    let spec: BTreeMap<String, serde_json::Value> = match &row.spec {
        serde_json::Value::Object(map) => map.clone().into_iter().collect(),
        _ => BTreeMap::new(),
    };
    let status: BTreeMap<String, serde_json::Value> = match &row.status {
        serde_json::Value::Object(map) => map.clone().into_iter().collect(),
        _ => BTreeMap::new(),
    };

    Resource {
        api_version: api_version.to_string(),
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
    use uuid::Uuid;

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
    fn row_to_resource_preserves_non_string_annotations_as_json_strings() {
        // Annotations written by external tools may store numbers, booleans, or
        // objects.  They must survive a GET → PUT round-trip rather than being
        // silently dropped.
        let mut r = row(json!({}), json!({}));
        r.metadata = json!({
            "count": 42,
            "flag": true,
            "nested": {"x": 1},
            "normal": "hello"
        });
        let resource = row_to_resource(&r);
        let ann = &resource.metadata.annotations;
        assert_eq!(ann.get("count").map(String::as_str), Some("42"));
        assert_eq!(ann.get("flag").map(String::as_str), Some("true"));
        assert_eq!(ann.get("nested").map(String::as_str), Some("{\"x\":1}"));
        assert_eq!(ann.get("normal").map(String::as_str), Some("hello"));
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
