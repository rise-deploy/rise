use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use crate::server::registry::{
    models::{JfrogConfig, JfrogTokenProvider, RegistryAuthMethod, RegistryCredentials},
    ImageTagType, RegistryProvider,
};

/// Cached pull credentials for a specific scope
struct CachedPullCredentials {
    username: String,
    access_token: String,
    created_at: Instant,
    refresh_after: Duration,
}

/// JFrog Artifactory container registry provider
///
/// Mints short-lived scoped access tokens for each push/pull operation.
/// Supports two token-issuing backends:
/// - Vault (via vault-plugin-secrets-artifactory)
/// - Direct (via JFrog's access token API)
pub struct JfrogProvider {
    config: JfrogConfig,
    http_client: reqwest::Client,
    registry_host: String,
    registry_url: String,
    client_registry_url: String,
    /// Per-scope cache for pull credentials (keyed by scope string)
    pull_cache: RwLock<HashMap<String, CachedPullCredentials>>,
}

#[derive(Deserialize)]
struct VaultTokenResponse {
    data: VaultTokenData,
}

#[derive(Deserialize)]
struct VaultTokenData {
    access_token: String,
    username: String,
}

#[derive(Deserialize)]
struct DirectTokenResponse {
    access_token: String,
    /// Username associated with the token (provided by JFrog)
    #[serde(default)]
    username: Option<String>,
}

impl JfrogProvider {
    pub fn new(config: JfrogConfig) -> Result<Self> {
        let registry_host = config.registry_host.clone();
        let registry_url = format!("{}/{}", config.registry_host, config.docker_repo_key);
        let client_registry_url =
            format!("{}/{}", config.client_registry_host, config.docker_repo_key);

        Ok(Self {
            config,
            http_client: reqwest::Client::new(),
            registry_host,
            registry_url,
            client_registry_url,
            pull_cache: RwLock::new(HashMap::new()),
        })
    }

    /// Read the current Vault token, supporting file-based rotation.
    fn resolve_vault_token(static_token: &str, token_file: &Option<String>) -> Result<String> {
        if let Some(path) = token_file {
            let token = std::fs::read_to_string(path)
                .with_context(|| format!("Failed to read Vault token from file: {}", path))?;
            let token = token.trim().to_string();
            if token.is_empty() {
                anyhow::bail!("Vault token file '{}' is empty", path);
            }
            Ok(token)
        } else {
            Ok(static_token.to_string())
        }
    }

    /// Request a scoped token from the configured backend.
    ///
    /// `scope` is the desired JFrog artifact scope string. For Vault mode with
    /// `scope_override: false`, the scope parameter is ignored and the Vault
    /// role's configured scope is used instead.
    async fn request_token(&self, scope: &str, ttl: u64) -> Result<(String, String)> {
        match &self.config.token_provider {
            JfrogTokenProvider::Vault {
                vault_addr,
                vault_token,
                vault_token_file,
                vault_mount_path,
                role,
                scope_override,
            } => {
                let token = Self::resolve_vault_token(vault_token, vault_token_file)?;
                let url = format!(
                    "{}/v1/{}/token/{}",
                    vault_addr.trim_end_matches('/'),
                    vault_mount_path,
                    role
                );

                // The vault-plugin-secrets-artifactory uses GET with query
                // parameters (POST returns "unsupported operation").
                let request_url = if *scope_override {
                    tracing::debug!(
                        scope = scope,
                        ttl = ttl,
                        "Requesting JFrog token via Vault (scope override)"
                    );
                    format!("{}?scope={}&ttl={}s", url, urlencoding::encode(scope), ttl)
                } else {
                    tracing::debug!(ttl = ttl, "Requesting JFrog token via Vault (role scope)");
                    format!("{}?ttl={}s", url, ttl)
                };
                let response = self
                    .http_client
                    .get(&request_url)
                    .header("X-Vault-Token", &token)
                    .send()
                    .await
                    .context("Failed to reach Vault token endpoint")?;

                if !response.status().is_success() {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    anyhow::bail!("Vault token request returned {}: {}", status, body);
                }

                let vault_resp: VaultTokenResponse = response
                    .json()
                    .await
                    .context("Failed to parse Vault token response")?;

                Ok((vault_resp.data.username, vault_resp.data.access_token))
            }
            JfrogTokenProvider::Direct {
                jfrog_url,
                admin_token,
            } => {
                let url = format!("{}/access/api/v1/tokens", jfrog_url.trim_end_matches('/'));

                tracing::debug!(
                    scope = scope,
                    ttl = ttl,
                    "Requesting JFrog token via Direct API"
                );

                let response = self
                    .http_client
                    .post(&url)
                    .header("Authorization", format!("Bearer {}", admin_token))
                    .json(&serde_json::json!({
                        "scope": scope,
                        "expires_in": ttl,
                    }))
                    .send()
                    .await
                    .context("Failed to reach JFrog token endpoint")?;

                if !response.status().is_success() {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    anyhow::bail!("JFrog token request returned {}: {}", status, body);
                }

                let token_resp: DirectTokenResponse = response
                    .json()
                    .await
                    .context("Failed to parse JFrog token response")?;

                let username = token_resp.username.unwrap_or_else(|| "admin".to_string());
                Ok((username, token_resp.access_token))
            }
        }
    }

    /// Build an artifact scope string with a recursive wildcard.
    /// JFrog uses `**` for recursive path matching; `*` only matches one level.
    fn artifact_scope(&self, repository: &str, permissions: &str) -> String {
        format!(
            "artifact:{}/**:{}",
            self.scope_path(repository),
            permissions
        )
    }

    /// Build the scope path: `<docker_repo_key>/<repository>`
    fn scope_path(&self, repository: &str) -> String {
        format!("{}/{}", self.config.docker_repo_key, repository)
    }
}

#[async_trait]
impl RegistryProvider for JfrogProvider {
    async fn get_credentials(&self, repository: &str, tag: &str) -> Result<RegistryCredentials> {
        // Docker/OCI push writes to three sub-path groups:
        //   1. {tag}/** — manifest (manifest.json or list.manifest.json)
        //   2. _uploads/** — blob staging during upload
        //   3. sha256*/* — content-addressed manifests written by BuildKit
        //      (attestations, multi-platform indexes stored as siblings of the tag)
        //
        // The sha256 scope uses `sha256*/*` (not `sha256:*`) because JFrog
        // stores these as `sha256:{digest}/` directories and the colon is
        // matched by the glob wildcard.
        let base = self.scope_path(repository);
        let perms = &self.config.push_permissions;
        let scope = format!(
            "artifact:{base}/{tag}/**:{perms} artifact:{base}/_uploads/**:{perms} artifact:{base}/sha256*/*:{perms}",
        );

        tracing::info!(
            repository = repository,
            tag = tag,
            scope = %scope,
            "Fetching scoped JFrog push token"
        );

        let (username, access_token) = self
            .request_token(&scope, self.config.push_token_ttl)
            .await?;

        Ok(RegistryCredentials {
            registry_url: format!("{}/{}", self.client_registry_url, repository),
            username,
            password: access_token,
            expires_in: Some(self.config.push_token_ttl),
            auth_method: RegistryAuthMethod::LoginCredentials,
        })
    }

    async fn get_pull_credentials(&self) -> Result<(String, String)> {
        // Return empty credentials. Pull credentials are always project-scoped
        // via get_k8s_pull_credentials.
        Ok((String::new(), String::new()))
    }

    async fn get_k8s_pull_credentials(&self, repository: &str) -> Result<RegistryCredentials> {
        let scope = self.artifact_scope(repository, &self.config.pull_permissions);

        // Check cache under read lock
        {
            let cache = self.pull_cache.read().unwrap();
            if let Some(entry) = cache.get(&scope) {
                if entry.created_at.elapsed() < entry.refresh_after {
                    tracing::debug!(
                        repository = repository,
                        scope = %scope,
                        "Using cached JFrog pull credentials"
                    );
                    return Ok(RegistryCredentials {
                        registry_url: self.registry_host.clone(),
                        username: entry.username.clone(),
                        password: entry.access_token.clone(),
                        expires_in: Some(self.config.pull_token_ttl),
                        auth_method: RegistryAuthMethod::LoginCredentials,
                    });
                }
            }
        }

        // Cache miss or expired — mint a new token
        tracing::info!(
            repository = repository,
            scope = %scope,
            "Fetching scoped JFrog pull token"
        );

        let (username, access_token) = self
            .request_token(&scope, self.config.pull_token_ttl)
            .await?;

        // Update cache under write lock
        {
            let mut cache = self.pull_cache.write().unwrap();
            cache.insert(
                scope,
                CachedPullCredentials {
                    username: username.clone(),
                    access_token: access_token.clone(),
                    created_at: Instant::now(),
                    refresh_after: Duration::from_secs(self.config.pull_token_ttl * 2 / 3),
                },
            );
        }

        Ok(RegistryCredentials {
            registry_url: self.registry_host.clone(),
            username,
            password: access_token,
            expires_in: Some(self.config.pull_token_ttl),
            auth_method: RegistryAuthMethod::LoginCredentials,
        })
    }

    fn registry_host(&self) -> &str {
        &self.registry_host
    }

    fn registry_url(&self) -> &str {
        &self.registry_url
    }

    fn get_image_tag(&self, repository: &str, tag: &str, tag_type: ImageTagType) -> String {
        let base = match tag_type {
            ImageTagType::ClientFacing => &self.client_registry_url,
            ImageTagType::Internal => &self.registry_url,
        };
        format!("{}/{}:{}", base, repository, tag)
    }

    fn requires_pull_secret(&self) -> bool {
        self.config.mint_pull_secrets
    }
}
