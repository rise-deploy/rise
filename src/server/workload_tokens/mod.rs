//! Workload identity token exchange.
//!
//! Lets a deployed app exchange its per-deployment bootstrap credential for a
//! short-lived, Rise-signed OIDC JWT describing the Rise identity (project +
//! environment), for federating to external systems (AWS STS, GCP WIF, ...).

pub mod handlers;
pub mod models;
pub mod routes;

// The shared workload-identity minting surface is backend-agnostic and lives in
// the support crates: the credential/subject/signing helpers in
// `rise-backend-auth` (the home for auth-token logic), and the token-refresh TTL
// math in `rise-backend-core`. Re-exported here so the HTTP token-exchange
// endpoint and both deployment controllers keep their existing import paths.
pub use rise_backend_auth::{
    generate_bootstrap_credential, sha256_hex, sign_audience_tokens, workload_subject,
    NO_ENVIRONMENT,
};
pub use rise_backend_core::token_ttl::{refresh_due_after_secs, remint_after_secs};

#[cfg(test)]
mod tests {
    use super::{sha256_hex, workload_subject};

    #[test]
    fn workload_subject_with_environment() {
        assert_eq!(
            workload_subject("myapp", Some("prod")),
            "rise:proj:myapp:env:prod"
        );
    }

    #[test]
    fn workload_subject_without_environment() {
        assert_eq!(
            workload_subject("myapp", None),
            "rise:proj:myapp:env:<null>"
        );
    }

    #[test]
    fn sha256_hex_matches_known_vector() {
        // Empty input → well-known SHA-256 digest.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_hex_is_deterministic_and_distinct() {
        assert_eq!(sha256_hex(b"credential"), sha256_hex(b"credential"));
        assert_ne!(sha256_hex(b"credential-a"), sha256_hex(b"credential-b"));
    }

    #[cfg(feature = "backend")]
    #[test]
    fn bootstrap_credential_is_random_and_url_safe() {
        use super::generate_bootstrap_credential;
        let a = generate_bootstrap_credential();
        let b = generate_bootstrap_credential();
        assert_ne!(a, b, "each credential must be freshly random");
        // 32 bytes base64url-no-pad → 43 chars, alphabet [A-Za-z0-9_-], no '='.
        assert_eq!(a.len(), 43);
        assert!(a
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }
}
