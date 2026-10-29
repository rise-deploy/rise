//! Provider contracts the deployment backends depend on: container registry
//! credential minting and secret encryption. Concrete implementations (ECR,
//! GitLab, JFrog, OCI client-auth, local AES-GCM, AWS KMS) live in
//! `rise-deploy`.

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// How the CLI should apply registry credentials before pushing
#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RegistryAuthMethod {
    /// Use `docker/podman login` (default; works for ECR, OCI registries)
    #[default]
    LoginCredentials,
    /// Write a `registrytoken` entry directly into the container CLI's auth config file.
    /// Used when a bearer JWT must be injected without going through the login handshake
    /// (e.g., GitLab scoped JWTs).
    RegistryToken,
}

/// Registry credentials response
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RegistryCredentials {
    /// Registry path for docker login (e.g., "123456789.dkr.ecr.us-east-1.amazonaws.com/rise/myapp")
    /// This should be the full repository path that the credentials are scoped to
    pub registry_url: String,
    /// Username for authentication
    pub username: String,
    /// Password or token for authentication
    pub password: String,
    /// How long the credentials are valid (in seconds)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<u64>,
    /// How the CLI should apply these credentials
    #[serde(default)]
    pub auth_method: RegistryAuthMethod,
}

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
    /// Mint temporary credentials for pushing one or more images, scoped to
    /// the smallest set of artifacts the provider can express.
    ///
    /// # Arguments
    /// * `repository` - The repository name (e.g., `"my-app"`). Every tag in
    ///   `tags` is written to this repository — multi-container deployments
    ///   share one repository and only differ in their tag suffix.
    /// * `project_id` - Immutable identity from the trusted project row.
    /// * `tags` - All image tags the returned credentials must be able to
    ///   write to. For single-container this is a one-element slice (the
    ///   deployment ID); for multi-container it has one entry per container
    ///   (e.g. `["<deployment_id>-frontend", "<deployment_id>-backend"]`).
    ///   Providers that scope by tag (JFrog) must include every entry in the
    ///   minted token's scope. Providers that scope by repository (ECR,
    ///   GitLab) may ignore the slice but MUST still accept it.
    ///
    /// Implementations may assume `tags` is non-empty; the caller guarantees
    /// at least one tag.
    async fn get_credentials(
        &self,
        repository: &str,
        project_id: uuid::Uuid,
        tags: &[&str],
    ) -> Result<RegistryCredentials>;

    /// Get credentials for pulling/reading images (registry-wide)
    ///
    /// Used for resolving image digests. Returns (username, password) tuple.
    /// Returns empty strings if no credentials are available (e.g., anonymous access).
    /// Providers requiring project scope refuse this operation.
    async fn get_pull_credentials(&self) -> Result<(String, String)>;

    /// Get pull credentials for a project's repository, including provider
    /// eligibility checks. Used by image admission and deployment runtimes.
    async fn get_project_pull_credentials(
        &self,
        repository: &str,
        project_id: uuid::Uuid,
    ) -> Result<RegistryCredentials> {
        self.get_k8s_pull_credentials(repository, project_id).await
    }

    /// Opaque identity of the current project pull-credential scope. Runtimes
    /// compare it with cached credentials to detect provider-side rotations.
    /// Providers without rotating scopes return `None`.
    async fn project_pull_credential_scope(
        &self,
        _repository: &str,
        _project_id: uuid::Uuid,
    ) -> Result<Option<String>> {
        Ok(None)
    }

    /// Return credentials together with the scope they were minted for.
    /// Providers with mutable scopes must override this method to return the
    /// exact scope snapshot used to mint or retrieve the credentials atomically.
    async fn get_project_pull_credentials_with_scope(
        &self,
        repository: &str,
        project_id: uuid::Uuid,
    ) -> Result<(RegistryCredentials, Option<String>)> {
        let credentials = self
            .get_project_pull_credentials(repository, project_id)
            .await?;
        let scope = self
            .project_pull_credential_scope(repository, project_id)
            .await?;
        Ok((credentials, scope))
    }

    /// The project named by a Rise-managed image, or `None` for an external
    /// image. Registry namespaces are matched on a path-segment boundary.
    fn image_project<'a>(&self, image: &'a str) -> Option<&'a str> {
        let path = image
            .strip_prefix(self.registry_url().trim_end_matches('/'))?
            .strip_prefix('/')?;
        path.split([':', '/', '@'])
            .next()
            .filter(|name| !name.is_empty())
    }

    /// Get credentials for a Kubernetes image pull secret.
    ///
    /// `repository` is the project/app name (e.g. `"my-app"`). Providers that issue
    /// repository-scoped credentials use it to restrict access to that image.
    ///
    /// Defaults to wrapping `get_pull_credentials()` as `LoginCredentials`.
    async fn get_k8s_pull_credentials(
        &self,
        _repository: &str,
        _project_id: uuid::Uuid,
    ) -> Result<RegistryCredentials> {
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
}

/// Encryption authenticates the consumer's independently derived resource and slot.
#[async_trait]
pub trait EncryptionProvider: Send + Sync {
    async fn encrypt(&self, plaintext: &str, context: &crate::EncryptionContext) -> Result<String>;
    async fn decrypt(
        &self,
        envelope: &str,
        expected_context: &crate::EncryptionContext,
    ) -> Result<String>;

    /// Only the startup migration may accept an unbound baseline value.
    async fn decrypt_legacy_for_migration(&self, _ciphertext: &str) -> Result<String> {
        anyhow::bail!("This provider does not support legacy baseline migration")
    }
}
