use std::{collections::BTreeMap, sync::OnceLock};

use regex::Regex;
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize};
use url::Url;

use crate::builtin_kind::ORGANIZATION_PARENT;
use crate::{
    BuiltInKindDefinition, BuiltInKindParent, ValidationError, API_VERSION_V1ALPHA1,
    CONTROLLER_COLLECTION, CONTROLLER_KIND, CONTROLLER_TRUST_POLICY_COLLECTION,
    CONTROLLER_TRUST_POLICY_KIND, GROUP_COLLECTION, GROUP_KIND, GROUP_MEMBERSHIP_COLLECTION,
    GROUP_MEMBERSHIP_KIND, SERVICE_ACCOUNT_COLLECTION, SERVICE_ACCOUNT_KIND,
    SERVICE_ACCOUNT_TRUST_POLICY_COLLECTION, SERVICE_ACCOUNT_TRUST_POLICY_KIND, USER_COLLECTION,
    USER_IDENTITY_COLLECTION, USER_IDENTITY_KIND, USER_KIND,
};

const USER_PARENT: BuiltInKindParent = BuiltInKindParent {
    api_version: API_VERSION_V1ALPHA1,
    kind: USER_KIND,
};

const CONTROLLER_PARENT: BuiltInKindParent = BuiltInKindParent {
    api_version: API_VERSION_V1ALPHA1,
    kind: CONTROLLER_KIND,
};

const GROUP_PARENT: BuiltInKindParent = BuiltInKindParent {
    api_version: API_VERSION_V1ALPHA1,
    kind: GROUP_KIND,
};

const SERVICE_ACCOUNT_PARENT: BuiltInKindParent = BuiltInKindParent {
    api_version: API_VERSION_V1ALPHA1,
    kind: SERVICE_ACCOUNT_KIND,
};

/// All persisted ADR-0001 identity kinds and their fixed placement.
pub const IDENTITY_KIND_DEFINITIONS: [BuiltInKindDefinition; 8] = [
    BuiltInKindDefinition {
        api_version: API_VERSION_V1ALPHA1,
        kind: USER_KIND,
        collection: USER_COLLECTION,
        parent: None,
    },
    BuiltInKindDefinition {
        api_version: API_VERSION_V1ALPHA1,
        kind: USER_IDENTITY_KIND,
        collection: USER_IDENTITY_COLLECTION,
        parent: Some(USER_PARENT),
    },
    BuiltInKindDefinition {
        api_version: API_VERSION_V1ALPHA1,
        kind: CONTROLLER_KIND,
        collection: CONTROLLER_COLLECTION,
        parent: None,
    },
    BuiltInKindDefinition {
        api_version: API_VERSION_V1ALPHA1,
        kind: CONTROLLER_TRUST_POLICY_KIND,
        collection: CONTROLLER_TRUST_POLICY_COLLECTION,
        parent: Some(CONTROLLER_PARENT),
    },
    BuiltInKindDefinition {
        api_version: API_VERSION_V1ALPHA1,
        kind: GROUP_KIND,
        collection: GROUP_COLLECTION,
        parent: Some(ORGANIZATION_PARENT),
    },
    BuiltInKindDefinition {
        api_version: API_VERSION_V1ALPHA1,
        kind: GROUP_MEMBERSHIP_KIND,
        collection: GROUP_MEMBERSHIP_COLLECTION,
        parent: Some(GROUP_PARENT),
    },
    BuiltInKindDefinition {
        api_version: API_VERSION_V1ALPHA1,
        kind: SERVICE_ACCOUNT_KIND,
        collection: SERVICE_ACCOUNT_COLLECTION,
        parent: Some(ORGANIZATION_PARENT),
    },
    BuiltInKindDefinition {
        api_version: API_VERSION_V1ALPHA1,
        kind: SERVICE_ACCOUNT_TRUST_POLICY_KIND,
        collection: SERVICE_ACCOUNT_TRUST_POLICY_COLLECTION,
        parent: Some(SERVICE_ACCOUNT_PARENT),
    },
];

/// Stable human identity and non-authoritative presentation fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UserSpec {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    #[schemars(with = "String")]
    pub display_name: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    #[schemars(with = "String")]
    pub primary_email: Option<String>,
    #[serde(default = "default_true")]
    pub active: bool,
}

impl Default for UserSpec {
    fn default() -> Self {
        Self {
            display_name: None,
            primary_email: None,
            active: true,
        }
    }
}

/// Maximum accepted canonical ASCII issuer length.
pub const MAX_ISSUER_BYTES: usize = 1024;

/// Maximum number of Unicode scalar values retained in an external subject.
///
/// OIDC subjects are limited to 255 ASCII characters. Allowing the same count
/// for provider adapters also bounds the worst-case UTF-8 representation to
/// 1020 bytes, keeping the future `(issuer, subject)` B-tree key below its
/// index-entry budget.
pub const MAX_EXTERNAL_SUBJECT_CHARS: usize = 255;

// One deliberately narrow ASCII URI language shared by runtime admission and JSON Schema.
// Hostnames start with an alphabetic label; exact IPv4 addresses and ports 0..=65535 are also
// accepted. Paths admit RFC 3986 unreserved/sub-delimiter characters and well-formed percent
// escapes. Queries, fragments, credentials, Unicode, IPv6, and parser-repaired spellings are
// intentionally outside the contract.
const ISSUER_PATTERN: &str = r"^https?://(?:(?:[A-Za-z](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?(?:\.[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?)*)|(?:(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])\.){3}(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9]))(?::(?:[0-9]{1,4}|[1-5][0-9]{4}|6[0-4][0-9]{3}|65[0-4][0-9]{2}|655[0-2][0-9]|6553[0-5]))?(?:/(?:[A-Za-z0-9._~!$&'()*+,;=:@/-]|%[0-9A-Fa-f]{2})*)?$";

fn issuer_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(ISSUER_PATTERN).expect("issuer pattern must compile"))
}

/// Canonical external identity issuer shared by UserIdentity and trust policy.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct Issuer(String);

impl Issuer {
    pub fn new(value: impl AsRef<str>) -> Result<Self, ValidationError> {
        let value = value.as_ref();
        if value.len() > MAX_ISSUER_BYTES {
            return Err(ValidationError::new(format!(
                "issuer URL must not exceed {MAX_ISSUER_BYTES} bytes"
            )));
        }
        if !issuer_regex().is_match(value) {
            return Err(ValidationError::new(
                "issuer URL does not match the supported HTTP(S) issuer grammar",
            ));
        }

        let parsed = Url::parse(value)
            .map_err(|error| ValidationError::new(format!("invalid issuer URL: {error}")))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(ValidationError::new(
                "issuer URL scheme must be http or https",
            ));
        }
        if parsed.host().is_none() {
            return Err(ValidationError::new("issuer URL must contain a host"));
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(ValidationError::new(
                "issuer URL must not contain credentials",
            ));
        }
        if parsed.query().is_some() || parsed.fragment().is_some() {
            return Err(ValidationError::new(
                "issuer URL must not contain a query or fragment",
            ));
        }

        // Reject aliases instead of silently changing an authentication
        // identifier. `url::Url` normalizes host case, default ports, and dot
        // segments; Rise additionally chooses the spelling without a trailing
        // slash as canonical.
        let canonical = parsed.as_str().trim_end_matches('/').to_owned();
        if canonical != value {
            return Err(ValidationError::new(format!(
                "issuer URL must be in its canonical form \"{canonical}\""
            )));
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for Issuer {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::fmt::Display for Issuer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Issuer {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for Issuer {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Issuer".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        // JSON Schema can validate the accepted URL shape but cannot express
        // `url::Url`'s exact canonical spelling. Typed admission performs that
        // final equality check.
        schemars::json_schema!({
            "type": "string",
            "minLength": 1,
            "maxLength": MAX_ISSUER_BYTES,
            "pattern": ISSUER_PATTERN
        })
    }
}

/// Opaque, case-sensitive subject from an external identity provider.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ExternalSubject(String);

impl ExternalSubject {
    pub fn new(value: impl Into<String>) -> Result<Self, ValidationError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(ValidationError::new("external subject must not be blank"));
        }
        if value.contains('\0') {
            return Err(ValidationError::new(
                "external subject must not contain U+0000",
            ));
        }
        if value.chars().count() > MAX_EXTERNAL_SUBJECT_CHARS {
            return Err(ValidationError::new(format!(
                "external subject must not exceed {MAX_EXTERNAL_SUBJECT_CHARS} characters"
            )));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for ExternalSubject {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::fmt::Display for ExternalSubject {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ExternalSubject {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for ExternalSubject {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ExternalSubject".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        // Require at least one non-whitespace character while preserving all
        // surrounding whitespace: an external subject is opaque, not trimmed.
        schemars::json_schema!({
            "type": "string",
            "minLength": 1,
            "maxLength": MAX_EXTERNAL_SUBJECT_CHARS,
            "allOf": [
                { "pattern": ".*\\S.*" },
                { "pattern": "^[^\\u0000]*$" }
            ]
        })
    }
}

/// One authoritative external SSO mapping beneath a User.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UserIdentitySpec {
    pub issuer: Issuer,
    pub subject: ExternalSubject,
    #[serde(default = "default_true")]
    pub active: bool,
}

/// Stable root-scoped workload identity.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ControllerSpec {}

/// Stable Organization-owned group identity.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GroupSpec {}

/// Stable Organization-owned workload identity.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServiceAccountSpec {}

/// One name-bound boolean membership marker under the parent Group.
///
/// Admission requires the resource name to equal an existing User's canonical
/// name. Optional lifecycle ownership belongs in generic resource metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GroupMembershipSpec {}

/// Required string-valued JWT claim constraints for a workload trust policy.
///
/// Arbitrary claim names are intentional, but every key and value is nonblank
/// and `aud` is mandatory. Matching semantics live in `rise-backend-auth`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct TrustPolicyClaims(BTreeMap<String, String>);

impl TrustPolicyClaims {
    pub fn new(claims: BTreeMap<String, String>) -> Result<Self, ValidationError> {
        validate_claims(&claims)?;
        Ok(Self(claims))
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }

    /// The underlying claim map, for callers matching against it directly
    /// (e.g. `rise_backend_auth::match_trust_candidates`, which cannot depend
    /// on this crate).
    pub fn as_map(&self) -> &BTreeMap<String, String> {
        &self.0
    }
}

impl<'de> Deserialize<'de> for TrustPolicyClaims {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let claims = BTreeMap::<String, String>::deserialize(deserializer)?;
        Self::new(claims).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for TrustPolicyClaims {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "TrustPolicyClaims".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "object",
            "properties": {
                "aud": { "type": "string", "minLength": 1, "pattern": ".*\\S.*" }
            },
            "required": ["aud"],
            "propertyNames": { "type": "string", "minLength": 1, "pattern": ".*\\S.*" },
            "additionalProperties": {
                "type": "string",
                "minLength": 1,
                "pattern": ".*\\S.*"
            }
        })
    }
}

/// One public external issuer and closed string-claim matcher beneath a Controller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ControllerTrustPolicySpec {
    pub issuer: Issuer,
    pub claims: TrustPolicyClaims,
}

/// One public external issuer and closed string-claim matcher beneath a ServiceAccount.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServiceAccountTrustPolicySpec {
    pub issuer: Issuer,
    pub claims: TrustPolicyClaims,
}

fn default_true() -> bool {
    true
}

fn deserialize_optional_non_null<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

fn validate_claims(claims: &BTreeMap<String, String>) -> Result<(), ValidationError> {
    if claims
        .iter()
        .any(|(key, value)| key.trim().is_empty() || value.trim().is_empty())
    {
        return Err(ValidationError::new(
            "trust policy claim names and constraints must not be blank",
        ));
    }
    if claims
        .get("aud")
        .is_none_or(|audience| audience.trim().is_empty())
    {
        return Err(ValidationError::new(
            "trust policy claims must contain a nonblank 'aud' constraint",
        ));
    }
    Ok(())
}
