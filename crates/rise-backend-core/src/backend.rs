//! The `DeploymentBackend` contract implemented by each deployment runtime
//! (Kubernetes, Docker) and consumed by `rise-deploy`'s HTTP handlers.

use async_trait::async_trait;

use crate::models::{Deployment, Project};

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

    /// A project's desired state changed — a new deployment, a status change, a
    /// stop, an access-class or custom-domain edit.
    ///
    /// Best-effort: callers log a failure and carry on, because the change is
    /// already durable in the database and the backend will converge on it
    /// regardless. This only asks the backend to converge *sooner*.
    ///
    /// Backends that run their own polling reconcile loop pick the change up on
    /// the next tick, so the default no-op is correct for them.
    async fn project_changed(&self, project: &Project) -> anyhow::Result<()> {
        let _ = project;
        Ok(())
    }

    /// A project was created.
    ///
    /// Backends with a per-project runtime object create it here. Backends that
    /// discover projects from the database need nothing.
    async fn project_created(&self, project: &Project) -> anyhow::Result<()> {
        let _ = project;
        Ok(())
    }

    /// A project was marked deleting.
    ///
    /// Backends with a per-project runtime object delete it here, which is what
    /// starts their finalization. Backends that garbage-collect from the
    /// database on their next reconcile need nothing.
    async fn project_deleting(&self, project: &Project) -> anyhow::Result<()> {
        let _ = project;
        Ok(())
    }

    /// Reject, at request time, container names that would produce runtime
    /// resource names this backend cannot represent.
    ///
    /// `Err` carries a user-facing message and is surfaced as a 400. The limits
    /// are per-backend — a name that is fine on one runtime may be too long on
    /// another — so the default accepts everything and each backend narrows it.
    fn validate_container_names(
        &self,
        project_name: &str,
        deployment_group: &str,
        deployment_id: &str,
        container_names: &[&str],
    ) -> Result<(), String> {
        let _ = (
            project_name,
            deployment_group,
            deployment_id,
            container_names,
        );
        Ok(())
    }

    /// Static facts about the runtime, surfaced by the platform capabilities
    /// endpoint so clients can adapt (e.g. build for the right architecture).
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::default()
    }
}

/// What a deployment backend's runtime can tell clients about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackendCapabilities {
    /// CPU architecture the runtime schedules onto (`amd64`, `arm64`), when the
    /// backend pins one. `None` means it does not constrain the choice.
    pub runtime_arch: Option<String>,
    /// Whether the runtime enforces a non-root security context. `None` when
    /// the concept does not apply to this runtime.
    pub pod_security_enabled: Option<bool>,
}
