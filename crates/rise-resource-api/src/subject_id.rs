use std::fmt;
use std::str::FromStr;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{validate_resource_name, ValidationError};

/// A concrete, canonical authorization subject.
///
/// This parser validates syntax only. Non-virtual subjects must additionally
/// be resolved against the resource store at policy write time.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SubjectId(String);

impl SubjectId {
    /// Whether this subject is a virtual predicate rather than a persisted
    /// identity resource.
    ///
    /// `org:<name>` and both `system:*` subjects are virtual in ADR-0001.
    pub fn is_virtual(&self) -> bool {
        matches!(self.kind(), "org" | "system")
    }

    /// Whether the subject is intrinsically tied to exactly one organization.
    ///
    /// Only Group and ServiceAccount identities drive org-native scope
    /// normalization. The virtual `org:<name>` predicate has organization
    /// affinity, but is not an org-native identity.
    pub fn is_org_native(&self) -> bool {
        matches!(self.kind(), "group" | "serviceaccount")
    }

    pub fn organization(&self) -> Option<&str> {
        let (_, value) = self.0.split_once(':')?;
        match self.kind() {
            "group" | "serviceaccount" => value.split_once('/').map(|(org, _)| org),
            "org" => Some(value),
            _ => None,
        }
    }

    /// Whether this subject could ever satisfy ADR-0001 §1's recipient boundary
    /// for `organization`, judged from the identifier alone.
    ///
    /// A subject that names its own organization — `group:`, `serviceaccount:`,
    /// `org:` — answers definitively, and both facts are frozen at write time,
    /// so a `false` here is permanent. Every other kind is *contingent*: a
    /// `user:` or `system:authenticated` subject belongs exactly while the
    /// caller is affiliated, which only the evaluator can decide. Those report
    /// `true` and leave the live question open.
    ///
    /// `controller:` is the one kind that belongs to no organization yet still
    /// reports `true`. Making that a write-time error would collide with the
    /// org opt-in enablement design tracked in rise-deploy/rise#437, under
    /// which an org-parented binding naming a controller becomes meaningful.
    pub fn may_belong_to(&self, organization: &str) -> bool {
        match self.organization() {
            Some(own) => own == organization,
            None => true,
        }
    }

    pub fn kind(&self) -> &str {
        self.0.split_once(':').expect("validated SubjectId").0
    }

    /// The identity's own name, without its kind prefix or organization
    /// segment: `group:acme/platform` is named `platform`, `user:alice` is
    /// named `alice`.
    ///
    /// For a virtual subject this is the predicate's own tail (`acme` for
    /// `org:acme`, `authenticated` for `system:authenticated`) — parsing does
    /// not imply a resource of that name exists.
    pub fn name(&self) -> &str {
        let (_, value) = self.0.split_once(':').expect("validated SubjectId");
        match self.kind() {
            "group" | "serviceaccount" => value.split_once('/').expect("validated SubjectId").1,
            _ => value,
        }
    }
}

impl fmt::Debug for SubjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SubjectId").field(&self.0).finish()
    }
}
impl fmt::Display for SubjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for SubjectId {
    type Err = ValidationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (kind, name) = value.split_once(':').ok_or_else(|| {
            ValidationError::new("subject must contain exactly one kind separator ':'")
        })?;
        if name.contains(':') {
            return Err(ValidationError::new(
                "subject must contain exactly one kind separator ':'",
            ));
        }

        match kind {
            "user" | "controller" | "org" => {
                if name.contains('/') {
                    return Err(ValidationError::new("subject has an unexpected '/'"));
                }
                validate_resource_name(name)?;
            }
            "group" | "serviceaccount" => {
                let (org, local) = name.split_once('/').ok_or_else(|| {
                    ValidationError::new(format!("{kind} subject must be '<org>/<name>'"))
                })?;
                if local.contains('/') {
                    return Err(ValidationError::new(format!(
                        "{kind} subject must be '<org>/<name>'"
                    )));
                }
                validate_resource_name(org)?;
                validate_resource_name(local)?;
            }
            "system" if matches!(name, "authenticated" | "operators") => {}
            "system" => return Err(ValidationError::new("unknown system subject")),
            _ => return Err(ValidationError::new("unknown subject kind")),
        }
        Ok(Self(value.to_owned()))
    }
}

impl TryFrom<&str> for SubjectId {
    type Error = ValidationError;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        value.parse()
    }
}
impl AsRef<str> for SubjectId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}
impl Serialize for SubjectId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}
impl<'de> Deserialize<'de> for SubjectId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}
impl JsonSchema for SubjectId {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "SubjectId".into()
    }
    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "oneOf": [
                {
                    "pattern": "^(?:user|controller|org):[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$",
                    "maxLength": 74
                },
                {
                    "pattern": "^(?:group|serviceaccount):[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?/[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$",
                    "maxLength": 142
                },
                { "enum": ["system:authenticated", "system:operators"] }
            ]
        })
    }
}
