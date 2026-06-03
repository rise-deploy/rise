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

pub use claims::{
    AccessClaims, ExternalClaims, PrincipalClaims, RiseClaims, Scope, WorkloadClaims,
    WorkloadSubjectInfo,
};
pub use error::{AuthError, JwtSignerError};
pub use matchers::{
    audience_matches, build_controller_indexes, match_controller_identity,
    matches_wildcard_pattern, validate_controller_id, validate_custom_claims, validate_oidc_issuer,
    ControllerIdentity, ControllerIndexes, ControllerMatch,
};
pub use signer::{compute_key_id, RiseTokenSigner, RISE_ACCESS_TYP};
pub use verify::{verify_external_jwt, JwksKeySource, RiseToken};

/// Check whether a JWT issuer is Rise-issued.
///
/// Rise JWTs set `iss` to the Rise public URL (e.g. "https://rise.example.com"),
/// minted from the signer's configured issuer. The match is **exact**: the
/// exchange endpoint relies on this predicate to reject Rise-issued tokens, so a
/// fuzzy prefix/port match could let a sibling-port issuer be treated as
/// Rise-issued. A non-Rise issuer can never forge a Rise *signature*, but exact
/// matching keeps the routing decision unambiguous at both call sites
/// (middleware + exchange).
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
