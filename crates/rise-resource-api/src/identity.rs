use std::{collections::BTreeMap, sync::OnceLock};

use regex::Regex;
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize};
use url::Url;
use uuid::Uuid;

use crate::{
    ResourceKind, ValidationError, API_VERSION_V1ALPHA1, CONTROLLER_COLLECTION, CONTROLLER_KIND,
    CONTROLLER_TRUST_POLICY_COLLECTION, CONTROLLER_TRUST_POLICY_KIND, GROUP_COLLECTION, GROUP_KIND,
    GROUP_MEMBERSHIP_COLLECTION, GROUP_MEMBERSHIP_KIND, ORGANIZATION_KIND,
    SERVICE_ACCOUNT_COLLECTION, SERVICE_ACCOUNT_KIND, SERVICE_ACCOUNT_TRUST_POLICY_COLLECTION,
    SERVICE_ACCOUNT_TRUST_POLICY_KIND, USER_COLLECTION, USER_IDENTITY_COLLECTION,
    USER_IDENTITY_KIND, USER_KIND,
};

/// Static API identity and placement of one ADR-0001 identity resource kind.
///
/// This is contract metadata only. Runtime registration remains behind the
/// transaction-scoped normalization and admission work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentityKindDefinition {
    pub api_version: &'static str,
    pub kind: &'static str,
    pub collection: &'static str,
    pub parent: Option<IdentityKindParent>,
}

/// Fixed parent identity for an ADR-0001 identity resource kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentityKindParent {
    pub api_version: &'static str,
    pub kind: &'static str,
}

const ORGANIZATION_PARENT: IdentityKindParent = IdentityKindParent {
    api_version: API_VERSION_V1ALPHA1,
    kind: ORGANIZATION_KIND,
};

const USER_PARENT: IdentityKindParent = IdentityKindParent {
    api_version: API_VERSION_V1ALPHA1,
    kind: USER_KIND,
};

const CONTROLLER_PARENT: IdentityKindParent = IdentityKindParent {
    api_version: API_VERSION_V1ALPHA1,
    kind: CONTROLLER_KIND,
};

const GROUP_PARENT: IdentityKindParent = IdentityKindParent {
    api_version: API_VERSION_V1ALPHA1,
    kind: GROUP_KIND,
};

const SERVICE_ACCOUNT_PARENT: IdentityKindParent = IdentityKindParent {
    api_version: API_VERSION_V1ALPHA1,
    kind: SERVICE_ACCOUNT_KIND,
};

/// All persisted ADR-0001 identity kinds and their fixed placement.
pub const IDENTITY_KIND_DEFINITIONS: [IdentityKindDefinition; 8] = [
    IdentityKindDefinition {
        api_version: API_VERSION_V1ALPHA1,
        kind: USER_KIND,
        collection: USER_COLLECTION,
        parent: None,
    },
    IdentityKindDefinition {
        api_version: API_VERSION_V1ALPHA1,
        kind: USER_IDENTITY_KIND,
        collection: USER_IDENTITY_COLLECTION,
        parent: Some(USER_PARENT),
    },
    IdentityKindDefinition {
        api_version: API_VERSION_V1ALPHA1,
        kind: CONTROLLER_KIND,
        collection: CONTROLLER_COLLECTION,
        parent: None,
    },
    IdentityKindDefinition {
        api_version: API_VERSION_V1ALPHA1,
        kind: CONTROLLER_TRUST_POLICY_KIND,
        collection: CONTROLLER_TRUST_POLICY_COLLECTION,
        parent: Some(CONTROLLER_PARENT),
    },
    IdentityKindDefinition {
        api_version: API_VERSION_V1ALPHA1,
        kind: GROUP_KIND,
        collection: GROUP_COLLECTION,
        parent: Some(ORGANIZATION_PARENT),
    },
    IdentityKindDefinition {
        api_version: API_VERSION_V1ALPHA1,
        kind: GROUP_MEMBERSHIP_KIND,
        collection: GROUP_MEMBERSHIP_COLLECTION,
        parent: Some(GROUP_PARENT),
    },
    IdentityKindDefinition {
        api_version: API_VERSION_V1ALPHA1,
        kind: SERVICE_ACCOUNT_KIND,
        collection: SERVICE_ACCOUNT_COLLECTION,
        parent: Some(ORGANIZATION_PARENT),
    },
    IdentityKindDefinition {
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

/// Maximum accepted raw ASCII issuer length; canonicalization can only shorten it.
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

        // The existing authentication matcher treats trailing slashes as
        // aliases. Persist one spelling so the future exact unique index does
        // not admit equivalent `(issuer, subject)` pairs.
        let canonical = parsed.as_str().trim_end_matches('/').to_owned();
        debug_assert!(canonical.len() <= MAX_ISSUER_BYTES);
        Ok(Self(canonical))
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
        // JSON Schema can validate the accepted raw URL shape but cannot
        // express `url::Url` canonicalization. Transaction-owned admission
        // must persist the typed, reserialized value.
        schemars::json_schema!({
            "type": "string",
            "minLength": 1,
            "maxLength": 1024,
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
        schemars::json_schema!({
            "type": "string",
            "minLength": 1,
            "maxLength": 255,
            "pattern": ".*\\S.*"
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

/// A UID-bound, version-independent reference to a User resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserReference {
    kind: ResourceKind,
    uid: Uuid,
}

impl UserReference {
    pub fn new(uid: Uuid) -> Result<Self, ValidationError> {
        if uid.is_nil() {
            return Err(ValidationError::new("a User reference UID must not be nil"));
        }
        Ok(Self {
            kind: ResourceKind::new("rise.dev", USER_KIND)
                .expect("the built-in User ResourceKind is valid"),
            uid,
        })
    }

    pub fn kind(&self) -> &ResourceKind {
        &self.kind
    }

    pub fn uid(&self) -> Uuid {
        self.uid
    }
}

impl<'de> Deserialize<'de> for UserReference {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Repr {
            kind: ResourceKind,
            uid: String,
        }

        let Repr { kind, uid } = Repr::deserialize(deserializer)?;
        if kind.group() != "rise.dev" || kind.kind() != USER_KIND {
            return Err(serde::de::Error::custom(
                "userRef.kind must be 'rise.dev/User'",
            ));
        }
        let parsed_uid = Uuid::parse_str(&uid).map_err(serde::de::Error::custom)?;
        if parsed_uid.hyphenated().to_string() != uid {
            return Err(serde::de::Error::custom(
                "userRef.uid must be a canonical lowercase hyphenated UUID",
            ));
        }
        if parsed_uid.is_nil() {
            return Err(serde::de::Error::custom(
                "userRef.uid must not be a nil UUID",
            ));
        }
        Ok(Self {
            kind,
            uid: parsed_uid,
        })
    }
}

impl JsonSchema for UserReference {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "UserReference".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "object",
            "properties": {
                "kind": { "const": "rise.dev/User" },
                "uid": {
                    "type": "string",
                    "format": "uuid",
                    "pattern": "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$",
                    "not": { "const": "00000000-0000-0000-0000-000000000000" }
                }
            },
            "required": ["kind", "uid"],
            "additionalProperties": false
        })
    }
}

/// One boolean membership edge from the parent Group to a User.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GroupMembershipSpec {
    pub user_ref: UserReference,
}

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
