//! Opt-in trace logging of decoded JWT headers and claims, shared by the CLI
//! (the token it presents) and the server (the IdP ID token it receives).
//!
//! The claims carry PII (email, name, groups, `sub`, IdP-specific claims), so the
//! output goes out at `trace` on a dedicated target. A bare `RUST_LOG=debug` does
//! not enable it; a directive naming the target does, e.g.
//! `RUST_LOG=info,rise::auth::idtoken=trace`.

use anyhow::{Context, Result};
use base64::{engine::general_purpose, Engine as _};
use serde_json::Value;

#[derive(Debug)]
struct JwtDebugParts {
    header: Value,
    claims: Value,
}

/// Log the JWT header and claims of `token` at `trace` on the `rise::auth::idtoken` target, without
/// validating or exposing the signature. `label` says where the token came from.
///
/// The token is only decoded when the target is enabled.
pub fn trace_token(label: &str, token: &str) {
    if !tracing::enabled!(target: "rise::auth::idtoken", tracing::Level::TRACE) {
        return;
    }

    match decode_jwt_debug_parts(token) {
        Ok(parts) => tracing::trace!(
            target: "rise::auth::idtoken",
            "{label}: token header.claims is {}.{}",
            parts.header,
            parts.claims
        ),
        Err(error) => tracing::trace!(
            target: "rise::auth::idtoken",
            "{label}: failed to decode token header.claims for trace logging: {error}"
        ),
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

pub(crate) fn decode_jwt_claims(token: &str) -> Result<Value> {
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

    fn trace_enabled(rust_log: &str) -> bool {
        use tracing_subscriber::layer::SubscriberExt;

        let subscriber =
            tracing_subscriber::registry().with(tracing_subscriber::EnvFilter::new(rust_log));
        tracing::subscriber::with_default(
            subscriber,
            || tracing::enabled!(target: "rise::auth::idtoken", tracing::Level::TRACE),
        )
    }

    #[test]
    fn trace_is_opt_in_by_target() {
        assert!(!trace_enabled("info"));
        assert!(!trace_enabled("debug"));
        assert!(!trace_enabled("rise=debug"));
        assert!(trace_enabled("info,rise::auth::idtoken=trace"));
    }
}
