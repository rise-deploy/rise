#[cfg(feature = "backend")]
mod kubernetes;

#[cfg(feature = "backend")]
pub use kubernetes::KubernetesBackend;

use async_trait::async_trait;

use crate::db::models::{Deployment, Project};

/// URLs where a deployment can be accessed
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DeploymentUrls {
    /// Default URL based on ingress template configuration
    pub default_url: String,
    /// Primary URL - the starred custom domain if one exists, otherwise the default URL
    pub primary_url: String,
    /// Additional URLs for custom domains
    pub custom_domain_urls: Vec<String>,
    /// Full ordered list of every URL the deployment is reachable at — deployment-group
    /// URL, environment URL, production URL, and any custom domains. Older clients can
    /// ignore this; new code prefers it over the narrower fields above.
    #[serde(default)]
    pub all_urls: Vec<String>,
}

/// Trait that all deployment backends must implement
///
/// With Metacontroller, reconciliation/health checks/termination are handled
/// by the sync webhook. This trait provides the remaining backend operations
/// needed by HTTP handlers: log streaming, URL computation, and environment cleanup.
#[async_trait]
pub trait DeploymentBackend: Send + Sync {
    /// Calculate URLs where this deployment can be accessed
    ///
    /// Returns the primary URL (from ingress templates) and any custom domain URLs.
    /// URLs are calculated dynamically based on current controller configuration.
    async fn get_deployment_urls(
        &self,
        deployment: &Deployment,
        project: &Project,
    ) -> anyhow::Result<DeploymentUrls>;

    /// Calculate URLs where a project would be accessed for a given deployment group.
    ///
    /// Similar to `get_deployment_urls` but takes a group name string instead of a Deployment object.
    /// Used for preview endpoints where no deployment exists yet.
    async fn get_project_urls(
        &self,
        project: &Project,
        deployment_group: &str,
    ) -> anyhow::Result<DeploymentUrls>;

    /// Clean up resources associated with a deleted environment
    ///
    /// With Metacontroller, this triggers a resync so the webhook stops returning
    /// resources for the deleted environment. Direct cleanup is no longer needed
    /// as Metacontroller handles resource garbage collection.
    async fn cleanup_environment(
        &self,
        project: &Project,
        environment_name: &str,
    ) -> anyhow::Result<()> {
        let _ = (project, environment_name);
        Ok(())
    }
}
