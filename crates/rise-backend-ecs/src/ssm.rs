//! Secret environment variables as SSM Parameter Store `SecureString`s.
//!
//! This is the one place the ECS backend improves on Docker rather than matching
//! it. The Docker backend flattens decrypted secrets into plain container
//! environment — visible to anyone who can run `docker inspect` — and documents
//! that as a known gap in the feature matrix. On ECS the value never enters the
//! task definition at all: it is written to SSM under a per-deployment path, and
//! the container definition carries only the parameter's name, which ECS resolves
//! at task start using the execution role.
//!
//! Two consequences worth stating, because both are easy to get wrong:
//!
//! - **The env hash still covers secret values, via a fingerprint.** Drift
//!   detection hashes the full merged environment; if it hashed only what reaches
//!   the task definition, editing a secret would leave the hash unchanged and the
//!   deployment would never roll. Secret values enter the hash as fingerprints of
//!   their stored form, never plaintext (see `redact_secrets_for_hash`).
//! - **Parameters are per deployment, not per project.** A rollback re-creates
//!   the prior deployment's services, which must resolve the values that
//!   deployment shipped with. Sharing one path across deployments would make a
//!   rollback silently pick up the newer secret.

use anyhow::{bail, Result};
use rise_backend_core::normalize_deployment_group;

/// SSM Standard-tier parameters cap the value at 4 KB. Advanced tier raises it to
/// 8 KB at a per-parameter cost; Rise stays on Standard and rejects oversized
/// values with an actionable message instead of silently upgrading the tier and
/// the operator's bill.
pub const MAX_STANDARD_VALUE_BYTES: usize = 4096;

/// Upper bound on a secret's env-var name as an SSM path segment, chosen so the
/// full parameter name cannot exceed SSM's 2048-character limit.
pub const MAX_KEY_SEGMENT_CHARS: usize = 512;

/// The parameter path for one deployment's secret.
///
/// `/{prefix}/{project}/{group}/{deployment_id}/{KEY}` — hierarchical so an
/// operator can grant `ssm:GetParameters` on `/{prefix}/{project}/*` and no more,
/// and so the reconciler can delete a whole deployment's secrets by path prefix
/// when the deployment is retired.
pub fn parameter_name(
    prefix: &str,
    project: &str,
    deployment_group: &str,
    deployment_id: &str,
    key: &str,
) -> String {
    let prefix = prefix.trim_matches('/');
    // The group is escaped to a single path segment: a raw group legally
    // contains `/` (branch/MR conventions like `mr/123`), and interpolating it
    // straight would deepen the hierarchy past SSM's 15-level cap and make
    // `PutParameter` fail for every secret. Same escaping the K8s and Docker
    // backends apply to the group, so a deployment resolves to one node here too.
    let group = normalize_deployment_group(deployment_group);
    format!("/{prefix}/{project}/{group}/{deployment_id}/{key}")
}

/// The path prefix covering every secret of one deployment. Used to enumerate
/// and delete them when the deployment is GC'd.
pub fn deployment_path_prefix(
    prefix: &str,
    project: &str,
    deployment_group: &str,
    deployment_id: &str,
) -> String {
    let prefix = prefix.trim_matches('/');
    // Must escape the group identically to `parameter_name`, or the delete
    // prefix would not match the names that were written.
    let group = normalize_deployment_group(deployment_group);
    format!("/{prefix}/{project}/{group}/{deployment_id}")
}

/// Whether an environment variable name is usable as an SSM path segment.
///
/// SSM parameter names allow `a-zA-Z0-9_.-` per segment; a `/` would silently
/// create a deeper path, which both breaks the delete-by-prefix contract and
/// lets a crafted variable name write outside its deployment's subtree.
pub fn is_safe_parameter_segment(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
}

/// Validate one secret before it is written.
pub fn validate(key: &str, value: &[u8]) -> Result<()> {
    if !is_safe_parameter_segment(key) {
        bail!(
            "environment variable name {key:?} cannot be stored as an SSM parameter: \
             names may contain only letters, digits, '_', '.' and '-'"
        );
    }
    // Bound the key so the composed `/{prefix}/{project}/{group}/{id}/{key}` name
    // stays under SSM's 2048-character limit. The other segments are each already
    // bounded (project and normalized group <= 63, the id a timestamp), so a
    // generous cap here is enough, and far above any real environment variable.
    if key.len() > MAX_KEY_SEGMENT_CHARS {
        bail!(
            "environment variable name {key:?} is {} characters, over the \
             {MAX_KEY_SEGMENT_CHARS}-character limit for an SSM parameter segment",
            key.len()
        );
    }
    if value.len() > MAX_STANDARD_VALUE_BYTES {
        bail!(
            "secret {key:?} is {} bytes, over the {MAX_STANDARD_VALUE_BYTES}-byte SSM \
             Standard-tier limit. Store large values in S3 and pass a reference instead.",
            value.len()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameter_path_is_scoped_per_deployment() {
        // A rollback re-creates the prior deployment's services; they must
        // resolve the values that deployment shipped with, so the path has to
        // include the deployment id rather than being shared per project.
        let a = parameter_name("rise", "myapp", "default", "20260101-120000", "API_KEY");
        let b = parameter_name("rise", "myapp", "default", "20260101-130000", "API_KEY");
        assert_ne!(a, b);
        assert_eq!(a, "/rise/myapp/default/20260101-120000/API_KEY");
    }

    #[test]
    fn the_delete_prefix_covers_exactly_one_deployment() {
        // GC deletes by prefix. If the prefix were one level shallower it would
        // delete the *other* live deployment's secrets mid-cutover.
        let prefix = deployment_path_prefix("rise", "myapp", "default", "20260101-120000");
        let mine = parameter_name("rise", "myapp", "default", "20260101-120000", "API_KEY");
        let other = parameter_name("rise", "myapp", "default", "20260101-130000", "API_KEY");
        assert!(mine.starts_with(&prefix));
        assert!(
            !other.starts_with(&prefix),
            "the delete prefix must not reach a sibling deployment"
        );
    }

    #[test]
    fn a_slash_in_a_variable_name_is_rejected() {
        // Path traversal: `A/../../B` would write outside the deployment subtree
        // and escape the delete-by-prefix contract.
        assert!(!is_safe_parameter_segment("API/KEY"));
        assert!(!is_safe_parameter_segment(".."));
        assert!(!is_safe_parameter_segment(""));
        assert!(is_safe_parameter_segment("API_KEY"));
        assert!(is_safe_parameter_segment("app.secret-1"));

        let err = validate("API/KEY", b"v").expect_err("must reject");
        assert!(err.to_string().contains("cannot be stored"));
    }

    #[test]
    fn a_slashed_group_is_flattened_to_one_segment() {
        // A raw group is allowed to contain `/`; left unescaped it would deepen
        // the path past SSM's 15-level cap. It must resolve to a single segment,
        // and the delete prefix must still match what was written.
        let name = parameter_name(
            "rise",
            "myapp",
            "team/app/feature",
            "20260101-120000",
            "API_KEY",
        );
        assert_eq!(
            name,
            "/rise/myapp/team--app--feature/20260101-120000/API_KEY"
        );
        let prefix = deployment_path_prefix("rise", "myapp", "team/app/feature", "20260101-120000");
        assert!(name.starts_with(&prefix));
    }

    #[test]
    fn an_overlong_key_is_rejected() {
        let long = "X".repeat(MAX_KEY_SEGMENT_CHARS + 1);
        let err = validate(&long, b"v").expect_err("must reject");
        assert!(err.to_string().contains("over the"), "{err}");
        // At the bound it is still accepted.
        assert!(validate(&"X".repeat(MAX_KEY_SEGMENT_CHARS), b"v").is_ok());
    }

    #[test]
    fn an_oversized_secret_is_rejected_with_an_actionable_message() {
        // Otherwise PutParameter fails at reconcile time, after the deploy has
        // already reported success to the user.
        let err =
            validate("BIG", &vec![b'x'; MAX_STANDARD_VALUE_BYTES + 1]).expect_err("must reject");
        let msg = err.to_string();
        assert!(msg.contains("over the"), "unhelpful: {msg}");
        assert!(msg.contains("S3"), "should suggest a way forward: {msg}");
    }
}
