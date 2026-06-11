use anyhow::{Context, Result};
use base64::{engine::general_purpose, Engine as _};
use chrono::{DateTime, Utc};
use jsonwebtoken::dangerous::insecure_decode;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug)]
struct JwtDebugParts {
    header: Value,
    claims: Value,
}

/// Log the JWT header and claims at debug level without exposing the signature.
pub fn log_token_debug(token: &str, source: &str) {
    if !tracing::enabled!(tracing::Level::DEBUG) {
        return;
    }

    tracing::debug!("Using token from {source}");

    match decode_jwt_debug_parts(token) {
        Ok(parts) => tracing::debug!("Token header.claims is {}.{}", parts.header, parts.claims),
        Err(error) => {
            tracing::debug!("Failed to decode token header.claims for debug logging: {error}")
        }
    }
}

fn decode_jwt_debug_parts(token: &str) -> Result<JwtDebugParts> {
    let header = jsonwebtoken::decode_header(token).context("Failed to decode token header")?;
    let mut header = serde_json::to_value(header).context("Failed to serialize token header")?;
    if let Value::Object(header_obj) = &mut header {
        header_obj.retain(|_, value| !value.is_null());
    }
    let claims = decode_jwt_claims(token)?;

    Ok(JwtDebugParts { header, claims })
}

fn decode_jwt_claims(token: &str) -> Result<Value> {
    let mut parts = token.split('.');
    let _header = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("Token is missing JWT header"))?;
    let claims = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("Token is missing JWT claims"))?;

    let decoded = general_purpose::URL_SAFE_NO_PAD
        .decode(claims)
        .or_else(|_| general_purpose::URL_SAFE.decode(claims))
        .context("Failed to base64url-decode token claims")?;

    serde_json::from_slice(&decoded).context("Failed to parse token claims JSON")
}

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

/// Extract the `iss` (issuer) claim from a JWT without validating its signature
/// or expiry. Returns `None` if the token isn't a decodable JWT or has no `iss`.
///
/// Used to tell a Rise-issued session token (whose `iss` is the backend URL)
/// apart from an external OIDC token, so only the latter is sent to the
/// token-exchange endpoint.
pub fn read_token_issuer(token: &str) -> Option<String> {
    decode_jwt_claims(token)
        .ok()?
        .get("iss")
        .and_then(Value::as_str)
        .map(str::to_string)
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
        general_purpose::URL_SAFE_NO_PAD.encode(json)
    }

    #[test]
    fn decode_jwt_debug_parts_reads_header_and_claims_without_signature() {
        let token = format!(
            "{}.{}.signature",
            encode_segment(r#"{"alg":"RS256","typ":"JWT"}"#),
            encode_segment(r#"{"sub":"service-account","aud":"demo-project"}"#),
        );

        let parts = decode_jwt_debug_parts(&token).expect("token should decode");

        assert_eq!(parts.header["alg"], "RS256");
        assert_eq!(parts.header["typ"], "JWT");
        assert_eq!(parts.claims["sub"], "service-account");
        assert_eq!(parts.claims["aud"], "demo-project");
    }

    #[test]
    fn token_header_claims_string_matches_expected_format() {
        let token = format!(
            "{}.{}.signature",
            encode_segment(r#"{"alg":"RS256"}"#),
            encode_segment(r#"{"sub":"service-account"}"#),
        );

        let parts = decode_jwt_debug_parts(&token).expect("token should decode");
        let combined = format!("{}.{}", parts.header, parts.claims);

        assert_eq!(combined, r#"{"alg":"RS256"}.{"sub":"service-account"}"#);
    }

    #[test]
    fn decode_jwt_debug_parts_rejects_invalid_claims() {
        let token = format!(
            "{}.{}.signature",
            encode_segment(r#"{"alg":"RS256","typ":"JWT"}"#),
            encode_segment("not-json"),
        );

        let error = decode_jwt_debug_parts(&token).expect_err("claims should fail to decode");
        assert!(error
            .to_string()
            .contains("Failed to parse token claims JSON"));
    }

    #[test]
    fn read_token_issuer_reads_iss_without_validation() {
        let token = format!(
            "{}.{}.signature",
            encode_segment(r#"{"alg":"HS256","typ":"JWT"}"#),
            encode_segment(r#"{"iss":"https://rise.example.com","exp":0}"#),
        );
        assert_eq!(
            read_token_issuer(&token).as_deref(),
            Some("https://rise.example.com")
        );
        // No `iss` claim, and an opaque (non-JWT) value, both yield `None`.
        let no_iss = format!(
            "{}.{}.sig",
            encode_segment(r#"{"alg":"HS256"}"#),
            encode_segment(r#"{"sub":"x"}"#),
        );
        assert_eq!(read_token_issuer(&no_iss), None);
        assert_eq!(read_token_issuer("not-a-jwt"), None);
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
