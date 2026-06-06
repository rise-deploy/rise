//! Docker deployment backend with Traefik routing.
//!
//! Unlike the Kubernetes backend (which delegates reconciliation to
//! Metacontroller), the Docker backend owns an in-process reconcile loop
//! ([`reconciler::DockerReconciler`]) that converges the Docker daemon's state
//! with the database on a fixed interval.
//!
//! [`DockerBackend`] itself implements [`DeploymentBackend`] and only provides
//! the synchronous HTTP-handler operations: URL computation and a no-op
//! environment cleanup (the reconciler GCs deleted-environment containers on
//! its next tick).

pub mod client;
pub mod container_builder;
pub mod diff;
pub mod env;
pub mod health;
pub mod labels;
pub mod pod_status;
pub mod reconciler;
pub mod rolling;
pub mod traefik_api;

#[cfg(test)]
mod test_helpers;

use super::{DeploymentBackend, DeploymentUrls};
use crate::db::models::{Deployment, Project};
use crate::server::deployment::resource_builder::ResourceBuilder;
use anyhow::Result;
use async_trait::async_trait;
use bollard::Docker;
use sqlx::PgPool;
use std::sync::Arc;

/// Slim Docker backend wrapping a bollard client + `ResourceBuilder`.
///
/// Provides URL computation and environment cleanup for HTTP handlers.
/// Reconciliation/health checks/termination run in
/// [`reconciler::DockerReconciler`], spawned at startup (see `state.rs`).
pub struct DockerBackend {
    docker: Docker,
    resource_builder: Arc<ResourceBuilder>,
    db_pool: PgPool,
}

impl DockerBackend {
    pub fn new(docker: Docker, resource_builder: Arc<ResourceBuilder>, db_pool: PgPool) -> Self {
        Self {
            docker,
            resource_builder,
            db_pool,
        }
    }

    /// Test Docker daemon connectivity.
    pub async fn test_connection(&self) -> Result<()> {
        self.docker
            .ping()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to connect to Docker daemon: {}", e))?;
        Ok(())
    }
}

#[async_trait]
impl DeploymentBackend for DockerBackend {
    async fn get_deployment_urls(
        &self,
        deployment: &Deployment,
        project: &Project,
    ) -> Result<DeploymentUrls> {
        let environment = if let Some(env_id) = deployment.environment_id {
            crate::db::environments::find_by_id(&self.db_pool, env_id).await?
        } else {
            None
        };
        let all_environments =
            crate::db::environments::list_for_project(&self.db_pool, project.id).await?;
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

    async fn cleanup_environment(&self, project: &Project, environment_name: &str) -> Result<()> {
        // The reconciler GCs containers belonging to a deleted environment on
        // its next tick (they fall out of the desired set), so no direct
        // cleanup is needed here.
        tracing::debug!(
            project = %project.name,
            environment = %environment_name,
            "Docker backend: environment cleanup deferred to reconcile loop"
        );
        Ok(())
    }
}
