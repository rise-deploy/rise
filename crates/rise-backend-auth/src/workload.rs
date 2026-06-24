//! Workload-identity token minting and credential helpers.
//!
//! Shared by both deployment backends (the Kubernetes sync webhook and the
//! Docker reconciler) so the bearer credential a workload presents and the
//! claim set of the tokens it receives are structurally identical regardless of
//! backend. Pure-core: no database, no backend feature gate.

use std::collections::BTreeMap;

use crate::claims::WorkloadSubjectInfo;
use crate::error::JwtSignerError;
use crate::signer::RiseTokenSigner;

/// Sentinel used for the environment in workload identity claims when a
/// deployment has no environment. Deliberately contains characters (`<`, `>`)
/// that a real environment name cannot, so it can never collide with one.
pub const NO_ENVIRONMENT: &str = "<null>";

/// Build the subject claim for a workload identity token.
///
/// Fixed and environment-aware: `rise:proj:<project>:env:<environment>`.
/// [`NO_ENVIRONMENT`] is used when the deployment has no environment.
pub fn workload_subject(project: &str, environment: Option<&str>) -> String {
    format!(
        "rise:proj:{}:env:{}",
        project,
        environment.unwrap_or(NO_ENVIRONMENT)
    )
}

/// SHA-256 hex digest of the given bytes.
///
/// Shared by the controllers (hashing the freshly observed bootstrap credential)
/// and the token-exchange endpoint (hashing the presented credential to look up
/// the deployment).
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut acc, b| {
            write!(acc, "{b:02x}").unwrap();
            acc
        })
}

/// Generate a fresh 32-byte bootstrap credential, base64url-encoded (no padding).
///
/// The single source of the credential format, shared by both deployment
/// backends so the bearer secret a workload presents to the token-exchange
/// endpoint is structurally identical regardless of backend.
pub fn generate_bootstrap_credential() -> String {
    use base64::Engine;
    use rand::Rng;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Mint one Rise-signed workload JWT per audience, returning a `filename → JWT`
/// map keyed by the same `[identity].audiences` keys.
///
/// The single home for the per-audience minting loop, shared by both deployment
/// backends so the claim set (subject, project, environment, group, id, audience)
/// can never drift between them.
pub fn sign_audience_tokens(
    signer: &RiseTokenSigner,
    info: &WorkloadSubjectInfo<'_>,
    audiences: &BTreeMap<String, String>,
    ttl_secs: u64,
) -> Result<BTreeMap<String, String>, JwtSignerError> {
    let mut out = BTreeMap::new();
    for (filename, audience) in audiences {
        let jwt = signer
            .sign_workload_jwt(info, audience, ttl_secs)
            .map_err(|e| JwtSignerError::WorkloadAudience {
                audience: audience.clone(),
                source: Box::new(e),
            })?;
        out.insert(filename.clone(), jwt);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn bootstrap_credential_is_random_and_url_safe() {
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
