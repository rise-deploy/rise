use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Request to create a new service account
#[derive(Debug, Deserialize)]
pub struct CreateServiceAccountRequest {
    pub issuer_url: String,
    pub claims: HashMap<String, String>,
    /// Restrict this service account to specific environments (by name).
    /// None or absent = all environments allowed.
    pub allowed_environments: Option<Vec<String>>,
}

/// Request to update an existing service account
#[derive(Debug, Deserialize)]
pub struct UpdateServiceAccountRequest {
    pub issuer_url: Option<String>,
    pub claims: Option<HashMap<String, String>>,
    /// Update environment restrictions.
    /// Absent = don't change. Some(None) / Some([]) = clear restriction (all environments).
    /// Some(["env1", "env2"]) = restrict to those environments.
    pub allowed_environments: Option<Option<Vec<String>>>,
}

/// Response for a single service account
#[derive(Debug, Serialize)]
pub struct ServiceAccountResponse {
    pub id: String,
    pub email: String,
    pub project_name: String,
    pub issuer_url: String,
    pub claims: HashMap<String, String>,
    /// Environment names this SA is allowed to deploy to. None = all environments.
    pub allowed_environments: Option<Vec<String>>,
    pub created_at: String,
}

/// Response for listing service accounts
#[derive(Debug, Serialize)]
pub struct ListServiceAccountsResponse {
    pub service_accounts: Vec<ServiceAccountResponse>,
}
