use std::fmt;
use std::str::FromStr;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{validate_resource_name, ResourceKind, ValidationError};

/// Canonical binding scope: `*` or `<api-group>/<Kind>/<names...>`.
///
/// Parsing establishes canonical syntax only. Registry lookup, parent-chain
/// shape, containment, and same-transaction target resolution are write-time
/// checks and are intentionally not performed by this dependency-light type.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Scope {
    canonical: String,
    resource_kind: Option<ResourceKind>,
    names: Vec<String>,
}

impl Scope {
    pub fn wildcard() -> Self {
        Self {
            canonical: "*".to_owned(),
            resource_kind: None,
            names: Vec::new(),
        }
    }
    pub fn is_wildcard(&self) -> bool {
        self.resource_kind.is_none()
    }

    /// The parsed leaf kind, borrowed without allocation.
    pub fn resource_kind(&self) -> Option<&ResourceKind> {
        self.resource_kind.as_ref()
    }

    /// Canonical ancestor names followed by the leaf name.
    pub fn names(&self) -> impl ExactSizeIterator<Item = &str> {
        self.names.iter().map(String::as_str)
    }
}

impl fmt::Debug for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Scope").field(&self.canonical).finish()
    }
}
impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.canonical)
    }
}
impl FromStr for Scope {
    type Err = ValidationError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value == "*" {
            return Ok(Self::wildcard());
        }
        let parts: Vec<&str> = value.split('/').collect();
        if parts.len() < 3 {
            return Err(ValidationError::new(
                "scope must be '*' or '<api-group>/<Kind>/<names...>'",
            ));
        }
        let resource_kind = ResourceKind::new(parts[0], parts[1])?;
        for name in &parts[2..] {
            validate_resource_name(name)?;
        }
        Ok(Self {
            canonical: value.to_owned(),
            resource_kind: Some(resource_kind),
            names: parts[2..].iter().map(|name| (*name).to_owned()).collect(),
        })
    }
}
impl TryFrom<&str> for Scope {
    type Error = ValidationError;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        value.parse()
    }
}
impl AsRef<str> for Scope {
    fn as_ref(&self) -> &str {
        &self.canonical
    }
}
impl Serialize for Scope {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.canonical)
    }
}
impl<'de> Deserialize<'de> for Scope {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}
impl JsonSchema for Scope {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Scope".into()
    }
    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "oneOf": [
                { "const": "*" },
                {
                    "pattern": "^(?=.{1,253}\\/)[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?(?:\\.[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?)*\\/[A-Z][A-Za-z0-9]*(?:\\/[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?)+$",
                    "minLength": 5
                }
            ]
        })
    }
}
