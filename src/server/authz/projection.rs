//! Response projection for the two read granularities (ADR-0001 §4).
//!
//! `get` and `list` are independent verbs, so a collection can contain items a
//! caller may see the existence of but not the contents of. The projector is
//! what keeps those apart: a list-only item is built from an explicit
//! allowlist — `apiVersion`, `kind`, and the documented `metadata` fields — and
//! never by deleting `spec` and `status` from a full object. A generic resource
//! may carry top-level fields nobody here knows the names of, and a deny-list
//! would hand those out.

use rise_resource_api::Resource;
use serde_json::{json, Value};

use crate::server::error::ServerError;

/// How much of one item the caller may see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadGranularity {
    /// `list` without `get`: existence, name, and targeting metadata.
    ListOnly,
    /// `list` and `get`: the full stored object.
    Full,
}

/// Project one item of a collection response.
///
/// The list-only shape carries the labels and their inherited resolution
/// deliberately: ADR-0001 §4 makes the common list-and-inspect path work without
/// a follow-up `get` per item, and ownership is what a listing is usually read
/// for. It is also why an organization should not put sensitive metadata in an
/// ancestor's labels and then grant broad `list` beneath it — inheritance is
/// org-wide by construction.
pub fn project_list_item(
    resource: &Resource,
    granularity: ReadGranularity,
) -> Result<Value, ServerError> {
    match granularity {
        ReadGranularity::Full => serde_json::to_value(resource).map_err(|error| {
            ServerError::internal_anyhow(
                anyhow::Error::new(error),
                "failed to serialize a resource",
            )
        }),
        ReadGranularity::ListOnly => {
            let mut metadata = json!({ "name": resource.metadata.name });
            let object = metadata
                .as_object_mut()
                .expect("metadata literal is an object");
            if !resource.metadata.labels.is_empty() {
                object.insert("labels".into(), json!(resource.metadata.labels));
            }
            if !resource.metadata.effective_labels.is_empty() {
                object.insert(
                    "effectiveLabels".into(),
                    json!(resource.metadata.effective_labels),
                );
            }
            if let Some(deletion_timestamp) = resource.metadata.deletion_timestamp {
                object.insert("deletionTimestamp".into(), json!(deletion_timestamp));
            }
            Ok(json!({
                "apiVersion": resource.api_version,
                "kind": resource.kind,
                "metadata": metadata,
            }))
        }
    }
}
