pub mod handlers;
pub mod models;
pub mod providers;
pub mod routes;

use crate::server::registry::models::{RegistryAuthMethod, RegistryCredentials};
use anyhow::Result;
use async_trait::async_trait;

/// Specifies whether the image tag is for client-facing or internal use
#[derive(Debug, Clone, Copy)]
pub enum ImageTagType {
    /// For CLI clients - uses client_registry_url if configured
    ClientFacing,
    /// For Kubernetes controller - uses internal registry_url only
    Internal,
}

/// Trait for container registry providers
#[async_trait]
pub trait RegistryProvider: Send + Sync {
    /// Get temporary credentials for pushing images (scoped to repository and tag)
    ///
    /// # Arguments
    /// * `repository` - The repository name (e.g., "my-app")
    /// * `tag` - The image tag (deployment ID, UUIDv7).
    ///           Providers that support tag-level scoping use this to mint
    ///           the narrowest possible credentials.
    ///
    /// # Returns
    /// Registry credentials including username, password, and registry URL
    async fn get_credentials(&self, repository: &str, tag: &str) -> Result<RegistryCredentials>;

    /// Get credentials for pulling/reading images (registry-wide)
    ///
    /// Used for resolving image digests. Returns (username, password) tuple.
    /// Returns empty strings if no credentials are available (e.g., anonymous access).
    async fn get_pull_credentials(&self) -> Result<(String, String)>;

    /// Get credentials for a Kubernetes image pull secret.
    ///
    /// `repository` is the project/app name (e.g. `"my-app"`). Providers that issue
    /// repository-scoped tokens (GitLab) use it to restrict the JWT to pull access on
    /// that specific image. Other providers (ECR, OCI client-auth) ignore it.
    ///
    /// Defaults to wrapping `get_pull_credentials()` as `LoginCredentials`.
    #[allow(dead_code)]
    async fn get_k8s_pull_credentials(&self, repository: &str) -> Result<RegistryCredentials> {
        let _ = repository;
        let (username, password) = self.get_pull_credentials().await?;
        Ok(RegistryCredentials {
            registry_url: self.registry_host().to_string(),
            username,
            password,
            expires_in: None,
            auth_method: RegistryAuthMethod::LoginCredentials,
        })
    }

    /// Get the registry host (for credentials map key)
    ///
    /// Returns the registry hostname without protocol or path
    /// (e.g., "459109751375.dkr.ecr.eu-west-1.amazonaws.com")
    fn registry_host(&self) -> &str;

    /// Get the base registry URL
    fn registry_url(&self) -> &str;

    /// Get the full image tag for a deployment
    ///
    /// # Arguments
    /// * `repository` - The repository/project name (e.g., "headscale")
    /// * `tag` - The image tag (deployment ID, UUIDv7)
    /// * `tag_type` - Whether this is for client-facing or internal use
    ///
    /// # Returns
    /// Full image reference for pushing (e.g., `localhost:5000/rise-apps/headscale:<uuidv7>`)
    fn get_image_tag(&self, repository: &str, tag: &str, tag_type: ImageTagType) -> String;

    /// Whether the Kubernetes controller should create and manage image pull secrets.
    ///
    /// Returns `false` when the cluster already has its own image pull mechanism
    /// (e.g., node-level IAM role, pre-configured service account credentials).
    /// Defaults to `true` so existing providers retain their current behaviour.
    fn requires_pull_secret(&self) -> bool {
        true
    }

    /// Hosts of external registries to which the strict cross-project policy
    /// applies. Entries MUST already be normalized via `normalize_host`
    /// (lowercase, no port) — the trait default validator byte-compares
    /// against them without re-normalizing per call. Default empty.
    fn strict_external_hosts(&self) -> &[String] {
        &[]
    }

    /// Validate that an `image_ref` supplied by a CLI caller is allowed to be
    /// deployed under `project_name`. Defends against cross-project image
    /// substitution: an attacker owning project `evil` must not be able to
    /// claim a deployment using project `compass`'s image ref.
    ///
    /// Three branches:
    ///
    /// 1. Image is under this provider's `registry_url()` — the first path
    ///    segment after the prefix must equal `project_name`. Matches the
    ///    historical ECR layout `<host>/<repo_prefix>/<project>:<tag>`.
    /// 2. Image's host (port-stripped, lowercased) is in
    ///    `strict_external_hosts()` — the **last** path segment of the
    ///    repository must equal `project_name`. Closes cross-project
    ///    substitution on a shared external registry (e.g. JFrog) where
    ///    the third-party-allowed default would let project `evil` claim
    ///    project `compass`'s image ref.
    /// 3. Otherwise — allowed unchecked (third-party images like
    ///    `nginx:latest` still work for the pre-built-image workflow).
    fn validate_image_for_project(&self, image_ref: &str, project_name: &str) -> Result<()> {
        // Parse once via the OCI reference grammar so both branches get
        // spec-compliant host/repository extraction. Hand-rolling `strip_prefix`
        // on the raw string opens port/case bypasses (`<host>:443/...`,
        // `<HOST>/...`) — closed here by construction.
        let reference: oci_distribution::Reference = image_ref.parse().map_err(|e| {
            anyhow::anyhow!("Image '{image_ref}' is not a valid OCI reference: {e}")
        })?;
        let image_host = normalize_host(reference.registry());
        let image_repository = reference.repository();

        // Branch 1: image is on this provider's own registry.
        let registry_url = self.registry_url();
        if !registry_url.is_empty() {
            let (primary_host, primary_path) = split_registry_url(registry_url);
            if image_host == normalize_host(primary_host) {
                let path_prefix = primary_path.trim_matches('/');
                let rest = if path_prefix.is_empty() {
                    image_repository
                } else {
                    // Image is on our host but outside our `repo_prefix` — not
                    // a project-managed image; fall through to the external
                    // branches so third-party layouts on the same host still
                    // work (unchanged behavior).
                    image_repository
                        .strip_prefix(path_prefix)
                        .and_then(|r| r.strip_prefix('/'))
                        .unwrap_or_default()
                };
                if !rest.is_empty() {
                    let first_segment = rest.split('/').next().unwrap_or("");
                    if first_segment != project_name {
                        anyhow::bail!(
                            "Image belongs to a different project '{first_segment}' — \
                             deployments can only use images from their own project '{project_name}'"
                        );
                    }
                    return Ok(());
                }
            }
        }

        // Branch 2: image is on a shared external registry the operator
        // opted into strict policy for. Pre-normalized entries — see
        // `EcrProvider::new`.
        let strict_hosts = self.strict_external_hosts();
        if strict_hosts.iter().any(|h| h == &image_host) {
            let last_segment = image_repository.rsplit('/').next().unwrap_or("");
            if last_segment != project_name {
                anyhow::bail!(
                    "Image '{image_ref}' on host '{}' is owned by a different \
                     project (image name '{last_segment}' != project '{project_name}'). \
                     Cross-project image deploys are not allowed for this host.",
                    reference.registry(),
                );
            }
            return Ok(());
        }

        // Branch 3: fully external image (e.g. `nginx:latest`) — allowed
        // unchecked to keep the pre-built-image workflow working.
        Ok(())
    }
}

/// Split a `registry_url()` (which for ECR is `<host>/<repo_prefix>`) into
/// its host and path components.
fn split_registry_url(url: &str) -> (&str, &str) {
    match url.split_once('/') {
        Some((host, path)) => (host, path),
        None => (url, ""),
    }
}

/// Canonicalize a registry host for comparison: lowercase, then strip any
/// `:<port>` suffix. The trust decision is hostname identity (DNS), not
/// endpoint identity — `JFROG.example.com`, `jfrog.example.com`,
/// `jfrog.example.com:443`, and `jfrog.example.com:8443` all denote the
/// same registry for trust purposes. Operators write hostnames, not
/// host:port pairs, in `strict_external_hosts`.
pub(crate) fn normalize_host(host: &str) -> String {
    let lower = host.to_ascii_lowercase();
    match lower.rsplit_once(':') {
        Some((h, _)) => h.to_string(),
        None => lower,
    }
}

#[cfg(test)]
mod validate_image_tests {
    use super::*;
    use async_trait::async_trait;

    /// Tiny mock used to exercise the trait default. Returns the configured
    /// `registry_url` and `strict_external_hosts`; everything else stubs.
    struct MockProvider {
        registry_url: String,
        strict_external_hosts: Vec<String>,
    }

    #[async_trait]
    impl RegistryProvider for MockProvider {
        async fn get_credentials(&self, _: &str, _: &str) -> Result<RegistryCredentials> {
            unimplemented!()
        }
        async fn get_pull_credentials(&self) -> Result<(String, String)> {
            unimplemented!()
        }
        fn registry_host(&self) -> &str {
            ""
        }
        fn registry_url(&self) -> &str {
            &self.registry_url
        }
        fn get_image_tag(&self, _: &str, _: &str, _: ImageTagType) -> String {
            unimplemented!()
        }
        fn strict_external_hosts(&self) -> &[String] {
            &self.strict_external_hosts
        }
    }

    fn provider(registry_url: &str, strict: &[&str]) -> MockProvider {
        MockProvider {
            registry_url: registry_url.to_string(),
            strict_external_hosts: strict.iter().map(|s| normalize_host(s)).collect(),
        }
    }

    const ECR_URL: &str = "123456789012.dkr.ecr.eu-west-1.amazonaws.com/rise";

    #[test]
    fn ecr_prefix_matching_project_allowed() {
        let p = provider(ECR_URL, &[]);
        p.validate_image_for_project(&format!("{ECR_URL}/compass:v1"), "compass")
            .unwrap();
    }

    #[test]
    fn ecr_prefix_mismatched_project_rejected() {
        let p = provider(ECR_URL, &[]);
        let err = p
            .validate_image_for_project(&format!("{ECR_URL}/compass:v1"), "evil")
            .unwrap_err();
        assert!(err.to_string().contains("'compass'"));
    }

    #[test]
    fn primary_host_with_default_port_still_enforces_first_segment() {
        // Previously `strip_prefix` on the raw ref let `<host>:443/rise/compass`
        // slip past the branch-1 check because the prefix wouldn't match.
        // After routing through the OCI parser, the canonicalized host
        // matches and the first-segment rule fires.
        let p = provider(ECR_URL, &[]);
        let err = p
            .validate_image_for_project(
                "123456789012.dkr.ecr.eu-west-1.amazonaws.com:443/rise/compass:v1",
                "evil",
            )
            .unwrap_err();
        assert!(err.to_string().contains("'compass'"));
    }

    #[test]
    fn primary_host_uppercase_still_enforces_first_segment() {
        let p = provider(ECR_URL, &[]);
        let err = p
            .validate_image_for_project(
                "123456789012.DKR.ECR.eu-west-1.amazonaws.com/rise/compass:v1",
                "evil",
            )
            .unwrap_err();
        assert!(err.to_string().contains("'compass'"));
    }

    #[test]
    fn primary_host_outside_repo_prefix_falls_through() {
        // Image on our host but outside our `repo_prefix` isn't Rise-managed —
        // fall through to the external branches (unchecked, matches previous
        // behavior for `<host>/some-other-path/foo:tag`).
        let p = provider(ECR_URL, &[]);
        p.validate_image_for_project(
            "123456789012.dkr.ecr.eu-west-1.amazonaws.com/some-other-team/foo:v1",
            "anything",
        )
        .unwrap();
    }

    #[test]
    fn third_party_image_allowed_without_strict_hosts() {
        let p = provider(ECR_URL, &[]);
        p.validate_image_for_project("nginx:latest", "anything")
            .unwrap();
        p.validate_image_for_project("jfrog.example.com/path/compass:v1", "evil")
            .unwrap();
    }

    #[test]
    fn third_party_host_not_in_strict_list_allowed() {
        let p = provider(ECR_URL, &["jfrog.example.com"]);
        p.validate_image_for_project("ghcr.io/owner/foo:v1", "anything")
            .unwrap();
    }

    #[test]
    fn strict_host_matching_project_allowed() {
        let p = provider(ECR_URL, &["jfrog.example.com"]);
        p.validate_image_for_project("jfrog.example.com/team/playground/compass:v1", "compass")
            .unwrap();
    }

    #[test]
    fn strict_host_mismatched_project_rejected() {
        let p = provider(ECR_URL, &["jfrog.example.com"]);
        let err = p
            .validate_image_for_project("jfrog.example.com/team/playground/compass:v1", "evil")
            .unwrap_err();
        assert!(err.to_string().contains("'compass'"));
    }

    #[test]
    fn strict_host_with_digest_rejected_cross_project() {
        let p = provider(ECR_URL, &["jfrog.example.com"]);
        let digest = "@sha256:0000000000000000000000000000000000000000000000000000000000000000";
        let err = p
            .validate_image_for_project(
                &format!("jfrog.example.com/team/playground/compass{digest}"),
                "evil",
            )
            .unwrap_err();
        assert!(err.to_string().contains("'compass'"));
    }

    #[test]
    fn strict_host_uppercase_rejected_cross_project() {
        let p = provider(ECR_URL, &["jfrog.example.com"]);
        let err = p
            .validate_image_for_project("JFROG.example.com/team/playground/compass:v1", "evil")
            .unwrap_err();
        assert!(err.to_string().contains("'compass'"));
    }

    #[test]
    fn strict_host_with_default_port_rejected_cross_project() {
        let p = provider(ECR_URL, &["jfrog.example.com"]);
        let err = p
            .validate_image_for_project("jfrog.example.com:443/team/playground/compass:v1", "evil")
            .unwrap_err();
        assert!(err.to_string().contains("'compass'"));
    }

    #[test]
    fn strict_host_with_nondefault_port_rejected_cross_project() {
        // The previous narrow normalize_host that only stripped :443/:80
        // missed this case — `:8443` survived in the image ref but not in
        // the trust list, so the strict check silently disabled.
        let p = provider(ECR_URL, &["jfrog.example.com"]);
        let err = p
            .validate_image_for_project("jfrog.example.com:8443/team/playground/compass:v1", "evil")
            .unwrap_err();
        assert!(err.to_string().contains("'compass'"));
    }

    #[test]
    fn strict_host_trust_list_with_port_matches_image_without_port() {
        // Operator writes `host:8443` in the trust list; image has bare
        // hostname. Both canonicalize to the same hostname.
        let p = provider(ECR_URL, &["jfrog.example.com:8443"]);
        let err = p
            .validate_image_for_project("jfrog.example.com/team/playground/compass:v1", "evil")
            .unwrap_err();
        assert!(err.to_string().contains("'compass'"));
    }

    #[test]
    fn invalid_oci_reference_rejected() {
        let p = provider(ECR_URL, &["jfrog.example.com"]);
        // Not a parseable image ref — must error rather than fall through.
        let err = p
            .validate_image_for_project("jfrog.example.com/", "anything")
            .unwrap_err();
        assert!(err.to_string().contains("not a valid OCI reference"));
    }
}
