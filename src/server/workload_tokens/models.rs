use serde::{Deserialize, Serialize};

/// Request body for `POST /api/v1/identity/token`.
#[derive(Debug, Deserialize)]
pub struct ExchangeTokenRequest {
    /// Audience (`aud` claim) to mint the token for.
    pub audience: String,
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
}
