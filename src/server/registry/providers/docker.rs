use anyhow::Result;
use async_trait::async_trait;

use crate::server::registry::{
    models::{OciClientAuthConfig, RegistryCredentials},
    ImageTagType, RegistryProvider,
};

/// OCI registry provider with optional static authentication
///
/// With no configured credentials, this provider assumes the user has already
/// authenticated via `docker login` or equivalent. Static credentials, when
/// configured, are returned to authorized clients and used for image pulls.
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
        anyhow::ensure!(
            config.username.is_empty() == config.password.is_empty(),
            "OCI registry static credentials require both username and password"
        );

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

        // The client_registry_url is used for push operations, while registry_url is used by deployment controllers.
        // Empty credentials preserve client-managed authentication via docker login.
        Ok(RegistryCredentials {
            registry_url: self.client_registry_url.clone(),
            username: self.config.username.clone(),
            password: self.config.password.clone(),
            expires_in: None,
            auth_method: Default::default(),
        })
    }

    async fn get_pull_credentials(&self) -> Result<(String, String)> {
        Ok((self.config.username.clone(), self.config.password.clone()))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn config(username: &str, password: &str) -> OciClientAuthConfig {
        OciClientAuthConfig {
            registry_url: "registry.internal:5000".to_string(),
            namespace: "rise-apps".to_string(),
            client_registry_url: Some("registry.example.com".to_string()),
            username: username.to_string(),
            password: password.to_string(),
        }
    }

    #[tokio::test]
    async fn returns_static_credentials_for_push_and_pull() {
        let provider = OciClientAuthProvider::new(config("rise", "secret")).unwrap();

        let push = provider
            .get_credentials("my-app", &["deployment-1"])
            .await
            .unwrap();
        assert_eq!(push.registry_url, "registry.example.com/rise-apps");
        assert_eq!(push.username, "rise");
        assert_eq!(push.password, "secret");
        assert_eq!(push.expires_in, None);

        let pull = provider.get_pull_credentials().await.unwrap();
        assert_eq!(pull, ("rise".to_string(), "secret".to_string()));
    }

    #[tokio::test]
    async fn preserves_client_managed_auth_when_credentials_are_empty() {
        let provider = OciClientAuthProvider::new(config("", "")).unwrap();

        let push = provider
            .get_credentials("my-app", &["deployment-1"])
            .await
            .unwrap();
        assert!(push.username.is_empty());
        assert!(push.password.is_empty());
        assert_eq!(
            provider.get_pull_credentials().await.unwrap(),
            (String::new(), String::new())
        );
    }

    #[test]
    fn rejects_partial_static_credentials() {
        let error = OciClientAuthProvider::new(config("rise", ""))
            .err()
            .expect("partial credentials must fail");
        assert_eq!(
            error.to_string(),
            "OCI registry static credentials require both username and password"
        );
    }
}
