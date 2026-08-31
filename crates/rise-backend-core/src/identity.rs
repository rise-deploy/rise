//! Workload-identity in-container contract shared by every deployment backend.
//!
//! Rise delivers a per-deployment bootstrap credential and any auto-minted
//! workload tokens as files inside the app's container. The mount path and key
//! names are part of the backend-agnostic contract a workload reads, so every
//! backend lands the files at the same place: Kubernetes via a projected Secret
//! volume, Docker via the archive API.

/// In-container mount path for the workload-identity material.
pub const IDENTITY_MOUNT_PATH: &str = "/var/run/secrets/rise/identity";
/// Subdirectory under [`IDENTITY_MOUNT_PATH`] holding the auto-minted
/// per-audience token files.
pub const IDENTITY_TOKENS_SUBDIR: &str = "tokens";
/// Secret data key for the bootstrap credential.
pub const IDENTITY_CREDENTIAL_KEY: &str = "credential";
