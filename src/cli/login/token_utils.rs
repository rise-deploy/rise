use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use jsonwebtoken::dangerous::insecure_decode;
use serde::Deserialize;

pub(crate) use crate::token_trace::decode_jwt_claims;

#[derive(Debug, Deserialize)]
struct TokenClaims {
    exp: usize,
}

/// Extract the `exp` (seconds since epoch) from a JWT without validating its
/// signature. Returns `None` if the token can't be decoded.
pub fn read_token_exp(token: &str) -> Option<i64> {
    jsonwebtoken::dangerous::insecure_decode::<TokenClaims>(token)
        .ok()
        .map(|data| data.claims.exp as i64)
}

/// Extract expiration from JWT token and format it as a human-readable string
/// Returns formatted string like "December 16, 2025 at 14:30 UTC (in 7 days)"
pub fn format_token_expiration(token: &str) -> Result<String> {
    // Decode token WITHOUT validation so we can inspect the expiration claim.
    // The backend has already validated the token before this point.
    let token_data = insecure_decode::<TokenClaims>(token)
        .context("Failed to decode token to read expiration")?;

    // Convert UNIX timestamp to DateTime
    let exp_timestamp = token_data.claims.exp as i64;
    let exp_datetime = DateTime::<Utc>::from_timestamp(exp_timestamp, 0)
        .ok_or_else(|| anyhow::anyhow!("Invalid expiration timestamp"))?;

    // Format absolute date/time
    let formatted_date = exp_datetime.format("%B %d, %Y at %H:%M UTC");

    // Calculate relative duration
    let now = Utc::now();
    let duration = exp_datetime.signed_duration_since(now);

    let relative = if duration.num_days() > 0 {
        let days = duration.num_days();
        if days == 1 {
            "in 1 day".to_string()
        } else {
            format!("in {} days", days)
        }
    } else if duration.num_hours() > 0 {
        let hours = duration.num_hours();
        if hours == 1 {
            "in 1 hour".to_string()
        } else {
            format!("in {} hours", hours)
        }
    } else if duration.num_minutes() > 0 {
        let minutes = duration.num_minutes();
        if minutes == 1 {
            "in 1 minute".to_string()
        } else {
            format!("in {} minutes", minutes)
        }
    } else {
        "expires soon".to_string()
    };

    Ok(format!("{} ({})", formatted_date, relative))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_segment(json: &str) -> String {
        use base64::{engine::general_purpose, Engine as _};
        general_purpose::URL_SAFE_NO_PAD.encode(json)
    }

    #[test]
    fn format_token_expiration_reads_exp_without_signature_validation() {
        let token = format!(
            "{}.{}.invalid-signature",
            encode_segment(r#"{"alg":"RS256","typ":"JWT"}"#),
            encode_segment(r#"{"exp":4102444800}"#),
        );

        let expiration = format_token_expiration(&token).expect("expiration should decode");

        assert!(expiration.starts_with("January 01, 2100 at 00:00 UTC"));
    }
}
