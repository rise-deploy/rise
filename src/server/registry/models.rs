use serde::{Deserialize, Serialize};

/// The credential types live in `rise-backend-core`; re-exported here so existing
/// `crate::server::registry::models::{RegistryCredentials, RegistryAuthMethod}`
/// references keep working.
pub use rise_backend_core::{RegistryAuthMethod, RegistryCredentials};

/// Registry credentials response wrapper
#[derive(Debug, Serialize)]
pub struct GetRegistryCredsResponse {
    pub credentials: RegistryCredentials,
    pub repository: String,
}

/// Configuration for AWS ECR registry
#[cfg(feature = "backend")]
#[derive(Debug, Clone, Deserialize)]
pub struct EcrConfig {
    /// AWS region (e.g., "us-east-1")
    pub region: String,
    /// AWS account ID (e.g., "123456789012")
    pub account_id: String,
    /// Optional: AWS access key ID (if not using IAM role)
    pub access_key_id: Option<String>,
    /// Optional: AWS secret access key (if not using IAM role)
    pub secret_access_key: Option<String>,
    /// Literal prefix for ECR repository names (e.g., "rise/" → repos named "rise/{project}")
    #[serde(default = "default_repo_prefix")]
    pub repo_prefix: String,
    /// IAM role ARN for push operations (assumed to generate scoped credentials)
    pub push_role_arn: String,
    /// Whether to automatically delete ECR repos when projects are deleted
    /// If false, repos are tagged as orphaned instead
    #[serde(default)]
    pub auto_remove: bool,
}

#[cfg(feature = "backend")]
fn default_repo_prefix() -> String {
    "rise/".to_string()
}

/// Configuration for an OCI registry with optional static authentication
///
/// When `username` and `password` are empty, the client must already be
/// authenticated (e.g., via `docker login`). When configured, the backend
/// returns the static credentials to authorized clients and uses them for pulls.
#[derive(Debug, Clone, Deserialize)]
pub struct OciClientAuthConfig {
    /// Registry URL (e.g., "localhost:5000", "registry.example.com")
    pub registry_url: String,
    /// Namespace/path within registry (e.g., "rise-apps", "myorg")
    #[serde(default = "default_namespace")]
    pub namespace: String,
    /// Optional client-facing registry URL for CLI push operations
    /// If not specified, defaults to registry_url
    #[serde(default)]
    pub client_registry_url: Option<String>,
    /// Optional static username returned to authorized CLI clients and used for pulls
    #[serde(default)]
    pub username: String,
    /// Optional static password returned to authorized CLI clients and used for pulls
    #[serde(default)]
    pub password: String,
}

fn default_namespace() -> String {
    String::new()
}

/// Configuration for JFrog Artifactory container registry
///
/// Supports two token-issuing backends:
/// - Vault: Uses HashiCorp Vault with vault-plugin-secrets-artifactory
/// - Direct: Uses JFrog's own access token API
#[cfg(feature = "backend")]
#[derive(Debug, Clone)]
pub struct JfrogConfig {
    pub token_provider: JfrogTokenProvider,
    pub registry_host: String,
    pub client_registry_host: String,
    pub docker_repo_key: String,
    pub push_permissions: String,
    pub pull_permissions: String,
    pub push_token_ttl: u64,
    pub pull_token_ttl: u64,
    pub mint_pull_secrets: bool,
}

/// Token provider backend for JFrog registry
#[cfg(feature = "backend")]
#[derive(Debug, Clone)]
pub enum JfrogTokenProvider {
    Vault {
        vault_addr: String,
        vault_token: String,
        vault_token_file: Option<String>,
        vault_mount_path: String,
        role: String,
        /// When true (default), Rise sends `?scope=...&ttl=...s` to the Vault endpoint,
        /// overriding the role's default scope. With the Rise fork of
        /// vault-plugin-secrets-artifactory, configure admin
        /// `allow_scope_override="opt-in"` and role `allow_scope_override=true`
        /// with narrow `allowed_scopes`.
        /// When false, Rise sends only `?ttl=...s` and the role's configured scope is used.
        scope_override: bool,
    },
    Direct {
        jfrog_url: String,
        admin_token: String,
    },
}

/// Configuration for GitLab container registry
///
/// Credentials are minted as short-lived scoped JWTs from GitLab's JWT auth endpoint,
/// injected into the container CLI's auth config (not via `docker login`).
#[cfg(feature = "backend")]
#[derive(Debug, Clone, Deserialize)]
pub struct GitLabRegistryConfig {
    /// GitLab instance URL (e.g., "https://gitlab.com")
    pub gitlab_url: String,
    /// Registry URL (e.g., "registry.gitlab.com")
    pub registry_url: String,
    /// Full image path prefix within the registry
    /// (e.g., "my-org/my-project" or "my-org/my-project/rise-apps")
    /// Images are stored at `<registry>/<namespace>/<app>:<tag>`
    pub namespace: String,
    /// GitLab username for authenticating against the JWT endpoint
    pub username: String,
    /// Personal Access Token or Deploy Token
    pub token: String,
    /// When true, the Kubernetes controller creates and manages an image pull secret
    /// in each project namespace using the PAT. Set to false if the cluster already
    /// has its own image pull mechanism configured.
    #[serde(default)]
    pub mint_pull_secrets: bool,
    /// Optional client-facing registry URL override (defaults to registry_url)
    #[serde(default)]
    pub client_registry_url: Option<String>,
}
