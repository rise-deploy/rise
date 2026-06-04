//! Request / response and error shapes for the RFC 8693 token-exchange endpoint.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

/// RFC 8693 grant type for token exchange.
pub const GRANT_TYPE_TOKEN_EXCHANGE: &str = "urn:ietf:params:oauth:grant-type:token-exchange";
/// RFC 8693 token type for a JWT subject / issued token.
pub const TOKEN_TYPE_JWT: &str = "urn:ietf:params:oauth:token-type:jwt";

/// Maximum accepted `subject_token` length (bytes), to blunt oversized-token
/// CPU/DoS before any parsing.
pub const MAX_SUBJECT_TOKEN_LEN: usize = 8 * 1024;

/// Request body for `POST /api/v1/auth/token` (RFC 8693, with one Rise field).
#[derive(Debug, Deserialize)]
pub struct ExchangeRequest {
    /// Must be [`GRANT_TYPE_TOKEN_EXCHANGE`].
    pub grant_type: String,
    /// The source OIDC JWT being exchanged.
    pub subject_token: String,
    /// Must be [`TOKEN_TYPE_JWT`].
    pub subject_token_type: String,
    /// Optional Rise project name; required for project service-account exchange.
    /// `resource` is accepted as an alias for strict RFC 8693 compatibility.
    #[serde(default, alias = "resource")]
    pub rise_project: Option<String>,
}

/// Success response (RFC 8693 §2.2.1).
#[derive(Debug, Serialize)]
pub struct ExchangeResponse {
    /// The minted Rise access token (HS256).
    pub access_token: String,
    /// Always `Bearer`.
    pub token_type: String,
    /// Always [`TOKEN_TYPE_JWT`].
    pub issued_token_type: String,
    /// Token lifetime in seconds.
    pub expires_in: u64,
}

/// OAuth-style error body (RFC 6749 §5.2 / RFC 8693 §2.4).
#[derive(Debug, Serialize)]
pub struct OAuthErrorBody {
    pub error: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_description: Option<String>,
}

/// A small OAuth error value. Carried in `Err` variants (rather than a full
/// `Response`) and rendered via [`IntoResponse`] to either an OAuth JSON error
/// body or a `429` rate-limit response.
#[derive(Debug)]
pub enum ExchangeError {
    /// An OAuth error (rendered as a JSON `{error, error_description}` body).
    OAuth {
        status: StatusCode,
        error: &'static str,
        description: Option<String>,
    },
    /// Rate limited — rendered as a `429` with `Retry-After`.
    RateLimited { retry_after: u64 },
}

impl ExchangeError {
    /// `400 invalid_request` — malformed request (missing/duplicate parameters,
    /// unsupported token type, oversized token).
    pub fn invalid_request(desc: impl Into<String>) -> Self {
        Self::OAuth {
            status: StatusCode::BAD_REQUEST,
            error: "invalid_request",
            description: Some(desc.into()),
        }
    }

    /// `400 invalid_grant` — the subject token is not acceptable (bad signature /
    /// expiry / issuer guard, no matching SA, controller-as-SA, ambiguous match).
    /// Descriptions are deliberately coarse to avoid leaking unknown-issuer vs
    /// no-match distinctions.
    pub fn invalid_grant(desc: impl Into<String>) -> Self {
        Self::OAuth {
            status: StatusCode::BAD_REQUEST,
            error: "invalid_grant",
            description: Some(desc.into()),
        }
    }

    /// `400 invalid_target` — the requested `rise_project` does not exist.
    pub fn invalid_target(desc: impl Into<String>) -> Self {
        Self::OAuth {
            status: StatusCode::BAD_REQUEST,
            error: "invalid_target",
            description: Some(desc.into()),
        }
    }

    /// `503 temporarily_unavailable` — a transient dependency failure (JWKS
    /// fetch / network), so the caller should retry.
    pub fn temporarily_unavailable(desc: impl Into<String>) -> Self {
        Self::OAuth {
            status: StatusCode::SERVICE_UNAVAILABLE,
            error: "temporarily_unavailable",
            description: Some(desc.into()),
        }
    }

    /// `429 Too Many Requests` with a `Retry-After` hint.
    pub fn rate_limited(retry_after: u64) -> Self {
        Self::RateLimited { retry_after }
    }
}

impl IntoResponse for ExchangeError {
    fn into_response(self) -> Response {
        match self {
            ExchangeError::OAuth {
                status,
                error,
                description,
            } => (
                status,
                Json(OAuthErrorBody {
                    error,
                    error_description: description,
                }),
            )
                .into_response(),
            ExchangeError::RateLimited { retry_after } => {
                crate::server::rate_limit::rate_limit_response(retry_after)
            }
        }
    }
}
