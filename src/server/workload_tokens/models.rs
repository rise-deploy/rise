use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Request body for `POST /api/v1/identity/token`.
#[derive(Debug, Deserialize)]
pub struct ExchangeTokenRequest {
    /// Audience (`aud` claim) to mint the token for.
    pub audience: String,
    /// Requested token lifetime in seconds. Capped at the server's configured maximum
    /// (`workload_token_max_ttl_seconds`, default 900). Omitting this field uses the maximum.
    pub ttl_seconds: Option<u64>,
}

/// Response from `POST /api/v1/identity/token`.
#[derive(Debug, Serialize)]
pub struct ExchangeTokenResponse {
    /// The signed workload identity JWT.
    pub token: String,
    /// Always `Bearer`.
    pub token_type: String,
    /// Token lifetime in seconds.
    pub expires_in: u64,
    /// The audience the token was minted for.
    pub audience: String,
}

/// Response from `POST /api/v1/identity/audience-tokens`.
#[derive(Debug, Serialize)]
pub struct AudienceTokensResponse {
    /// In-container token filename → signed workload JWT, one per declared
    /// `[identity].audiences` entry. Empty when the deployment declares none.
    pub tokens: BTreeMap<String, String>,
    /// Lifetime of every token in `tokens`, in seconds.
    pub expires_in: u64,
    /// When the caller should fetch fresh tokens, in seconds from now.
    pub refresh_after_secs: u64,
}
