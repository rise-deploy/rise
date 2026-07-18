use std::fmt;
use std::str::FromStr;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{validate_resource_name, SubjectId, ValidationError};

/// Typed dynamic-binding input: an absolute User or organization-relative Group.
///
/// Resolution is intentionally separate from parsing because a Group reference
/// acquires its organization from the resource being evaluated.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SubjectRef(String);

impl SubjectRef {
    pub fn resolve(&self, organization: Option<&str>) -> Result<SubjectId, ValidationError> {
        let (kind, name) = self.0.split_once(':').expect("validated SubjectRef");
        match kind {
            "user" => self.0.parse(),
            "group" => {
                let org = organization.ok_or_else(|| {
                    ValidationError::new("relative group subject requires an organization")
                })?;
                validate_resource_name(org)?;
                format!("group:{org}/{name}").parse()
            }
            _ => unreachable!("validated SubjectRef"),
        }
    }
}

impl fmt::Debug for SubjectRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SubjectRef").field(&self.0).finish()
    }
}
impl fmt::Display for SubjectRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl FromStr for SubjectRef {
    type Err = ValidationError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (kind, name) = value.split_once(':').ok_or_else(|| {
            ValidationError::new("subject reference must be 'user:<name>' or 'group:<name>'")
        })?;
        if name.contains(':') || name.contains('/') {
            return Err(ValidationError::new(
                "subject reference must contain one unqualified resource name",
            ));
        }
        if !matches!(kind, "user" | "group") {
            return Err(ValidationError::new(
                "subject reference must be 'user:<name>' or 'group:<name>'",
            ));
        }
        if kind == "user" && name == "me" {
            return Err(ValidationError::new(
                "'user:me' is an input alias, not a canonical subject reference",
            ));
        }
        validate_resource_name(name)?;
        Ok(Self(value.to_owned()))
    }
}
impl TryFrom<&str> for SubjectRef {
    type Error = ValidationError;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        value.parse()
    }
}
impl AsRef<str> for SubjectRef {
    fn as_ref(&self) -> &str {
        &self.0
    }
}
impl Serialize for SubjectRef {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}
impl<'de> Deserialize<'de> for SubjectRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}
impl JsonSchema for SubjectRef {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "SubjectRef".into()
    }
    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "oneOf": [
                {
                    "pattern": "^user:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$",
                    "maxLength": 68,
                    "not": { "const": "user:me" }
                },
                {
                    "pattern": "^group:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$",
                    "maxLength": 69
                }
            ]
        })
    }
}
