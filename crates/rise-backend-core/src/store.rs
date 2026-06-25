//! The `DeploymentStore` trait — the database boundary between the deployment
//! backends and Rise's persistence layer.
//!
//! The deployment controllers (Kubernetes, Docker) read and mutate deployment
//! state exclusively through this trait. `rise-deploy` provides the only
//! implementation (`PgDeploymentStore`) over SQLX. Defining the boundary as a
//! trait is the seam that lets the controllers move into their own crates
//! without depending on `rise-deploy`, and matches the out-of-process
//! controller direction in `ROADMAP.md` (Phase C).

use anyhow::Result;
use async_trait::async_trait;
use uuid::Uuid;

use crate::models::{
    CustomDomain, Deployment, DeploymentEnvVar, DeploymentStatus, Environment, Project,
    TerminationReason,
};

/// Persistence operations the deployment controllers depend on.
///
/// Method names and signatures mirror the `rise-deploy` `db` helpers they wrap,
/// so the implementation is a thin pass-through.
#[async_trait]
pub trait DeploymentStore: Send + Sync {
    // --- projects ---

    /// List projects, optionally filtered by owning user.
    async fn list_projects(&self, owner_user_id: Option<Uuid>) -> Result<Vec<Project>>;

    /// Look up a project by its primary key.
    async fn find_project(&self, id: Uuid) -> Result<Option<Project>>;

    /// Recompute and persist a project's status from its deployments.
    async fn update_project_calculated_status(&self, project_id: Uuid) -> Result<Project>;

    /// Resolve the Organization resource UID a project is linked to.
    async fn organization_uid_for_project(&self, project_id: Uuid) -> Result<Option<Uuid>>;

    // --- environments ---

    /// Look up an environment by its primary key.
    async fn find_environment(&self, id: Uuid) -> Result<Option<Environment>>;

    /// List all environments for a project.
    async fn list_environments_for_project(&self, project_id: Uuid) -> Result<Vec<Environment>>;

    // --- custom domains ---

    /// List the custom domains configured for a project.
    async fn list_project_custom_domains(&self, project_id: Uuid) -> Result<Vec<CustomDomain>>;

    // --- deployment env vars ---

    /// List the resolved environment variables for a deployment.
    async fn list_deployment_env_vars(&self, deployment_id: Uuid) -> Result<Vec<DeploymentEnvVar>>;

    // --- deployments: reads ---

    /// Look up a deployment by its primary key.
    async fn find_deployment(&self, id: Uuid) -> Result<Option<Deployment>>;

    /// List the non-terminal deployments of a project.
    async fn list_non_terminal_deployments_for_project(
        &self,
        project_id: Uuid,
    ) -> Result<Vec<Deployment>>;

    /// Find the active deployment for a project's deployment group, if any.
    async fn find_active_deployment_for_project_and_group(
        &self,
        project_id: Uuid,
        group: &str,
    ) -> Result<Option<Deployment>>;

    /// List the non-terminal deployments in a project's deployment group.
    async fn find_non_terminal_deployments_for_project_and_group(
        &self,
        project_id: Uuid,
        group: &str,
    ) -> Result<Vec<Deployment>>;

    // --- deployments: mutations ---

    /// Transition a deployment to a new status (validated by the store).
    async fn update_deployment_status(
        &self,
        id: Uuid,
        status: DeploymentStatus,
    ) -> Result<Deployment>;

    /// Replace a deployment's controller metadata blob.
    async fn update_deployment_controller_metadata(
        &self,
        id: Uuid,
        metadata: &serde_json::Value,
    ) -> Result<Deployment>;

    /// Mark a deployment failed with an error message.
    async fn mark_deployment_failed(&self, id: Uuid, error_message: &str) -> Result<Deployment>;

    /// Mark a deployment cancelled.
    async fn mark_deployment_cancelled(&self, id: Uuid) -> Result<Deployment>;

    /// Mark a deployment stopped.
    async fn mark_deployment_stopped(&self, id: Uuid) -> Result<Deployment>;

    /// Mark a deployment superseded.
    async fn mark_deployment_superseded(&self, id: Uuid) -> Result<Deployment>;

    /// Mark a deployment expired.
    async fn mark_deployment_expired(&self, id: Uuid) -> Result<Deployment>;

    /// Mark a deployment healthy.
    async fn mark_deployment_healthy(&self, id: Uuid) -> Result<Deployment>;

    /// Mark a deployment unhealthy with a reason.
    async fn mark_deployment_unhealthy(&self, id: Uuid, reason: String) -> Result<Deployment>;

    /// Mark a deployment terminating with a termination reason.
    async fn mark_deployment_terminating(
        &self,
        id: Uuid,
        reason: TerminationReason,
    ) -> Result<Deployment>;

    /// Mark a deployment as the single active deployment of its project group.
    async fn mark_deployment_as_active(
        &self,
        deployment_id: Uuid,
        project_id: Uuid,
        deployment_group: &str,
    ) -> Result<()>;

    /// Persist the hash of a deployment's workload-identity bootstrap credential
    /// once it has been delivered to a running container.
    async fn set_identity_credential_hash(&self, id: Uuid, hash: &str) -> Result<()>;
}
