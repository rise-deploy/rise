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
use sqlx::PgPool;
use std::sync::Arc;

/// Slim Kubernetes backend wrapping ResourceBuilder and kube client.
///
/// Provides log streaming, URL computation, and environment cleanup.
/// Reconciliation/health checks/termination are handled by the Metacontroller
/// sync webhook (`src/server/deployment/webhook.rs`).
pub struct KubernetesBackend {
    kube_client: kube::Client,
    resource_builder: Arc<ResourceBuilder>,
    db_pool: PgPool,
}

impl KubernetesBackend {
    pub fn new(
        kube_client: kube::Client,
        resource_builder: Arc<ResourceBuilder>,
        db_pool: PgPool,
    ) -> Self {
        Self {
            kube_client,
            resource_builder,
            db_pool,
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
            crate::db::environments::find_by_id(&self.db_pool, env_id).await?
        } else {
            None
        };

        // All envs are needed so `compute_deployment_urls` can suppress a DG URL
        // whose host would collide with another env's URL (see
        // `host_conflicts_with_other_env`).
        let all_environments =
            crate::db::environments::list_for_project(&self.db_pool, project.id).await?;

        // Load custom domains for the project
        let custom_domains =
            crate::db::custom_domains::list_project_custom_domains(&self.db_pool, project.id)
                .await?;

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
        let custom_domains =
            crate::db::custom_domains::list_project_custom_domains(&self.db_pool, project.id)
                .await?;

        Ok(self
            .resource_builder
            .compute_project_urls(project, deployment_group, &custom_domains))
    }

    async fn cleanup_environment(&self, project: &Project, _environment_name: &str) -> Result<()> {
        // With Metacontroller, environment cleanup is handled automatically:
        // the sync webhook won't return ServiceAccount resources for deleted
        // environments, and Metacontroller will garbage-collect them.
        // Trigger a resync to make this happen promptly.
        if let Err(e) =
            crate::server::deployment::crd::trigger_resync(&self.kube_client, &project.name).await
        {
            tracing::warn!(
                "Failed to trigger CRD resync for environment cleanup on project '{}': {:?}",
                project.name,
                e
            );
        }
        Ok(())
    }
}
