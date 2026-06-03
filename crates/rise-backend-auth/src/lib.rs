//! Pure-core token signing, verification, and matching for Rise.
//!
//! This crate is the single home for Rise's auth-token logic:
//! - One verification path for Rise-issued JWTs ([`RiseTokenSigner::verify_rise_jwt`]),
//! - One verification path for arbitrary external JWTs ([`verify_external_jwt`]),
//! - Token signing ([`RiseTokenSigner`]),
//! - The pure claim/identity matchers.
//!
//! It is intentionally **pure**: no `reqwest`, `axum`, `sqlx`, `tokio`,
//! `anyhow`, or `regex`. JWKS fetching is abstracted behind the
//! [`JwksKeySource`] trait, implemented by rise-deploy.

mod claims;
mod error;
mod matchers;
mod signer;
mod verify;

pub use claims::{ExternalClaims, RiseClaims, WorkloadClaims, WorkloadSubjectInfo};
pub use error::{AuthError, JwtSignerError};
pub use matchers::{
    audience_matches, build_controller_indexes, match_controller_identity,
    matches_wildcard_pattern, validate_controller_id, validate_custom_claims, validate_oidc_issuer,
    ControllerIdentity, ControllerIndexes, ControllerMatch,
};
pub use signer::{compute_key_id, RiseTokenSigner};
pub use verify::{verify_external_jwt, JwksKeySource, RiseToken};

/// Check if a JWT issuer is a Rise-issued JWT.
///
/// Rise JWTs have `iss` set to the Rise public URL (e.g.,
/// "https://rise.example.com"). This helper checks for exact match or a
/// port-stripping scheme-prefix match.
pub fn is_rise_issued_jwt(issuer: &str, public_url: &str) -> bool {
    // Exact match
    if issuer == public_url {
        return true;
    }

    // Check if issuer starts with the public_url's base (handles port differences)
    if let Some(public_base) = public_url.strip_suffix(|c: char| c.is_ascii_digit() || c == ':') {
        if issuer.starts_with(public_base) {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_rise_issued_jwt_exact_match() {
        assert!(is_rise_issued_jwt(
            "https://rise.example.com",
            "https://rise.example.com"
        ));
    }

    #[test]
    fn test_is_rise_issued_jwt_port_prefix() {
        // The port-stripping prefix branch: public_url ends in a port digit, and
        // the issuer matches the digit-stripped base. Mirrors the original
        // middleware helper's fuzzy `starts_with` behavior verbatim.
        assert!(is_rise_issued_jwt(
            "https://rise.example.com:844",
            "https://rise.example.com:8443"
        ));
    }

    #[test]
    fn test_is_rise_issued_jwt_no_match() {
        assert!(!is_rise_issued_jwt(
            "https://evil.example.com",
            "https://rise.example.com"
        ));
    }
}
