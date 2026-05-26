use serde::{Deserialize, Serialize};

/// Request to add a custom domain
#[derive(Debug, Deserialize)]
pub struct AddCustomDomainRequest {
    pub domain: String,
    /// Environment name to attach the domain to. Defaults to the project's
    /// production environment when omitted.
    #[serde(default)]
    pub environment: Option<String>,
}

/// API response for a single custom domain
#[derive(Debug, Serialize)]
pub struct CustomDomainResponse {
    pub id: String,
    pub domain: String,
    pub is_primary: bool,
    pub environment: String,
    pub environment_id: String,
    pub created_at: String,
    pub updated_at: String,
}

impl CustomDomainResponse {
    /// Create response from database model
    pub fn from_db_model(domain: &crate::db::models::CustomDomain, environment_name: &str) -> Self {
        Self {
            id: domain.id.to_string(),
            domain: domain.domain.clone(),
            is_primary: domain.is_primary,
            environment: environment_name.to_string(),
            environment_id: domain.environment_id.to_string(),
            created_at: domain.created_at.to_rfc3339(),
            updated_at: domain.updated_at.to_rfc3339(),
        }
    }
}

/// Response containing multiple custom domains
#[derive(Debug, Serialize)]
pub struct CustomDomainsResponse {
    pub domains: Vec<CustomDomainResponse>,
}
