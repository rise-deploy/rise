//! `PgDeploymentStore` — the SQLX-backed implementation of
//! `rise_backend_core::DeploymentStore`.
//!
//! It is a thin pass-through over the `crate::db` helpers, holding the
//! connection pool so the deployment controllers can read and mutate deployment
//! state through the trait rather than reaching into `crate::db` directly. This
//! is the seam that lets the controllers move into their own crates.

use anyhow::Result;
use async_trait::async_trait;
use rise_backend_core::models::{
    CustomDomain, Deployment, DeploymentEnvVar, DeploymentStatus, Environment, Project,
    TerminationReason,
};
use rise_backend_core::DeploymentStore;
use sqlx::PgPool;
use uuid::Uuid;

/// SQLX-backed `DeploymentStore`.
#[derive(Clone)]
pub struct PgDeploymentStore {
    pool: PgPool,
}

impl PgDeploymentStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl DeploymentStore for PgDeploymentStore {
    async fn list_projects(&self, owner_user_id: Option<Uuid>) -> Result<Vec<Project>> {
        crate::db::projects::list(&self.pool, owner_user_id).await
    }

    async fn find_project(&self, id: Uuid) -> Result<Option<Project>> {
        crate::db::projects::find_by_id(&self.pool, id).await
    }

    async fn find_project_by_name(&self, name: &str) -> Result<Option<Project>> {
        crate::db::projects::find_by_name(&self.pool, name).await
    }

    async fn list_active_projects(&self) -> Result<Vec<Project>> {
        crate::db::projects::list_active(&self.pool).await
    }

    async fn update_project_calculated_status(&self, project_id: Uuid) -> Result<Project> {
        crate::db::projects::update_calculated_status(&self.pool, project_id).await
    }

    async fn organization_uid_for_project(&self, project_id: Uuid) -> Result<Option<Uuid>> {
        crate::db::organization_links::organization_uid_for_project(&self.pool, project_id).await
    }

    async fn find_environment(&self, id: Uuid) -> Result<Option<Environment>> {
        crate::db::environments::find_by_id(&self.pool, id).await
    }

    async fn list_environments_for_project(&self, project_id: Uuid) -> Result<Vec<Environment>> {
        crate::db::environments::list_for_project(&self.pool, project_id).await
    }

    async fn list_project_custom_domains(&self, project_id: Uuid) -> Result<Vec<CustomDomain>> {
        crate::db::custom_domains::list_project_custom_domains(&self.pool, project_id).await
    }

    async fn list_deployment_env_vars(&self, deployment_id: Uuid) -> Result<Vec<DeploymentEnvVar>> {
        crate::db::env_vars::list_deployment_env_vars(&self.pool, deployment_id).await
    }

    async fn find_deployment(&self, id: Uuid) -> Result<Option<Deployment>> {
        crate::db::deployments::find_by_id(&self.pool, id).await
    }

    async fn list_non_terminal_deployments_for_project(
        &self,
        project_id: Uuid,
    ) -> Result<Vec<Deployment>> {
        crate::db::deployments::list_non_terminal_for_project(&self.pool, project_id).await
    }

    async fn find_non_terminal_deployments_for_project_and_group(
        &self,
        project_id: Uuid,
        group: &str,
    ) -> Result<Vec<Deployment>> {
        crate::db::deployments::find_non_terminal_for_project_and_group(
            &self.pool, project_id, group,
        )
        .await
    }

    async fn update_deployment_status(
        &self,
        id: Uuid,
        status: DeploymentStatus,
    ) -> Result<Deployment> {
        crate::db::deployments::update_status(&self.pool, id, status, None).await
    }

    async fn update_deployment_controller_metadata(
        &self,
        id: Uuid,
        metadata: &serde_json::Value,
    ) -> Result<Deployment> {
        crate::db::deployments::update_controller_metadata(&self.pool, id, metadata).await
    }

    async fn mark_deployment_failed(
        &self,
        id: Uuid,
        error_message: &str,
    ) -> Result<Option<Deployment>> {
        crate::db::deployments::mark_failed(&self.pool, id, error_message).await
    }

    async fn mark_deployment_cancelling(&self, id: Uuid) -> Result<Option<Deployment>> {
        crate::db::deployments::mark_cancelling(&self.pool, id).await
    }

    async fn mark_deployment_cancelled(&self, id: Uuid) -> Result<Option<Deployment>> {
        crate::db::deployments::mark_cancelled(&self.pool, id).await
    }

    async fn mark_deployment_stopped(&self, id: Uuid) -> Result<Option<Deployment>> {
        crate::db::deployments::mark_stopped(&self.pool, id).await
    }

    async fn mark_deployment_superseded(&self, id: Uuid) -> Result<Option<Deployment>> {
        crate::db::deployments::mark_superseded(&self.pool, id).await
    }

    async fn mark_deployment_expired(&self, id: Uuid) -> Result<Option<Deployment>> {
        crate::db::deployments::mark_expired(&self.pool, id).await
    }

    async fn mark_deployment_healthy(&self, id: Uuid) -> Result<Option<Deployment>> {
        crate::db::deployments::mark_healthy(&self.pool, id).await
    }

    async fn mark_deployment_unhealthy(
        &self,
        id: Uuid,
        reason: String,
    ) -> Result<Option<Deployment>> {
        crate::db::deployments::mark_unhealthy(&self.pool, id, reason).await
    }

    async fn list_container_observations(
        &self,
        deployment_id: Uuid,
    ) -> Result<Vec<rise_backend_core::observation::ContainerObservation>> {
        crate::db::container_observations::list_for_deployment(&self.pool, deployment_id).await
    }

    async fn record_container_observations(
        &self,
        deployment_id: Uuid,
        source: rise_backend_core::events::EventSource,
        observations: &[rise_backend_core::observation::ContainerObservation],
        events: &[rise_backend_core::observation::DerivedEvent],
    ) -> Result<()> {
        crate::db::container_observations::record_observations(
            &self.pool,
            deployment_id,
            source,
            observations,
            events,
        )
        .await
    }

    async fn forward_backend_events(
        &self,
        deployment_id: Uuid,
        source: rise_backend_core::events::EventSource,
        events: &[rise_backend_core::events::ForwardedEvent],
    ) -> Result<u64> {
        crate::db::backend_events::forward(&self.pool, deployment_id, source, events).await
    }

    async fn mark_deployment_terminating(
        &self,
        id: Uuid,
        reason: TerminationReason,
        superseded_by: Option<&str>,
    ) -> Result<Option<Deployment>> {
        crate::db::deployments::mark_terminating(&self.pool, id, reason, superseded_by).await
    }

    async fn mark_deployment_healthy_and_supersede(
        &self,
        deployment_id: Uuid,
        project_id: Uuid,
        deployment_group: &str,
    ) -> Result<rise_backend_core::SupersessionOutcome> {
        crate::db::deployments::mark_healthy_and_supersede(
            &self.pool,
            deployment_id,
            project_id,
            deployment_group,
        )
        .await
    }

    async fn mark_deployment_as_active(
        &self,
        deployment_id: Uuid,
        project_id: Uuid,
        deployment_group: &str,
    ) -> Result<()> {
        crate::db::deployments::mark_as_active(
            &self.pool,
            deployment_id,
            project_id,
            deployment_group,
        )
        .await
    }

    async fn set_identity_credential_hash(&self, id: Uuid, hash: &str) -> Result<()> {
        crate::db::deployments::set_identity_credential_hash(&self.pool, id, hash).await
    }
}
