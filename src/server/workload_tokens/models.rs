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
