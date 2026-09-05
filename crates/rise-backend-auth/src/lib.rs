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
mod workload;

pub use claims::{
    AccessClaims, ExternalClaims, PrincipalClaims, RiseClaims, Scope, WorkloadClaims,
    WorkloadSubjectInfo,
};
pub use error::{AuthError, JwtSignerError};
pub use matchers::{
    audience_matches, match_trust_candidates, matches_wildcard_pattern, validate_custom_claims,
    validate_oidc_issuer, TrustCandidate, TrustMatch,
};
pub use signer::{compute_key_id, RiseTokenSigner, RISE_ACCESS_TYP};
pub use verify::{verify_external_jwt, JwksKeySource, RiseToken};
pub use workload::{
    generate_bootstrap_credential, sha256_hex, sign_audience_tokens, workload_subject,
    NO_ENVIRONMENT,
};

/// Check whether a JWT issuer is Rise-issued.
///
/// Rise JWTs set `iss` to the Rise public URL (e.g. "https://rise.example.com"),
/// minted from the signer's configured issuer. The match is **exact**, kept that
/// way deliberately so this predicate stays consistent with the `aud` checks in
/// the auth middleware, which compare `claims.aud` to `public_url` exactly. (Both
/// `iss` and `aud` are minted from the same `public_url`, so they share its form;
/// matching `iss` loosely here while `aud` is matched strictly would route a
/// slash-variant token to the Rise path only to reject it at the `aud` check.)
/// The exchange endpoint also relies on this to reject Rise-issued source tokens,
/// so a fuzzy prefix/port match could let a sibling-port issuer be treated as
/// Rise-issued.
///
/// The CLI does not compare issuers at all: it decides whether to pre-exchange a
/// token from the input *channel* (`RISE_TOKEN` / stored login are ready Rise
/// bearers; `RISE_TOKEN_COMMAND` / GitHub Actions OIDC are external tokens to
/// exchange), so it never depends on the client's backend URL matching
/// `public_url`. This predicate is purely server-side and can stay exact.
pub fn is_rise_issued_jwt(issuer: &str, public_url: &str) -> bool {
    issuer == public_url
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
    fn test_is_rise_issued_jwt_exact_match_with_port() {
        assert!(is_rise_issued_jwt(
            "https://rise.example.com:8443",
            "https://rise.example.com:8443"
        ));
    }

    #[test]
    fn test_is_rise_issued_jwt_rejects_trailing_slash_mismatch() {
        // Exact match, intentionally: the middleware's `aud` checks compare
        // exactly too, so loosening only `iss` here would desync routing from
        // audience validation.
        assert!(!is_rise_issued_jwt(
            "https://rise.example.com/",
            "https://rise.example.com"
        ));
    }

    #[test]
    fn test_is_rise_issued_jwt_rejects_sibling_port() {
        // A sibling port that the old fuzzy prefix match would have accepted must
        // NOT be treated as Rise-issued. The exchange relies on this exactness to
        // reject Rise-issued source tokens.
        assert!(!is_rise_issued_jwt(
            "https://rise.example.com:8440",
            "https://rise.example.com:8443"
        ));
        // The digit-stripped prefix superset is likewise rejected.
        assert!(!is_rise_issued_jwt(
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
