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
    async fn get_credentials(&self, repository: &str, tags: &[&str])
        -> Result<RegistryCredentials>;

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
}

/// Encryption provider trait for encrypting/decrypting secrets
#[async_trait]
pub trait EncryptionProvider: Send + Sync {
    /// Encrypt plaintext and return base64-encoded ciphertext
    async fn encrypt(&self, plaintext: &str) -> Result<String>;

    /// Decrypt base64-encoded ciphertext and return plaintext
    async fn decrypt(&self, ciphertext: &str) -> Result<String>;
}
