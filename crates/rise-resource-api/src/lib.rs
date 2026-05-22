use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

pub const API_VERSION_V1ALPHA1: &str = "rise.dev/v1alpha1";

pub const ORGANIZATION_KIND: &str = "Organization";
pub const ORGANIZATION_COLLECTION: &str = "organizations";

pub const RESOURCE_DEFINITION_KIND: &str = "ResourceDefinition";
pub const RESOURCE_DEFINITION_COLLECTION: &str = "resourcedefinitions";

pub const RESERVED_COLLECTION_NAMES: &[&str] = &[
    ORGANIZATION_COLLECTION,
    RESOURCE_DEFINITION_COLLECTION,
    "projects",
    "users",
    "teams",
    "environments",
    "deployments",
    "serviceaccounts",
];

pub type JsonObject = BTreeMap<String, serde_json::Value>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Resource<TSpec: Default = JsonObject, TStatus: Default = JsonObject> {
    pub api_version: String,
    pub kind: String,
    pub metadata: ResourceMetadata,
    #[serde(default)]
    pub spec: TSpec,
    #[serde(default)]
    pub status: TStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct ResourceMetadata {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discriminator: Option<String>,
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
    #[serde(default)]
    pub finalizers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletion_timestamp: Option<DateTime<Utc>>,
}

pub type ResourceList<TSpec = JsonObject, TStatus = JsonObject> = Vec<Resource<TSpec, TStatus>>;
pub type ResourceResponse<TSpec = JsonObject, TStatus = JsonObject> = Resource<TSpec, TStatus>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateResourceRequest<TSpec: Default = JsonObject> {
    pub api_version: String,
    pub kind: String,
    pub metadata: CreateResourceMetadata,
    #[serde(default)]
    pub spec: TSpec,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateResourceMetadata {
    pub name: String,
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
    #[serde(default)]
    pub finalizers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateResourceRequest<TSpec: Default = JsonObject> {
    pub api_version: String,
    pub kind: String,
    pub metadata: UpdateResourceMetadata,
    #[serde(default)]
    pub spec: TSpec,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateResourceMetadata {
    pub name: String,
    pub revision: i64,
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
    #[serde(default)]
    pub finalizers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct ControllerStatusMap {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub controllers: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct OrganizationSpec {
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment_controller_class: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct OrganizationStatus {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub controllers: BTreeMap<String, serde_json::Value>,
}

pub type OrganizationResource = Resource<OrganizationSpec, OrganizationStatus>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct ResourceDefinitionSpec {
    pub group: String,
    pub kind: String,
    pub plural: String,
    /// The resource is root-scoped when this is absent; otherwise it must live
    /// under a parent of the referenced API group and kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<ResourceParentRef>,
    pub versions: Vec<ResourceDefinitionVersion>,
    #[serde(default)]
    pub allowed_status_controller_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResourceParentRef {
    pub api_version: String,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResourceDefinitionVersion {
    pub name: String,
    pub served: bool,
    pub storage: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct ResourceDefinitionStatus {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub controllers: BTreeMap<String, serde_json::Value>,
}

pub type ResourceDefinitionResource = Resource<ResourceDefinitionSpec, ResourceDefinitionStatus>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    message: String,
}

impl ValidationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ValidationError {}

pub fn validate_resource_name(value: &str) -> Result<(), ValidationError> {
    validate_dns_label(value, 63, "resource name")
}

pub fn validate_discriminator(value: &str) -> Result<(), ValidationError> {
    if value.len() != 8 {
        return Err(ValidationError::new(
            "discriminator must be exactly 8 characters",
        ));
    }
    validate_dns_label(value, 8, "discriminator")
}

pub fn validate_collection_name(value: &str) -> Result<(), ValidationError> {
    validate_dns_label(value, 63, "collection name")
}

pub fn is_reserved_collection_name(value: &str) -> bool {
    RESERVED_COLLECTION_NAMES
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(value))
}

pub fn validate_resource_group(value: &str) -> Result<(), ValidationError> {
    validate_dns_subdomain(value, "resource group")
}

pub fn validate_resource_kind(value: &str) -> Result<(), ValidationError> {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return Err(ValidationError::new("resource kind must not be empty"));
    };
    if !first.is_ascii_uppercase() {
        return Err(ValidationError::new(
            "resource kind must start with an uppercase ASCII letter",
        ));
    }
    if !chars.all(|c| c.is_ascii_alphanumeric()) {
        return Err(ValidationError::new(
            "resource kind must contain only ASCII letters and digits",
        ));
    }
    Ok(())
}

pub fn validate_resource_version(value: &str) -> Result<(), ValidationError> {
    if value.is_empty() {
        return Err(ValidationError::new("resource version must not be empty"));
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    {
        return Err(ValidationError::new(
            "resource version must contain only lowercase ASCII letters and digits",
        ));
    }
    Ok(())
}

pub fn validate_controller_id(value: &str) -> Result<(), ValidationError> {
    let (subdomain, path) = value
        .split_once('/')
        .map_or((value, None), |(left, right)| (left, Some(right)));
    validate_dns_subdomain(subdomain, "controller ID domain")?;
    if let Some(path) = path {
        validate_qualified_name_path(path, "controller ID path")?;
    }
    Ok(())
}

fn validate_dns_subdomain(value: &str, label: &str) -> Result<(), ValidationError> {
    if value.is_empty() || value.len() > 253 {
        return Err(ValidationError::new(format!(
            "{label} must be between 1 and 253 characters"
        )));
    }
    for part in value.split('.') {
        validate_dns_label(part, 63, label)?;
    }
    Ok(())
}

fn validate_dns_label(value: &str, max_len: usize, label: &str) -> Result<(), ValidationError> {
    if value.is_empty() || value.len() > max_len {
        return Err(ValidationError::new(format!(
            "{label} must be between 1 and {max_len} characters"
        )));
    }
    let bytes = value.as_bytes();
    if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit() {
        return Err(ValidationError::new(format!(
            "{label} must start with a lowercase ASCII letter or digit"
        )));
    }
    if !bytes[bytes.len() - 1].is_ascii_lowercase() && !bytes[bytes.len() - 1].is_ascii_digit() {
        return Err(ValidationError::new(format!(
            "{label} must end with a lowercase ASCII letter or digit"
        )));
    }
    if !bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
    {
        return Err(ValidationError::new(format!(
            "{label} must contain only lowercase ASCII letters, digits, and hyphens"
        )));
    }
    Ok(())
}

fn validate_qualified_name_path(value: &str, label: &str) -> Result<(), ValidationError> {
    if value.is_empty() || value.len() > 63 {
        return Err(ValidationError::new(format!(
            "{label} must be between 1 and 63 characters"
        )));
    }
    let bytes = value.as_bytes();
    if !bytes[0].is_ascii_alphanumeric() || !bytes[bytes.len() - 1].is_ascii_alphanumeric() {
        return Err(ValidationError::new(format!(
            "{label} must start and end with an ASCII letter or digit"
        )));
    }
    if !bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || matches!(*b, b'-' | b'_' | b'.'))
    {
        return Err(ValidationError::new(format!(
            "{label} must contain only ASCII letters, digits, hyphens, underscores, and dots"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_resource_names() {
        assert!(validate_resource_name("my-app").is_ok());
        assert!(validate_resource_name("app1").is_ok());
        assert!(validate_resource_name("-app").is_err());
        assert!(validate_resource_name("app-").is_err());
        assert!(validate_resource_name("MyApp").is_err());
    }

    #[test]
    fn validates_discriminators() {
        assert!(validate_discriminator("abc123de").is_ok());
        assert!(validate_discriminator("-bc123de").is_err());
        assert!(validate_discriminator("abc123d-").is_err());
        assert!(validate_discriminator("abc123d").is_err());
        assert!(validate_discriminator("abc123def").is_err());
    }

    #[test]
    fn detects_reserved_collection_names() {
        assert!(is_reserved_collection_name("organizations"));
        assert!(is_reserved_collection_name("resourcedefinitions"));
        assert!(is_reserved_collection_name("Projects"));
        assert!(!is_reserved_collection_name("widgets"));
    }

    #[test]
    fn validates_controller_ids() {
        assert!(validate_controller_id("controller.example.com").is_ok());
        assert!(validate_controller_id("controller.example.com/my-controller").is_ok());
        assert!(validate_controller_id("Controller.example.com").is_err());
        assert!(validate_controller_id("controller.example.com/-bad").is_err());
    }

    #[test]
    fn serializes_organization_resource_shape() {
        let resource = OrganizationResource {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: ORGANIZATION_KIND.to_string(),
            metadata: ResourceMetadata {
                name: "default".to_string(),
                ..ResourceMetadata::default()
            },
            spec: OrganizationSpec {
                display_name: "Default".to_string(),
                deployment_controller_class: Some("kubernetes.rise.dev/default".to_string()),
            },
            status: OrganizationStatus::default(),
        };

        let value = serde_json::to_value(resource).unwrap();
        assert_eq!(value["apiVersion"], API_VERSION_V1ALPHA1);
        assert_eq!(value["kind"], ORGANIZATION_KIND);
        assert_eq!(value["metadata"]["name"], "default");
        assert_eq!(value["spec"]["displayName"], "Default");
    }

    #[test]
    fn defaults_untyped_spec_and_status_to_empty_objects() {
        let resource: Resource = serde_json::from_value(serde_json::json!({
            "apiVersion": "example.dev/v1",
            "kind": "Widget",
            "metadata": {
                "name": "widget-a"
            }
        }))
        .unwrap();

        assert!(resource.spec.is_empty());
        assert!(resource.status.is_empty());

        let value = serde_json::to_value(resource).unwrap();
        assert_eq!(value["spec"], serde_json::json!({}));
        assert_eq!(value["status"], serde_json::json!({}));
    }

    #[test]
    fn skips_empty_controller_status_maps() {
        let controller_status = serde_json::to_value(ControllerStatusMap::default()).unwrap();
        let organization_status = serde_json::to_value(OrganizationStatus::default()).unwrap();
        let resource_definition_status =
            serde_json::to_value(ResourceDefinitionStatus::default()).unwrap();

        assert_eq!(controller_status, serde_json::json!({}));
        assert_eq!(organization_status, serde_json::json!({}));
        assert_eq!(resource_definition_status, serde_json::json!({}));

        let with_controller = OrganizationStatus {
            controllers: BTreeMap::from([(
                "controller.example.com".to_string(),
                serde_json::json!({"ready": true}),
            )]),
        };

        let value = serde_json::to_value(with_controller).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "controllers": {
                    "controller.example.com": {
                        "ready": true
                    }
                }
            })
        );
    }

    #[test]
    fn create_request_rejects_server_controlled_metadata_and_status() {
        let server_controlled_metadata: Result<CreateResourceRequest, _> =
            serde_json::from_value(serde_json::json!({
                "apiVersion": "example.dev/v1",
                "kind": "Widget",
                "metadata": {
                    "name": "widget-a",
                    "uid": "5ac452d8-4e4d-45c9-bd62-5d5aff38095f"
                },
                "spec": {}
            }));

        assert!(server_controlled_metadata.is_err());

        let status: Result<CreateResourceRequest, _> = serde_json::from_value(serde_json::json!({
            "apiVersion": "example.dev/v1",
            "kind": "Widget",
            "metadata": {
                "name": "widget-a"
            },
            "spec": {},
            "status": {}
        }));

        assert!(status.is_err());
    }

    #[test]
    fn update_request_rejects_top_level_status() {
        let result: Result<UpdateResourceRequest, _> = serde_json::from_value(serde_json::json!({
            "apiVersion": "example.dev/v1",
            "kind": "Widget",
            "metadata": {
                "name": "widget-a",
                "revision": 7
            },
            "spec": {},
            "status": {}
        }));

        assert!(result.is_err());
    }

    #[test]
    fn update_request_accepts_revision_but_rejects_other_identity_metadata() {
        let valid: UpdateResourceRequest = serde_json::from_value(serde_json::json!({
            "apiVersion": "example.dev/v1",
            "kind": "Widget",
            "metadata": {
                "name": "widget-a",
                "revision": 7
            },
            "spec": {}
        }))
        .unwrap();
        assert_eq!(valid.metadata.revision, 7);

        let invalid: Result<UpdateResourceRequest, _> = serde_json::from_value(serde_json::json!({
            "apiVersion": "example.dev/v1",
            "kind": "Widget",
            "metadata": {
                "name": "widget-a",
                "revision": 7,
                "discriminator": "abc123de"
            },
            "spec": {}
        }));
        assert!(invalid.is_err());
    }

    #[test]
    fn generates_builtin_schemas() {
        let org_schema = serde_json::to_value(schemars::schema_for!(OrganizationResource)).unwrap();
        let rd_schema =
            serde_json::to_value(schemars::schema_for!(ResourceDefinitionResource)).unwrap();

        assert!(org_schema["properties"]["spec"]["$ref"]
            .as_str()
            .unwrap()
            .contains("OrganizationSpec"));
        assert!(rd_schema["properties"]["spec"]["$ref"]
            .as_str()
            .unwrap()
            .contains("ResourceDefinitionSpec"));
    }
}
