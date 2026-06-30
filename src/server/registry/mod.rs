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
    /// * `tag` - The image tag (e.g., deployment ID like "20251215-204525").
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
    /// * `tag` - The image tag (e.g., deployment ID like "20251215-204525")
    /// * `tag_type` - Whether this is for client-facing or internal use
    ///
    /// # Returns
    /// Full image reference for pushing (e.g., "localhost:5000/rise-apps/headscale:20251215-204525")
    fn get_image_tag(&self, repository: &str, tag: &str, tag_type: ImageTagType) -> String;

    /// Whether the Kubernetes controller should create and manage image pull secrets.
    ///
    /// Returns `false` when the cluster already has its own image pull mechanism
    /// (e.g., node-level IAM role, pre-configured service account credentials).
    /// Defaults to `true` so existing providers retain their current behaviour.
    fn requires_pull_secret(&self) -> bool {
        true
    }

    /// Validate that an `image_ref` supplied by a CLI caller is allowed to be
    /// deployed under `project_name`. Defends against cross-project image
    /// substitution: an attacker owning project `evil` must not be able to
    /// claim a deployment using project `compass`'s image ref.
    ///
    /// Default implementation: when the image starts with this provider's
    /// `registry_url()/`, the first path segment after the prefix must equal
    /// `project_name`. External images (not under that prefix) are allowed.
    /// Matches the historical ECR layout `<host>/<repo_prefix>/<project>:<tag>`.
    ///
    /// Stricter providers override this to enforce the invariant universally
    /// (regardless of repo prefix layout) and to reject external images.
    fn validate_image_for_project(&self, image_ref: &str, project_name: &str) -> Result<()> {
        let registry_url = self.registry_url();
        if registry_url.is_empty() {
            return Ok(());
        }
        let prefix = format!("{}/", registry_url.trim_end_matches('/'));
        let Some(image_path) = image_ref.strip_prefix(&prefix) else {
            return Ok(());
        };
        let image_project = image_path.split([':', '/', '@']).next().unwrap_or("");
        if image_project != project_name {
            anyhow::bail!(
                "Image belongs to a different project '{}' — \
                 deployments can only use images from their own project '{}'",
                image_project,
                project_name
            );
        }
        Ok(())
    }
}
