//! Wire types for the generic resource HTTP API.
//!
//! Request bodies (`CreateResourceRequest`, `UpdateResourceRequest`) come from
//! `rise_resource_api`; this module holds the response envelope, the small
//! per-endpoint payloads (status, finalizers), and the conversion from a
//! stored `ResourceRow` to the wire `Resource` envelope.

use rise_resource_api::{JsonObject, Resource, ResourceRow, ValidationError};
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
/// Conversion validates metadata, spec, and status fail-closed. The generic
/// JSON object types preserve valid external resource fields verbatim, while
/// `apiVersion` is projected separately for the requested served route.
pub fn row_to_resource(row: &ResourceRow) -> Result<Resource, ValidationError> {
    row_to_resource_with_api_version(row, &row.api_version)
}

pub fn row_to_resource_with_api_version(
    row: &ResourceRow,
    api_version: &str,
) -> Result<Resource, ValidationError> {
    let mut resource = row.to_resource::<JsonObject, JsonObject>()?;
    resource.api_version = api_version.to_owned();
    Ok(resource)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;
    use uuid::Uuid;

    fn row(
        metadata: serde_json::Value,
        spec: serde_json::Value,
        status: serde_json::Value,
    ) -> ResourceRow {
        ResourceRow {
            uid: Uuid::new_v4(),
            api_version: "rise.dev/v1alpha1".into(),
            kind: "Organization".into(),
            parent_uid: None,
            name: "acme".into(),
            discriminator: "abcd1234".into(),
            metadata,
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
        let r = row(
            json!({"team": "platform"}),
            json!({"displayName": "Acme"}),
            json!({"controllers": {}}),
        );
        let resource = row_to_resource(&r).unwrap();

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
    fn row_to_resource_projects_the_served_api_version() {
        let r = row(json!({}), json!({"value": 1}), json!({}));
        let resource = row_to_resource_with_api_version(&r, "rise.dev/v1").unwrap();
        assert_eq!(resource.api_version, "rise.dev/v1");
        assert_eq!(resource.spec.get("value"), Some(&json!(1)));
    }

    #[test]
    fn row_to_resource_rejects_malformed_metadata() {
        let r = row(json!({"count": 42}), json!({}), json!({}));
        assert!(row_to_resource(&r)
            .unwrap_err()
            .to_string()
            .contains("invalid metadata"));
    }

    #[test]
    fn row_to_resource_rejects_malformed_spec() {
        let r = row(json!({}), json!("not an object"), json!({}));
        assert!(row_to_resource(&r)
            .unwrap_err()
            .to_string()
            .contains("invalid spec"));
    }

    #[test]
    fn row_to_resource_rejects_malformed_status() {
        let r = row(json!({}), json!({}), json!(null));
        assert!(row_to_resource(&r)
            .unwrap_err()
            .to_string()
            .contains("invalid status"));
    }
}
