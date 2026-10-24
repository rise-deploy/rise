use serde::{Deserialize, Serialize};

/// Request to set or update an environment variable
#[derive(Debug, Deserialize)]
pub struct SetEnvVarRequest {
    pub value: String,
    #[serde(default)]
    pub is_secret: bool,
    /// When omitted, defaults to the same value as is_secret (secrets are protected by default)
    pub is_protected: Option<bool>,
}

/// Request to move an environment variable to a different environment
#[derive(Debug, Deserialize)]
pub struct MoveEnvVarRequest {
    /// Source environment name (null for global)
    pub from_environment: Option<String>,
    /// Target environment name (null for global)
    pub to_environment: Option<String>,
}

/// API response for a single environment variable
/// Secrets are always masked in the response unless explicitly decrypted
#[derive(Debug, Serialize)]
pub struct EnvVarResponse {
    pub key: String,
    pub value: String, // Masked as "••••••••" if is_secret = true (unless decrypted)
    pub is_secret: bool,
    pub is_protected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl EnvVarResponse {
    /// Create response from database model, masking secrets
    pub fn from_db_model(
        key: String,
        value: String,
        is_secret: bool,
        is_protected: bool,
        source: Option<String>,
    ) -> Self {
        let displayed_value = if is_secret {
            "••••••••".to_string()
        } else {
            value
        };

        Self {
            key,
            value: displayed_value,
            is_secret,
            is_protected,
            environment: None,
            source,
        }
    }
}

/// Response containing multiple environment variables
#[derive(Debug, Serialize)]
pub struct EnvVarsResponse {
    pub env_vars: Vec<EnvVarResponse>,
}

/// Response for retrieving a single secret value
#[derive(Debug, Serialize)]
pub struct EnvVarValueResponse {
    pub value: String,
}

/// How a variable differs between a deployment's snapshot and what a new
/// deployment would get now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvVarChange {
    Added,
    Removed,
    Changed,
    Unchanged,
    /// Present on both sides but not comparable for this caller: a secret it
    /// may not read, or one that could not be decrypted.
    Unknown,
}

/// One variable in a snapshot-versus-now comparison. Values are included only
/// for plain variables; a secret's entry says whether it changed, never what
/// it is.
#[derive(Debug, Serialize)]
pub struct EnvVarDiffEntry {
    pub key: String,
    pub change: EnvVarChange,
    pub is_secret: bool,
    pub is_protected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// Where the value comes from on each side: `global`, `env:<name>`,
    /// `toml` or `cli`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_source: Option<String>,
}

/// Response for comparing a deployment's variables with the current ones
#[derive(Debug, Serialize)]
pub struct EnvVarDiffResponse {
    /// The environment the comparison was made against (`None` = Global only).
    pub environment: Option<String>,
    pub changes: Vec<EnvVarDiffEntry>,
}
