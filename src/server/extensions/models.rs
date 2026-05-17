use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Extension API response model
#[derive(Debug, Serialize, Deserialize)]
pub struct Extension {
    /// Extension instance name (configurable)
    pub extension: String,
    /// Extension type (constant identifier for UI registry lookup)
    pub extension_type: String,
    pub spec: Value,
    pub status: Value,
    /// Human-readable status summary formatted by the extension provider
    pub status_summary: String,
    pub created: String,
    pub updated: String,
    /// Whether the extension has been marked for deletion
    pub deleted: bool,
}

/// Request to create an extension
#[derive(Debug, Serialize, Deserialize)]
pub struct CreateExtensionRequest {
    /// Extension type (handler identifier, e.g., "aws-rds-provisioner", "oauth")
    pub extension_type: String,
    pub spec: Value,
}

/// Response after creating an extension
#[derive(Debug, Serialize, Deserialize)]
pub struct CreateExtensionResponse {
    pub extension: Extension,
}

/// Request to update an extension
#[derive(Debug, Serialize, Deserialize)]
pub struct UpdateExtensionRequest {
    pub spec: Value,
}

/// Response after updating an extension
#[derive(Debug, Serialize, Deserialize)]
pub struct UpdateExtensionResponse {
    pub extension: Extension,
}

/// Response listing extensions
#[derive(Debug, Serialize, Deserialize)]
pub struct ListExtensionsResponse {
    pub extensions: Vec<Extension>,
}

/// Metadata about an available extension type
#[derive(Debug, Serialize, Deserialize)]
pub struct ExtensionTypeMetadata {
    /// Extension type (constant identifier for UI registry lookup, e.g., "aws-rds-provisioner")
    pub extension_type: String,
    /// Human-readable display name (e.g., "AWS RDS Database")
    pub display_name: String,
    /// Human-readable description (longer, more detailed)
    pub description: String,
    /// Full documentation (markdown)
    pub documentation: String,
    /// JSON schema for the spec
    pub spec_schema: Value,
}

/// Response listing available extension types
#[derive(Debug, Serialize, Deserialize)]
pub struct ListExtensionTypesResponse {
    pub extension_types: Vec<ExtensionTypeMetadata>,
}
