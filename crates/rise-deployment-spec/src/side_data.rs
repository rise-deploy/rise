use serde::{Deserialize, Serialize};
use std::fmt;

/// On-disk schema version for the `containers` / `routes` JSONB side-data.
pub const CONTAINER_SIDE_DATA_VERSION: u32 = 1;

#[derive(Serialize)]
struct SideDataWrite<'a, T> {
    version: u32,
    items: &'a [T],
}

#[derive(Deserialize)]
struct SideDataRead {
    version: u32,
    items: serde_json::Value,
}

pub enum SideDataError {
    Envelope(serde_json::Error),
    UnsupportedVersion {
        found: u32,
        expected: u32,
    },
    Items {
        version: u32,
        source: serde_json::Error,
    },
}

impl fmt::Debug for SideDataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for SideDataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SideDataError::Envelope(e) => {
                write!(f, "side-data envelope could not be deserialized: {e}")
            }
            SideDataError::UnsupportedVersion { found, expected } => write!(
                f,
                "unsupported container side-data version {found} (this build understands version {expected})"
            ),
            SideDataError::Items { version, source } => write!(
                f,
                "side-data items (version {version}) could not be deserialized: {source}"
            ),
        }
    }
}

impl std::error::Error for SideDataError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SideDataError::Envelope(e) => Some(e),
            SideDataError::Items { source, .. } => Some(source),
            SideDataError::UnsupportedVersion { .. } => None,
        }
    }
}

pub fn encode_side_data<T: Serialize>(items: &[T]) -> Result<serde_json::Value, serde_json::Error> {
    serde_json::to_value(SideDataWrite {
        version: CONTAINER_SIDE_DATA_VERSION,
        items,
    })
}

pub fn decode_side_data<T: serde::de::DeserializeOwned>(
    value: &serde_json::Value,
) -> Result<Vec<T>, SideDataError> {
    let envelope: SideDataRead =
        serde_json::from_value(value.clone()).map_err(SideDataError::Envelope)?;
    if envelope.version != CONTAINER_SIDE_DATA_VERSION {
        return Err(SideDataError::UnsupportedVersion {
            found: envelope.version,
            expected: CONTAINER_SIDE_DATA_VERSION,
        });
    }
    serde_json::from_value(envelope.items).map_err(|source| SideDataError::Items {
        version: envelope.version,
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request_spec::{ContainerSpec, RouteSpec};
    use serde_json::json;

    #[test]
    fn side_data_uses_versioned_envelope() {
        let specs = vec![ContainerSpec {
            name: "api".to_string(),
            image: Some("img".to_string()),
            port: Some(8080),
            replicas: None,
            cpu: None,
            memory: None,
            env_overrides: vec![],
            health_check: None,
        }];

        let encoded = encode_side_data(&specs).unwrap();
        assert_eq!(encoded["version"], CONTAINER_SIDE_DATA_VERSION);
        assert_eq!(encoded["items"][0]["name"], "api");
        let decoded: Vec<ContainerSpec> = decode_side_data(&encoded).unwrap();
        assert_eq!(decoded[0].name, "api");
    }

    #[test]
    fn side_data_routes_round_trip_same_envelope() {
        use crate::access::AccessRequirement;
        let routes = vec![
            RouteSpec {
                path: "/api".to_string(),
                container: "api".to_string(),
                access: None,
            },
            RouteSpec {
                path: "/admin".to_string(),
                container: "api".to_string(),
                access: Some(AccessRequirement::Member),
            },
        ];
        let decoded: Vec<RouteSpec> =
            decode_side_data(&encode_side_data(&routes).unwrap()).unwrap();
        assert_eq!(decoded[0].path, "/api");
        assert_eq!(decoded[0].access, None);
        assert_eq!(decoded[1].access, Some(AccessRequirement::Member));
    }

    #[test]
    fn side_data_routes_without_access_field_decode_as_none() {
        // A version-1 payload written before `access` existed must still decode
        // (the field is `#[serde(default)]`) — no version bump required.
        let legacy = json!({
            "version": CONTAINER_SIDE_DATA_VERSION,
            "items": [{ "path": "/", "container": "app" }],
        });
        let decoded: Vec<RouteSpec> = decode_side_data(&legacy).unwrap();
        assert_eq!(decoded[0].access, None);
    }

    #[test]
    fn side_data_rejects_unknown_version_and_bare_array() {
        let future = json!({ "version": CONTAINER_SIDE_DATA_VERSION + 1, "items": [] });
        let err = decode_side_data::<ContainerSpec>(&future).unwrap_err();
        assert!(format!("{err:?}").contains("unsupported container side-data version"));

        let bare = json!([{ "name": "api", "port": 8080 }]);
        assert!(decode_side_data::<ContainerSpec>(&bare).is_err());
    }
}
