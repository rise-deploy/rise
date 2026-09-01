//! Slim Kubernetes backend for Metacontroller-based deployments.
//!
//! All reconciliation, health checks, and infrastructure management are handled
//! by the Metacontroller sync webhook. This module provides only the remaining
//! operations needed by HTTP handlers: log streaming, URL computation, and
//! environment cleanup.

use super::{DeploymentBackend, DeploymentUrls};
use crate::db::models::{Deployment, Project};
use crate::server::deployment::resource_builder::ResourceBuilder;
use anyhow::Result;
use async_trait::async_trait;
use rise_backend_core::BackendCapabilities;
use rise_backend_core::DeploymentStore;
use std::sync::Arc;

/// Slim Kubernetes backend wrapping ResourceBuilder and kube client.
///
/// Provides log streaming, URL computation, and environment cleanup.
/// Reconciliation/health checks/termination are handled by the Metacontroller
/// sync webhook (`src/server/deployment/webhook.rs`).
pub struct KubernetesBackend {
    kube_client: kube::Client,
    resource_builder: Arc<ResourceBuilder>,
    store: Arc<dyn DeploymentStore>,
    /// Stamped on the RiseProjects this backend creates, so a project is only
    /// reconciled by the controller that owns its Organization.
    controller_class: Option<String>,
}

impl KubernetesBackend {
    pub fn new(
        kube_client: kube::Client,
        resource_builder: Arc<ResourceBuilder>,
        store: Arc<dyn DeploymentStore>,
        controller_class: Option<String>,
    ) -> Self {
        Self {
            kube_client,
            resource_builder,
            store,
            controller_class,
        }
    }

    /// Test Kubernetes API connectivity by listing pods (ClusterRole grants pod read access)
    pub async fn test_connection(&self) -> Result<()> {
        use k8s_openapi::api::core::v1::Pod;
        use kube::api::Api;
        let pod_api: Api<Pod> = Api::all(self.kube_client.clone());
        pod_api
            .list(&kube::api::ListParams::default().limit(1))
            .await
            .map_err(|e| anyhow::anyhow!("Failed to connect to Kubernetes API: {}", e))?;
        Ok(())
    }
}

#[async_trait]
impl DeploymentBackend for KubernetesBackend {
    async fn get_deployment_urls(
        &self,
        deployment: &Deployment,
        project: &Project,
    ) -> Result<DeploymentUrls> {
        // Load environment info if the deployment has one
        let environment = if let Some(env_id) = deployment.environment_id {
            self.store.find_environment(env_id).await?
        } else {
            None
        };

        // All envs are needed so `compute_deployment_urls` can suppress a DG URL
        // whose host would collide with another env's URL (see
        // `host_conflicts_with_other_env`).
        let all_environments = self.store.list_environments_for_project(project.id).await?;

        // Load custom domains for the project
        let custom_domains = self.store.list_project_custom_domains(project.id).await?;

        Ok(self.resource_builder.compute_deployment_urls(
            project,
            deployment,
            environment.as_ref(),
            &all_environments,
            &custom_domains,
        ))
    }

    async fn get_project_urls(
        &self,
        project: &Project,
        deployment_group: &str,
    ) -> Result<DeploymentUrls> {
        let custom_domains = self.store.list_project_custom_domains(project.id).await?;

        Ok(self
            .resource_builder
            .compute_project_urls(project, deployment_group, &custom_domains))
    }

    async fn cleanup_environment(&self, project: &Project, _environment_name: &str) -> Result<()> {
        // With Metacontroller, environment cleanup is handled automatically:
        // the sync webhook won't return ServiceAccount resources for deleted
        // environments, and Metacontroller will garbage-collect them. The
        // resync just makes it happen promptly.
        self.project_changed(project).await
    }

    async fn project_changed(&self, project: &Project) -> Result<()> {
        crate::server::deployment::crd::trigger_resync(&self.kube_client, &project.name).await
    }

    async fn project_created(&self, project: &Project) -> Result<()> {
        crate::server::deployment::crd::ensure_rise_project(
            &self.kube_client,
            &project.name,
            self.controller_class.as_deref(),
        )
        .await
    }

    async fn project_deleting(&self, project: &Project) -> Result<()> {
        crate::server::deployment::crd::delete_rise_project(&self.kube_client, &project.name).await
    }

    fn validate_container_names(
        &self,
        project_name: &str,
        deployment_group: &str,
        deployment_id: &str,
        container_names: &[&str],
    ) -> std::result::Result<(), String> {
        validate_container_names(
            project_name,
            deployment_group,
            deployment_id,
            container_names,
        )
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            // The node selector pins the architecture pods are scheduled onto.
            runtime_arch: self
                .resource_builder
                .node_selector
                .get("kubernetes.io/arch")
                .and_then(|arch| crate::server::platform::models::normalize_runtime_arch(arch)),
            pod_security_enabled: Some(self.resource_builder.pod_security_enabled),
        }
    }
}

/// Check that the Kubernetes resource names a deployment will generate stay
/// within Kubernetes' limits.
///
/// The per-container Service name `<group>-<container>` must be a DNS-1035
/// label (≤ 63 chars) — the binding constraint, since the deployment group and
/// container name share that budget. The per-container Deployment name
/// `<project>-<deployment_id>-<container>` must be a DNS-1123 subdomain
/// (≤ 253 chars). Metacontroller surfaces no apply error for an over-limit name
/// back to the deployment, so this rejects at request time instead.
fn validate_container_names(
    project_name: &str,
    deployment_group: &str,
    deployment_id: &str,
    container_names: &[&str],
) -> std::result::Result<(), String> {
    let group = ResourceBuilder::escaped_group_name(deployment_group);
    for name in container_names {
        let service_name = format!("{group}-{name}");
        if service_name.len() > 63 {
            return Err(format!(
                "Container '{name}' would produce Service name '{service_name}' ({} chars), over \
                 Kubernetes' 63-character limit. Shorten the deployment group or the container \
                 name.",
                service_name.len()
            ));
        }
        let deployment_name = format!("{project_name}-{deployment_id}-{name}");
        if deployment_name.len() > 253 {
            return Err(format!(
                "Container '{name}' would produce Deployment name '{deployment_name}' ({} chars), \
                 over Kubernetes' 253-character limit. Shorten the project or container name.",
                deployment_name.len()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_container_names;

    #[test]
    fn accepts_normal_names() {
        validate_container_names(
            "my-app",
            "default",
            "20260101-000000",
            &["app", "api", "worker"],
        )
        .expect("normal names should be accepted");
    }

    #[test]
    fn rejects_overlong_service_name() {
        // The deployment group and container name share the 63-char Service-name
        // budget. A long group + container overflows it.
        let long_group = "g".repeat(60);
        let err =
            validate_container_names("proj", &long_group, "20260101-000000", &["api"]).unwrap_err();
        assert!(err.contains("63-character"), "got: {err}");
    }

    #[test]
    fn single_container_app_is_checked() {
        // Single-container deployments also emit a suffixed Service `<group>-app`.
        let long_group = "g".repeat(61);
        let err =
            validate_container_names("proj", &long_group, "20260101-000000", &["app"]).unwrap_err();
        assert!(err.contains("63-character"), "got: {err}");
    }
}
