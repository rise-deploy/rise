use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;

use crate::{validate_resource_reference_name, ResourceKind, ValidationError};

/// A UID-authoritative lifecycle reference to another resource.
///
/// The API version, kind, and name make the relationship inspectable and are
/// verified when the reference is written. Garbage collection follows `uid`,
/// so recreating the same name never transfers lifecycle ownership.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OwnerReference {
    api_version: String,
    kind: String,
    name: String,
    uid: Uuid,
}

impl OwnerReference {
    pub fn new(
        api_version: impl Into<String>,
        kind: impl Into<String>,
        name: impl Into<String>,
        uid: Uuid,
    ) -> Result<Self, ValidationError> {
        let reference = Self {
            api_version: api_version.into(),
            kind: kind.into(),
            name: name.into(),
            uid,
        };
        reference.validate()?;
        Ok(reference)
    }

    pub fn api_version(&self) -> &str {
        &self.api_version
    }

    pub fn kind(&self) -> &str {
        &self.kind
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn uid(&self) -> Uuid {
        self.uid
    }

    pub fn resource_kind(&self) -> Result<ResourceKind, ValidationError> {
        ResourceKind::from_api_route(&self.api_version, &self.kind)
    }

    fn validate(&self) -> Result<(), ValidationError> {
        self.resource_kind()?;
        validate_resource_reference_name(&self.name)?;
        if self.uid.is_nil() {
            return Err(ValidationError::new("owner reference UID must not be nil"));
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for OwnerReference {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Repr {
            api_version: String,
            kind: String,
            name: String,
            uid: Uuid,
        }

        let Repr {
            api_version,
            kind,
            name,
            uid,
        } = Repr::deserialize(deserializer)?;
        Self::new(api_version, kind, name, uid).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for OwnerReference {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "OwnerReference".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "object",
            "properties": {
                "apiVersion": {
                    "type": "string",
                    "pattern": "^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?(?:\\.[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?)*/[a-z0-9]+$"
                },
                "kind": {
                    "type": "string",
                    "pattern": "^[A-Z][A-Za-z0-9]*$"
                },
                "name": {
                    "type": "string",
                    "maxLength": 253,
                    "pattern": "^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?(?:\\.[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?)*$"
                },
                "uid": {
                    "type": "string",
                    "format": "uuid",
                    "not": { "const": "00000000-0000-0000-0000-000000000000" }
                }
            },
            "required": ["apiVersion", "kind", "name", "uid"],
            "additionalProperties": false
        })
    }
}
