//! Workload-identity in-container contract shared by every deployment backend.
//!
//! Rise delivers a per-deployment bootstrap credential and any auto-minted
//! workload tokens as files inside the app's container. The mount path and key
//! names are part of the backend-agnostic contract a workload reads, so every
//! backend lands the files at the same place: Kubernetes via a projected Secret
//! volume, Docker via the archive API, ECS via an identity sidecar writing to a
//! task-scoped volume.

/// In-container mount path for the workload-identity material.
pub const IDENTITY_MOUNT_PATH: &str = "/var/run/secrets/rise/identity";
/// Subdirectory under [`IDENTITY_MOUNT_PATH`] holding the auto-minted
/// per-audience token files.
pub const IDENTITY_TOKENS_SUBDIR: &str = "tokens";
/// Secret data key for the bootstrap credential.
pub const IDENTITY_CREDENTIAL_KEY: &str = "credential";

/// Whether a `[identity].audiences` map key is safe to use as a single in-pod
/// token filename. Rejects empty, `.`/`..`, and any name containing a path
/// separator so a crafted filename can never escape the tokens directory,
/// whichever backend writes it (path-traversal defense).
pub fn is_safe_token_filename(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_filenames_cannot_escape_the_tokens_directory() {
        for ok in ["e2e", "aws.jwt", "..hidden", "a-b_c"] {
            assert!(is_safe_token_filename(ok), "{ok:?} should be accepted");
        }
        for bad in ["", ".", "..", "a/b", "../x", "a\\b", "a\0b"] {
            assert!(!is_safe_token_filename(bad), "{bad:?} should be rejected");
        }
    }
}
