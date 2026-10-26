//! Slim Kubernetes backend for Metacontroller-based deployments.
//!
//! All reconciliation, health checks, and infrastructure management are handled
//! by the Metacontroller sync webhook. This module provides only the remaining
//! operations needed by HTTP handlers: log streaming, URL computation, and
//! environment cleanup.

use crate::resource_builder::ResourceBuilder;
use anyhow::Result;
use async_trait::async_trait;
use rise_backend_core::models::{Deployment, Project};
use rise_backend_core::BackendCapabilities;
use rise_backend_core::DeploymentStore;
use rise_backend_core::{DeploymentBackend, DeploymentUrls};
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
        let environment = self
            .store
            .find_environment(deployment.environment_id)
            .await?;

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
        let environments = self.store.list_environments_for_project(project.id).await?;
        let custom_domains = self.store.list_project_custom_domains(project.id).await?;

        Ok(self.resource_builder.compute_project_urls(
            project,
            deployment_group,
            &environments,
            &custom_domains,
        ))
    }

    async fn cleanup_environment(&self, project: &Project, _environment_name: &str) -> Result<()> {
        // With Metacontroller, environment cleanup is handled automatically:
        // the sync webhook won't return ServiceAccount resources for deleted
        // environments, and Metacontroller will garbage-collect them. The
        // resync just makes it happen promptly.
        self.project_changed(project).await
    }

    async fn project_changed(&self, project: &Project) -> Result<()> {
        crate::crd::trigger_resync(&self.kube_client, &project.name).await
    }

    async fn project_created(&self, project: &Project) -> Result<()> {
        crate::crd::ensure_rise_project(
            &self.kube_client,
            &project.name,
            self.controller_class.as_deref(),
        )
        .await
    }

    async fn project_deleting(&self, project: &Project) -> Result<()> {
        crate::crd::delete_rise_project(&self.kube_client, &project.name).await
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            // The node selector pins the architecture pods are scheduled onto.
            runtime_arch: self
                .resource_builder
                .node_selector
                .get("kubernetes.io/arch")
                .and_then(|arch| rise_backend_core::normalize_runtime_arch(arch)),
            pod_security_enabled: Some(self.resource_builder.pod_security_enabled),
        }
    }
}
