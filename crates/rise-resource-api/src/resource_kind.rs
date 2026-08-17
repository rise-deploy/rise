use std::fmt;
use std::str::FromStr;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    validate_resource_group, validate_resource_kind, validate_resource_version, ValidationError,
};

/// Canonical, version-independent identity of a resource kind.
///
/// Parsing validates only the closed `<api-group>/<Kind>` syntax. Whether the
/// kind is registered is deliberately a write-time concern for the service
/// that owns the resource-definition registry.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResourceKind(String);

impl ResourceKind {
    pub fn new(group: &str, kind: &str) -> Result<Self, ValidationError> {
        validate_resource_group(group)?;
        validate_resource_kind(kind)?;
        Ok(Self(format!("{group}/{kind}")))
    }

    /// Normalize a versioned API route (`group/version`, `Kind`) to its
    /// version-independent authorization identity.
    pub fn from_api_route(api_version: &str, kind: &str) -> Result<Self, ValidationError> {
        let (group, version) = api_version
            .split_once('/')
            .ok_or_else(|| ValidationError::new("api version must be '<api-group>/<version>'"))?;
        if version.contains('/') {
            return Err(ValidationError::new(
                "api version must be '<api-group>/<version>'",
            ));
        }
        validate_resource_version(version)?;
        Self::new(group, kind)
    }

    pub fn group(&self) -> &str {
        self.0.split_once('/').expect("validated ResourceKind").0
    }

    pub fn kind(&self) -> &str {
        self.0.split_once('/').expect("validated ResourceKind").1
    }
}

impl fmt::Debug for ResourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ResourceKind").field(&self.0).finish()
    }
}

impl fmt::Display for ResourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for ResourceKind {
    type Err = ValidationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (group, kind) = value
            .split_once('/')
            .ok_or_else(|| ValidationError::new("resource kind must be '<api-group>/<Kind>'"))?;
        if kind.contains('/') {
            return Err(ValidationError::new(
                "resource kind must be '<api-group>/<Kind>'",
            ));
        }
        Self::new(group, kind)
    }
}

impl TryFrom<&str> for ResourceKind {
    type Error = ValidationError;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl AsRef<str> for ResourceKind {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Serialize for ResourceKind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ResourceKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for ResourceKind {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ResourceKind".into()
    }
    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": "^(?=.{1,253}\\/)[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?(?:\\.[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?)*\\/[A-Z][A-Za-z0-9]*$",
            "minLength": 3
        })
    }
}
