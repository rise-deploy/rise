use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::builtin_kind::ORGANIZATION_PARENT;
use crate::{
    validate_resource_group, validate_resource_name, BuiltInKindDefinition, ResourceKind, Scope,
    SubjectId, ValidationError, API_VERSION_V1ALPHA1, PLATFORM_ROLE_BINDING_COLLECTION,
    PLATFORM_ROLE_BINDING_KIND, PLATFORM_ROLE_COLLECTION, PLATFORM_ROLE_KIND,
    ROLE_BINDING_COLLECTION, ROLE_BINDING_KIND, ROLE_COLLECTION, ROLE_KIND,
};

/// All persisted ADR-0001 policy kinds and their fixed placement.
///
/// The parent model is exact, so each policy object comes as an
/// organization-parented and a root-parented kind rather than one kind with a
/// choice of parents (ADR-0001 §3).
pub const POLICY_KIND_DEFINITIONS: [BuiltInKindDefinition; 4] = [
    BuiltInKindDefinition {
        api_version: API_VERSION_V1ALPHA1,
        kind: ROLE_KIND,
        collection: ROLE_COLLECTION,
        parent: Some(ORGANIZATION_PARENT),
    },
    BuiltInKindDefinition {
        api_version: API_VERSION_V1ALPHA1,
        kind: ROLE_BINDING_KIND,
        collection: ROLE_BINDING_COLLECTION,
        parent: Some(ORGANIZATION_PARENT),
    },
    BuiltInKindDefinition {
        api_version: API_VERSION_V1ALPHA1,
        kind: PLATFORM_ROLE_KIND,
        collection: PLATFORM_ROLE_COLLECTION,
        parent: None,
    },
    BuiltInKindDefinition {
        api_version: API_VERSION_V1ALPHA1,
        kind: PLATFORM_ROLE_BINDING_KIND,
        collection: PLATFORM_ROLE_BINDING_COLLECTION,
        parent: None,
    },
];

macro_rules! string_serde {
    ($type:ty) => {
        impl Serialize for $type {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.to_string())
            }
        }

        impl<'de> Deserialize<'de> for $type {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                String::deserialize(deserializer)?
                    .parse()
                    .map_err(serde::de::Error::custom)
            }
        }
    };
}

macro_rules! matcher_serde {
    ($type:ty, $variant:ident, $item:ty, $field:literal) => {
        impl Serialize for $type {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                match self {
                    Self::All => serializer.serialize_str("*"),
                    Self::$variant(values) => values.serialize(serializer),
                }
            }
        }

        impl<'de> Deserialize<'de> for $type {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                #[derive(Deserialize)]
                #[serde(untagged)]
                enum Repr<T> {
                    Wildcard(String),
                    Values(Vec<T>),
                }

                match Repr::<$item>::deserialize(deserializer)? {
                    Repr::Wildcard(value) if value == "*" => Ok(Self::All),
                    Repr::Wildcard(_) => Err(serde::de::Error::custom(concat!(
                        $field,
                        " wildcard must be '*'"
                    ))),
                    Repr::Values(values) if values.is_empty() => Err(serde::de::Error::custom(
                        concat!($field, " must not be empty"),
                    )),
                    Repr::Values(values) => {
                        let original_len = values.len();
                        let values: BTreeSet<$item> = values.into_iter().collect();
                        if values.len() != original_len {
                            return Err(serde::de::Error::custom(concat!(
                                $field,
                                " must not contain duplicates"
                            )));
                        }
                        Ok(Self::$variant(values))
                    }
                }
            }
        }
    };
}

/// The closed verb vocabulary used by Role statements and authorization requests.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Verb {
    Get,
    List,
    Create,
    Update,
    Delete,
    Use,
}

/// A Role statement's outcome when it matches an authorization tuple.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum Effect {
    Allow,
    Deny,
}

/// One exact kind or API-group wildcard inside a Role statement's `kinds` list.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ResourceKindPattern {
    Exact(ResourceKind),
    Group(String),
}

impl ResourceKindPattern {
    pub fn group(&self) -> &str {
        match self {
            Self::Exact(kind) => kind.group(),
            Self::Group(group) => group,
        }
    }
}

impl fmt::Debug for ResourceKindPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ResourceKindPattern")
            .field(&self.to_string())
            .finish()
    }
}

impl fmt::Display for ResourceKindPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exact(kind) => kind.fmt(f),
            Self::Group(group) => write!(f, "{group}/*"),
        }
    }
}

impl FromStr for ResourceKindPattern {
    type Err = ValidationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if let Some(group) = value.strip_suffix("/*") {
            if group.contains('/') {
                return Err(ValidationError::new(
                    "kind pattern must be '<api-group>/<Kind>' or '<api-group>/*'",
                ));
            }
            validate_resource_group(group)?;
            return Ok(Self::Group(group.to_owned()));
        }
        Ok(Self::Exact(value.parse()?))
    }
}

string_serde!(ResourceKindPattern);

impl JsonSchema for ResourceKindPattern {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ResourceKindPattern".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": "^(?=.{1,253}\\/)[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?(?:\\.[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?)*\\/(?:[A-Z][A-Za-z0-9]*|\\*)$",
            "minLength": 3
        })
    }
}

/// A canonical lowercase subresource name. Registration is checked transactionally.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SubresourceName(String);

impl fmt::Debug for SubresourceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SubresourceName").field(&self.0).finish()
    }
}

impl fmt::Display for SubresourceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for SubresourceName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl FromStr for SubresourceName {
    type Err = ValidationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        validate_resource_name(value)?;
        Ok(Self(value.to_owned()))
    }
}

string_serde!(SubresourceName);

impl JsonSchema for SubresourceName {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "SubresourceName".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": "^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$",
            "maxLength": 63
        })
    }
}

/// `"*"` or a non-empty, canonical set of kind patterns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KindMatcher {
    All,
    Patterns(BTreeSet<ResourceKindPattern>),
}

/// `"*"` or a non-empty, canonical set of verbs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerbMatcher {
    All,
    Verbs(BTreeSet<Verb>),
}

/// `"*"` or a non-empty, canonical set of named subresources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubresourceMatcher {
    All,
    Names(BTreeSet<SubresourceName>),
}

matcher_serde!(KindMatcher, Patterns, ResourceKindPattern, "kinds");
matcher_serde!(VerbMatcher, Verbs, Verb, "verbs");
matcher_serde!(SubresourceMatcher, Names, SubresourceName, "subresources");

impl JsonSchema for KindMatcher {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "KindMatcher".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "oneOf": [
                { "const": "*" },
                {
                    "type": "array",
                    "items": {
                        "type": "string",
                        "pattern": "^(?=.{1,253}\\/)[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?(?:\\.[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?)*\\/(?:[A-Z][A-Za-z0-9]*|\\*)$"
                    },
                    "minItems": 1,
                    "uniqueItems": true
                }
            ]
        })
    }
}

impl JsonSchema for VerbMatcher {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "VerbMatcher".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "oneOf": [
                { "const": "*" },
                {
                    "type": "array",
                    "items": { "enum": ["get", "list", "create", "update", "delete", "use"] },
                    "minItems": 1,
                    "uniqueItems": true
                }
            ]
        })
    }
}

impl JsonSchema for SubresourceMatcher {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "SubresourceMatcher".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "oneOf": [
                { "const": "*" },
                {
                    "type": "array",
                    "items": {
                        "type": "string",
                        "pattern": "^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$",
                        "maxLength": 63
                    },
                    "minItems": 1,
                    "uniqueItems": true
                }
            ]
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PolicyStatement {
    pub effect: Effect,
    pub kinds: KindMatcher,
    pub verbs: VerbMatcher,
    /// Omission selects the main resource. `All` selects subresources only.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    #[schemars(with = "SubresourceMatcher")]
    pub subresources: Option<SubresourceMatcher>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RoleSpec {
    #[serde(default)]
    pub statements: Vec<PolicyStatement>,
}

/// Kubernetes-shaped label key syntax used by binding selectors.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LabelKey(String);

impl fmt::Debug for LabelKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("LabelKey").field(&self.0).finish()
    }
}

impl fmt::Display for LabelKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for LabelKey {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl FromStr for LabelKey {
    type Err = ValidationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (prefix, name) = match value.rsplit_once('/') {
            Some((prefix, name)) => {
                validate_resource_group(prefix)?;
                (Some(prefix), name)
            }
            None => (None, value),
        };
        if prefix.is_some_and(str::is_empty)
            || name.is_empty()
            || name.len() > 63
            || !name.as_bytes()[0].is_ascii_alphanumeric()
            || !name.as_bytes()[name.len() - 1].is_ascii_alphanumeric()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            return Err(ValidationError::new("invalid label key"));
        }
        Ok(Self(value.to_owned()))
    }
}

string_serde!(LabelKey);

impl JsonSchema for LabelKey {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "LabelKey".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": "^(?:(?=.{1,253}\\/)[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?(?:\\.[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?)*\\/)?[A-Za-z0-9](?:[A-Za-z0-9_.-]{0,61}[A-Za-z0-9])?$",
            "maxLength": 317
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LabelSelector {
    pub key: LabelKey,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    #[schemars(with = "String")]
    pub value: Option<String>,
}

/// A literal subject or one of ADR-0001's three closed subject templates.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BindingSubject {
    Literal(SubjectId),
    UserNameTemplate,
    GroupNameTemplate,
    SubjectRefTemplate,
}

impl BindingSubject {
    pub fn is_dynamic(&self) -> bool {
        !matches!(self, Self::Literal(_))
    }

    pub fn literal(&self) -> Option<&SubjectId> {
        match self {
            Self::Literal(subject) => Some(subject),
            _ => None,
        }
    }
}

impl fmt::Debug for BindingSubject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("BindingSubject")
            .field(&self.to_string())
            .finish()
    }
}

impl fmt::Display for BindingSubject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Literal(subject) => subject.fmt(f),
            Self::UserNameTemplate => f.write_str("user:${ref.name}"),
            Self::GroupNameTemplate => f.write_str("group:${ref.name}"),
            Self::SubjectRefTemplate => f.write_str("${ref.subject}"),
        }
    }
}

impl FromStr for BindingSubject {
    type Err = ValidationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "user:${ref.name}" => Ok(Self::UserNameTemplate),
            "group:${ref.name}" => Ok(Self::GroupNameTemplate),
            "${ref.subject}" => Ok(Self::SubjectRefTemplate),
            _ if value.contains("${") => Err(ValidationError::new("unknown subject template")),
            _ => Ok(Self::Literal(value.parse()?)),
        }
    }
}

string_serde!(BindingSubject);

impl JsonSchema for BindingSubject {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "BindingSubject".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "oneOf": [
                {
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
                },
                { "enum": ["user:${ref.name}", "group:${ref.name}", "${ref.subject}"] }
            ]
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum SubjectMembership {
    #[default]
    Any,
    ResourceOrganization,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum RoleRefKind {
    Role,
    PlatformRole,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RoleRef {
    pub kind: RoleRefKind,
    #[serde(deserialize_with = "deserialize_resource_name")]
    #[schemars(pattern("^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$"), length(max = 63))]
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlatformRoleRef {
    pub kind: PlatformRoleRefKind,
    #[serde(deserialize_with = "deserialize_resource_name")]
    #[schemars(pattern("^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$"), length(max = 63))]
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum PlatformRoleRefKind {
    PlatformRole,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RoleBindingSpec {
    pub subject: BindingSubject,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    #[schemars(with = "Scope")]
    pub scope: Option<Scope>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    #[schemars(with = "LabelSelector")]
    pub label_selector: Option<LabelSelector>,
    pub role_ref: RoleRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlatformRoleBindingSpec {
    pub subject: BindingSubject,
    #[serde(default)]
    pub subject_membership: SubjectMembership,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    #[schemars(with = "Scope")]
    pub scope: Option<Scope>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    #[schemars(with = "LabelSelector")]
    pub label_selector: Option<LabelSelector>,
    pub role_ref: PlatformRoleRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LocallyNormalizedRoleBindingSpec {
    subject: BindingSubject,
    scope: Scope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "LabelSelector")]
    label_selector: Option<LabelSelector>,
    role_ref: RoleRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LocallyNormalizedPlatformRoleBindingSpec {
    subject: BindingSubject,
    subject_membership: SubjectMembership,
    scope: Scope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "LabelSelector")]
    label_selector: Option<LabelSelector>,
    role_ref: PlatformRoleRef,
}

impl LocallyNormalizedRoleBindingSpec {
    pub fn subject(&self) -> &BindingSubject {
        &self.subject
    }

    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    pub fn label_selector(&self) -> Option<&LabelSelector> {
        self.label_selector.as_ref()
    }

    pub fn role_ref(&self) -> &RoleRef {
        &self.role_ref
    }
}

impl LocallyNormalizedPlatformRoleBindingSpec {
    pub fn subject(&self) -> &BindingSubject {
        &self.subject
    }

    pub fn subject_membership(&self) -> SubjectMembership {
        self.subject_membership
    }

    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    pub fn label_selector(&self) -> Option<&LabelSelector> {
        self.label_selector.as_ref()
    }

    pub fn role_ref(&self) -> &PlatformRoleRef {
        &self.role_ref
    }
}

impl RoleBindingSpec {
    pub fn normalize(
        self,
        parent_organization: &str,
    ) -> Result<LocallyNormalizedRoleBindingSpec, ValidationError> {
        validate_resource_name(parent_organization)?;
        validate_subject_selector(&self.subject, self.label_selector.as_ref())?;
        let scope = match self.scope {
            Some(scope) if scope.is_wildcard() => {
                return Err(ValidationError::new(
                    "an organization RoleBinding cannot use wildcard scope",
                ));
            }
            Some(scope) => scope,
            None => format!("rise.dev/Organization/{parent_organization}").parse()?,
        };
        Ok(LocallyNormalizedRoleBindingSpec {
            subject: self.subject,
            scope,
            label_selector: self.label_selector,
            role_ref: self.role_ref,
        })
    }
}

impl PlatformRoleBindingSpec {
    pub fn normalize(self) -> Result<LocallyNormalizedPlatformRoleBindingSpec, ValidationError> {
        validate_subject_selector(&self.subject, self.label_selector.as_ref())?;
        let static_org = self
            .subject
            .literal()
            .filter(|subject| subject.is_org_native())
            .and_then(SubjectId::organization);
        let scope = match (self.scope, static_org) {
            (Some(scope), Some(_)) if scope.is_wildcard() => {
                return Err(ValidationError::new(
                    "a static org-native subject cannot use wildcard scope",
                ));
            }
            (Some(scope), _) => scope,
            (None, Some(organization)) => {
                format!("rise.dev/Organization/{organization}").parse()?
            }
            (None, None) => Scope::wildcard(),
        };
        Ok(LocallyNormalizedPlatformRoleBindingSpec {
            subject: self.subject,
            subject_membership: self.subject_membership,
            scope,
            label_selector: self.label_selector,
            role_ref: self.role_ref,
        })
    }
}

fn validate_subject_selector(
    subject: &BindingSubject,
    selector: Option<&LabelSelector>,
) -> Result<(), ValidationError> {
    if subject
        .literal()
        .is_some_and(|subject| subject.as_ref() == "system:operators")
    {
        return Err(ValidationError::new(
            "system:operators is reserved for the seeded bootstrap binding",
        ));
    }
    match (subject.is_dynamic(), selector) {
        (true, None) => Err(ValidationError::new(
            "a dynamic subject requires labelSelector",
        )),
        (false, Some(LabelSelector { value: None, .. })) => Err(ValidationError::new(
            "a static subject requires labelSelector.value when a selector is present",
        )),
        _ => Ok(()),
    }
}

fn deserialize_resource_name<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    validate_resource_name(&value).map_err(serde::de::Error::custom)?;
    Ok(value)
}

fn deserialize_optional_non_null<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}
