use anyhow::Result;
use async_trait::async_trait;

use crate::server::registry::{
    models::{OciClientAuthConfig, RegistryCredentials},
    ImageTagType, RegistryProvider,
};

/// OCI registry provider that relies on client-side authentication
///
/// This provider assumes the user has already authenticated via `docker login`
/// or equivalent. It simply returns the registry URL - no credential generation.
///
/// Works with any OCI-compliant registry (Docker Hub, Harbor, Quay, etc.)
pub struct OciClientAuthProvider {
    #[allow(dead_code)]
    config: OciClientAuthConfig,
    registry_url: String,
    registry_host: String,
    client_registry_url: String,
}

impl OciClientAuthProvider {
    /// Create a new OCI client-auth registry provider
    pub fn new(config: OciClientAuthConfig) -> Result<Self> {
        // Extract host from registry_url (remove protocol and path)
        let registry_host = config
            .registry_url
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap_or(&config.registry_url)
            .to_string();

        // Trim trailing slashes from namespace to prevent double slashes in image tags
        let namespace = config.namespace.trim_end_matches('/');

        let registry_url = if namespace.is_empty() {
            config.registry_url.trim_end_matches('/').to_string()
        } else {
            format!(
                "{}/{}",
                config.registry_url.trim_end_matches('/'),
                namespace
            )
        };

        // Calculate client-facing registry URL (use client_registry_url if provided, otherwise use registry_url)
        let client_base = config
            .client_registry_url
            .as_ref()
            .unwrap_or(&config.registry_url);
        let client_registry_url = if namespace.is_empty() {
            client_base.trim_end_matches('/').to_string()
        } else {
            format!("{}/{}", client_base.trim_end_matches('/'), namespace)
        };

        Ok(Self {
            config,
            registry_url,
            registry_host,
            client_registry_url,
        })
    }
}

#[async_trait]
impl RegistryProvider for OciClientAuthProvider {
    async fn get_credentials(
        &self,
        repository: &str,
        _tags: &[&str],
    ) -> Result<RegistryCredentials> {
        tracing::info!("Returning OCI registry info for repository: {}", repository);

        // Return client-facing registry URL - credentials assumed to be configured via docker login
        // The client_registry_url is used for push operations, while registry_url is used by deployment controllers
        Ok(RegistryCredentials {
            registry_url: self.client_registry_url.clone(),
            username: String::new(), // Empty - docker CLI uses stored credentials
            password: String::new(), // Empty - docker CLI uses stored credentials
            expires_in: None,
            auth_method: Default::default(),
        })
    }

    async fn get_pull_credentials(&self) -> Result<(String, String)> {
        // Client-auth provider assumes docker login was used
        // Return empty credentials - the docker CLI will use stored credentials
        Ok((String::new(), String::new()))
    }

    fn registry_host(&self) -> &str {
        &self.registry_host
    }

    fn registry_url(&self) -> &str {
        &self.registry_url
    }

    fn get_image_tag(&self, repository: &str, tag: &str, tag_type: ImageTagType) -> String {
        let registry_url = match tag_type {
            ImageTagType::ClientFacing => &self.client_registry_url,
            ImageTagType::Internal => &self.registry_url,
        };
        format!("{}/{}:{}", registry_url, repository, tag)
    }
}
