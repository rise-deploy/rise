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

    async fn stream_logs(
        &self,
        deployment: &Deployment,
        project: &Project,
        follow: bool,
        tail_lines: Option<i64>,
        timestamps: bool,
        since_seconds: Option<i64>,
    ) -> Result<futures::stream::BoxStream<'static, Result<bytes::Bytes, anyhow::Error>>> {
        use futures::StreamExt;
        use k8s_openapi::api::core::v1::Pod;
        use kube::api::{Api, ListParams, LogParams};

        // Derive namespace from project name
        let namespace = self.resource_builder.namespace_name(project);

        // Find pod using label selector
        let pod_api: Api<Pod> = Api::namespaced(self.kube_client.clone(), &namespace);
        let pods = pod_api
            .list(&ListParams::default().labels(&format!(
                "rise.dev/deployment-id={}",
                deployment.deployment_id
            )))
            .await?;

        let pod = pods
            .items
            .first()
            .ok_or_else(|| anyhow::anyhow!("Pod not found - deployment may not be ready yet"))?;

        let pod_name = pod
            .metadata
            .name
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Pod name not found"))?
            .clone();

        // Build LogParams
        let mut log_params = LogParams {
            follow,
            timestamps,
            ..Default::default()
        };
        if let Some(tail) = tail_lines {
            log_params.tail_lines = Some(tail);
        }
        if let Some(since) = since_seconds {
            log_params.since_seconds = Some(since);
        }

        // Stream logs from pod
        let mut log_stream = pod_api.log_stream(&pod_name, &log_params).await?;

        // Convert AsyncBufRead to Stream of Bytes
        use futures::AsyncReadExt;
        let stream = async_stream::stream! {
            let mut buffer = vec![0u8; 8192];
            loop {
                match log_stream.read(&mut buffer).await {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        yield Ok(bytes::Bytes::copy_from_slice(&buffer[..n]));
                    }
                    Err(e) => {
                        yield Err(anyhow::anyhow!("Log stream error: {}", e));
                        break;
                    }
                }
            }
        };

        Ok(stream.boxed())
    }
}
