//! Workload-identity material delivery for the Docker backend.
//!
//! On Kubernetes the controller mints per-audience JWTs and mounts a Secret with
//! the bootstrap credential + token files into the pod (see
//! `resource_builder::create_identity_secret`). Docker has no Secret/volume
//! equivalent, so this module delivers the SAME files to the SAME in-container
//! paths via the Docker archive API (`PUT /containers/{id}/archive`):
//!
//! ```text
//! /var/run/secrets/rise/identity/credential        # bootstrap credential
//! /var/run/secrets/rise/identity/tokens/<filename>  # one per [identity] audience
//! ```
//!
//! The in-container paths are shared with the K8s backend via the
//! [`IDENTITY_MOUNT_PATH`] / [`IDENTITY_TOKENS_SUBDIR`] / [`IDENTITY_CREDENTIAL_KEY`]
//! constants so the contract a workload reads is identical on both backends.
//!
//! Delivery (`upload_*`) and recovery (`read_back_credential`) touch the daemon
//! and are exercised end-to-end; the tar (de)serialization is pure and unit-tested.

use std::collections::BTreeMap;
use std::io::Read;

use anyhow::{Context, Result};
use bollard::container::{DownloadFromContainerOptions, UploadToContainerOptions};
use bollard::Docker;
use futures::StreamExt;

use crate::server::deployment::resource_builder::{
    IDENTITY_CREDENTIAL_KEY, IDENTITY_MOUNT_PATH, IDENTITY_TOKENS_SUBDIR,
};

/// In-tar (relative) form of [`IDENTITY_MOUNT_PATH`] — the archive PUT extracts
/// into `/`, so entries carry the full path minus the leading slash. The Docker
/// daemon creates the intermediate directories (and follows the conventional
/// `/var/run` → `/run` symlink) when extracting, so we emit file entries only.
fn mount_base_relative() -> &'static str {
    IDENTITY_MOUNT_PATH.trim_start_matches('/')
}

/// Whether a `[identity].audiences` map key is safe to use as a single in-pod
/// token filename. Rejects empty, `.`/`..`, and any name containing a path
/// separator so a crafted filename can never escape the tokens directory when we
/// build the tar (path-traversal defense).
pub fn is_safe_token_filename(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}

/// Build a tar archive (for the archive PUT) carrying the identity files.
///
/// `credential` is included as `<mount>/credential` when `Some` (omit it for a
/// token-only refresh of a container that already has the credential). Each
/// entry in `tokens` (filename → JWT) becomes `<mount>/tokens/<filename>`.
/// Unsafe filenames are skipped (they are also filtered upstream when minting).
/// Files are mode `0o444` so the workload reads them regardless of its UID.
pub fn build_identity_tar(
    credential: Option<&str>,
    tokens: &BTreeMap<String, String>,
) -> Result<Vec<u8>> {
    let base = mount_base_relative();
    let mut builder = tar::Builder::new(Vec::new());

    if let Some(cred) = credential {
        append_file(
            &mut builder,
            &format!("{base}/{IDENTITY_CREDENTIAL_KEY}"),
            cred.as_bytes(),
        )?;
    }
    for (filename, jwt) in tokens {
        if !is_safe_token_filename(filename) {
            continue;
        }
        append_file(
            &mut builder,
            &format!("{base}/{IDENTITY_TOKENS_SUBDIR}/{filename}"),
            jwt.as_bytes(),
        )?;
    }

    builder
        .into_inner()
        .context("Failed to finalize identity tar archive")
}

fn append_file(builder: &mut tar::Builder<Vec<u8>>, path: &str, contents: &[u8]) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header
        .set_path(path)
        .with_context(|| format!("Invalid identity tar entry path: {path}"))?;
    header.set_size(contents.len() as u64);
    header.set_mode(0o444);
    header.set_mtime(0);
    header.set_cksum();
    builder
        .append(&header, contents)
        .with_context(|| format!("Failed to append identity tar entry: {path}"))?;
    Ok(())
}

/// Extract the bootstrap credential from a `download_from_container` tar (the
/// archive GET of the credential file path). Returns the trimmed credential, or
/// `None` when the archive has no readable, non-empty `credential` entry.
pub fn parse_credential_from_tar(tar_bytes: &[u8]) -> Option<String> {
    let mut archive = tar::Archive::new(tar_bytes);
    let entries = archive.entries().ok()?;
    for entry in entries {
        let mut entry = entry.ok()?;
        let is_credential = entry
            .path()
            .ok()
            .and_then(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n == IDENTITY_CREDENTIAL_KEY)
            })
            .unwrap_or(false);
        if !is_credential {
            continue;
        }
        let mut buf = String::new();
        entry.read_to_string(&mut buf).ok()?;
        let trimmed = buf.trim();
        if trimmed.is_empty() {
            return None;
        }
        return Some(trimmed.to_string());
    }
    None
}

/// Upload identity files into a (created or running) container, extracting the
/// tar at `/`. Used both right after create (credential + tokens) and on the
/// periodic token refresh (tokens only).
pub async fn upload_identity(docker: &Docker, container: &str, tar: Vec<u8>) -> Result<()> {
    docker
        .upload_to_container(
            container,
            Some(UploadToContainerOptions {
                path: "/",
                ..Default::default()
            }),
            tar.into(),
        )
        .await
        .with_context(|| format!("Failed to upload identity files to container {container}"))
}

/// Recover a deployment's bootstrap credential from an existing container by
/// reading back its credential file (the Docker analogue of K8s reading the
/// credential from the observed Secret). Returns `None` when the file is absent
/// or unreadable (e.g. the container predates this feature) — the caller then
/// generates a fresh credential.
pub async fn read_back_credential(docker: &Docker, container: &str) -> Option<String> {
    let path = format!("{IDENTITY_MOUNT_PATH}/{IDENTITY_CREDENTIAL_KEY}");
    let mut stream =
        docker.download_from_container(container, Some(DownloadFromContainerOptions { path }));
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => buf.extend_from_slice(&bytes),
            // A missing file yields a 404 error here; treat any read error as
            // "no recoverable credential" so the caller generates a fresh one.
            Err(_) => return None,
        }
    }
    parse_credential_from_tar(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(tar_bytes: &[u8]) -> BTreeMap<String, String> {
        let mut archive = tar::Archive::new(tar_bytes);
        let mut out = BTreeMap::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().into_owned();
            let mut content = String::new();
            entry.read_to_string(&mut content).unwrap();
            out.insert(path, content);
        }
        out
    }

    #[test]
    fn tar_lands_files_at_the_shared_mount_paths() {
        let mut tokens = BTreeMap::new();
        tokens.insert("aws".to_string(), "jwt-aws".to_string());
        tokens.insert("gcp".to_string(), "jwt-gcp".to_string());

        let tar = build_identity_tar(Some("the-credential"), &tokens).unwrap();
        let map = entries(&tar);

        assert_eq!(
            map.get("var/run/secrets/rise/identity/credential")
                .map(String::as_str),
            Some("the-credential")
        );
        assert_eq!(
            map.get("var/run/secrets/rise/identity/tokens/aws")
                .map(String::as_str),
            Some("jwt-aws")
        );
        assert_eq!(
            map.get("var/run/secrets/rise/identity/tokens/gcp")
                .map(String::as_str),
            Some("jwt-gcp")
        );
        // Entries are relative (extracted at `/`), never absolute.
        assert!(map.keys().all(|k| !k.starts_with('/')));
    }

    #[test]
    fn tar_without_credential_carries_tokens_only() {
        let mut tokens = BTreeMap::new();
        tokens.insert("aws".to_string(), "jwt-aws".to_string());
        let tar = build_identity_tar(None, &tokens).unwrap();
        let map = entries(&tar);
        assert!(!map.contains_key("var/run/secrets/rise/identity/credential"));
        assert!(map.contains_key("var/run/secrets/rise/identity/tokens/aws"));
    }

    #[test]
    fn credential_round_trips_through_download_tar_shape() {
        // The archive GET of the credential file returns a tar whose single entry
        // is the basename `credential`; `parse_credential_from_tar` must recover it.
        let mut builder = tar::Builder::new(Vec::new());
        append_file(&mut builder, "credential", b"recovered-cred\n").unwrap();
        let tar = builder.into_inner().unwrap();
        assert_eq!(
            parse_credential_from_tar(&tar).as_deref(),
            Some("recovered-cred")
        );
    }

    #[test]
    fn parse_credential_handles_empty_and_missing() {
        // Empty file → None.
        let mut builder = tar::Builder::new(Vec::new());
        append_file(&mut builder, "credential", b"").unwrap();
        let tar = builder.into_inner().unwrap();
        assert_eq!(parse_credential_from_tar(&tar), None);

        // A tar without a `credential` entry → None.
        let mut builder = tar::Builder::new(Vec::new());
        append_file(&mut builder, "tokens/aws", b"jwt").unwrap();
        let tar = builder.into_inner().unwrap();
        assert_eq!(parse_credential_from_tar(&tar), None);

        // Garbage bytes → None (no panic).
        assert_eq!(parse_credential_from_tar(b"not a tar"), None);
    }

    #[test]
    fn unsafe_token_filenames_are_skipped() {
        assert!(!is_safe_token_filename(""));
        assert!(!is_safe_token_filename("."));
        assert!(!is_safe_token_filename(".."));
        assert!(!is_safe_token_filename("../escape"));
        assert!(!is_safe_token_filename("a/b"));
        assert!(is_safe_token_filename("aws"));
        assert!(is_safe_token_filename("aws.sts"));
        assert!(is_safe_token_filename("aws-prod_1"));

        let mut tokens = BTreeMap::new();
        tokens.insert("../escape".to_string(), "evil".to_string());
        tokens.insert("ok".to_string(), "good".to_string());
        let tar = build_identity_tar(None, &tokens).unwrap();
        let map = entries(&tar);
        assert!(map.contains_key("var/run/secrets/rise/identity/tokens/ok"));
        assert!(map
            .keys()
            .all(|k| !k.contains("escape") && !k.contains("..")));
    }
}
