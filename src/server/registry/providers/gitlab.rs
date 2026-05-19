use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

use crate::server::registry::{
    models::{GitLabRegistryConfig, RegistryAuthMethod, RegistryCredentials},
    ImageTagType, RegistryProvider,
};

/// GitLab container registry provider
///
/// Mints short-lived (~15 min) scoped JWTs from GitLab's JWT auth endpoint for each
/// push operation. The JWT is injected directly into the container CLI's auth config
/// file (not via `docker login`) using the `registrytoken` key.
pub struct GitLabRegistryProvider {
    config: GitLabRegistryConfig,
    http_client: reqwest::Client,
    registry_host: String,
    registry_url: String,
    client_registry_url: String,
    /// The path prefix used in JWT scopes: <namespace>[/<image_prefix>]
    path_prefix: String,
}

#[derive(Deserialize)]
struct JwtAuthResponse {
    token: String,
}

impl GitLabRegistryProvider {
    pub fn new(config: GitLabRegistryConfig) -> Result<Self> {
        let registry_host = config
            .registry_url
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap_or(&config.registry_url)
            .to_string();

        let path_prefix = config.namespace.trim_matches('/').to_string();

        let registry_url = format!(
            "{}/{}",
            config.registry_url.trim_end_matches('/'),
            path_prefix
        );

        let client_base = config
            .client_registry_url
            .as_deref()
            .unwrap_or(&config.registry_url);
        let client_registry_url = format!("{}/{}", client_base.trim_end_matches('/'), path_prefix);

        let http_client = reqwest::Client::new();

        Ok(Self {
            config,
            http_client,
            registry_host,
            registry_url,
            client_registry_url,
            path_prefix,
        })
    }

    async fn fetch_jwt(&self, image_path: &str, actions: &str) -> Result<String> {
        let scope = format!("repository:{}:{}", image_path, actions);
        let base_url = format!("{}/jwt/auth", self.config.gitlab_url.trim_end_matches('/'));
        let url = reqwest::Url::parse_with_params(
            &base_url,
            &[("service", "container_registry"), ("scope", scope.as_str())],
        )
        .context("Failed to build GitLab JWT auth URL")?;

        tracing::debug!("Fetching GitLab registry JWT for scope: {}", scope);

        let response = self
            .http_client
            .get(url)
            .basic_auth(&self.config.username, Some(&self.config.token))
            .send()
            .await
            .context("Failed to reach GitLab JWT auth endpoint")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("GitLab JWT auth returned {}: {}", status, body);
        }

        let jwt: JwtAuthResponse = response
            .json()
            .await
            .context("Failed to parse GitLab JWT auth response")?;

        Ok(jwt.token)
    }
}

#[async_trait]
impl RegistryProvider for GitLabRegistryProvider {
    async fn get_credentials(&self, repository: &str, _tag: &str) -> Result<RegistryCredentials> {
        // JWT scope path: <namespace>[/<image_prefix>]/<repository>
        let image_path = format!("{}/{}", self.path_prefix, repository);

        tracing::info!(
            repository = repository,
            "Fetching scoped GitLab registry JWT"
        );

        let jwt = self.fetch_jwt(&image_path, "push,pull").await?;

        Ok(RegistryCredentials {
            registry_url: format!("{}/{}", self.client_registry_url, repository),
            username: String::new(),
            password: jwt,
            expires_in: Some(900),
            auth_method: RegistryAuthMethod::RegistryToken,
        })
    }

    async fn get_pull_credentials(&self) -> Result<(String, String)> {
        // PAT is used for K8s pull secrets (default get_k8s_pull_credentials impl) and
        // OCI digest resolution. containerd does not support pre-obtained bearer tokens
        // (RegistryToken/registrytoken) in K8s pull secrets — it ignores them and falls back
        // to anonymous auth. The PAT lets the container runtime do its own JWT exchange
        // with GitLab's auth endpoint on each pull.
        Ok((self.config.username.clone(), self.config.token.clone()))
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
