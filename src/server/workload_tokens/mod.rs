//! Workload identity token exchange.
//!
//! Lets a deployed app exchange its per-deployment bootstrap credential for a
//! short-lived, Rise-signed OIDC JWT describing the Rise identity (project +
//! environment), for federating to external systems (AWS STS, GCP WIF, ...).

pub mod handlers;
pub mod models;
pub mod routes;

/// Build the subject claim for a workload identity token.
///
/// Fixed and environment-aware: `rise:proj:<project>:env:<environment>`.
/// `_none` is used literally when the deployment has no environment.
pub fn workload_subject(project: &str, environment: Option<&str>) -> String {
    format!(
        "rise:proj:{}:env:{}",
        project,
        environment.unwrap_or("_none")
    )
}

/// SHA-256 hex digest of the given bytes.
///
/// Shared by the controller (hashing the freshly observed bootstrap credential)
/// and the token-exchange endpoint (hashing the presented credential to look up
/// the deployment).
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

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
        assert_eq!(workload_subject("myapp", None), "rise:proj:myapp:env:_none");
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
}
