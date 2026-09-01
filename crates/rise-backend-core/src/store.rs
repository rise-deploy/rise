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

/// Result of [`DeploymentStore::mark_deployment_healthy_and_supersede`].
#[derive(Debug, Clone)]
pub struct SupersessionOutcome {
    /// Whether the deployment was actually marked healthy. `false` means the
    /// atomic write was rejected (the deployment had already moved to a
    /// protected status) and nothing in the group was touched.
    pub became_healthy: bool,
    /// The previously-active deployment in the group, now `Terminating`, if
    /// there was one.
    pub superseded: Option<Deployment>,
}

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

    /// Look up a project by its (unique) name.
    async fn find_project_by_name(&self, name: &str) -> Result<Option<Project>>;

    /// List active (non-terminated) projects — used by the CRD backfill.
    async fn list_active_projects(&self) -> Result<Vec<Project>>;

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
    ///
    /// Guarded by the store's own transition validation: returns `None`
    /// (no-op) rather than overwriting a status this write must not touch —
    /// e.g. a deployment a concurrent request already moved on from.
    /// Callers on a routine/opportunistic path should treat `None` as a
    /// benign race and skip any follow-up (no error, no status recompute);
    /// callers driven by an explicit user request should surface it as
    /// "modified concurrently, please retry".
    async fn mark_deployment_failed(
        &self,
        id: Uuid,
        error_message: &str,
    ) -> Result<Option<Deployment>>;

    /// Transition a deployment into the cancelling state. See
    /// `mark_deployment_failed` for the `None` contract.
    async fn mark_deployment_cancelling(&self, id: Uuid) -> Result<Option<Deployment>>;

    /// Mark a deployment cancelled. See `mark_deployment_failed` for the
    /// `None` contract.
    async fn mark_deployment_cancelled(&self, id: Uuid) -> Result<Option<Deployment>>;

    /// Mark a deployment stopped. See `mark_deployment_failed` for the
    /// `None` contract.
    async fn mark_deployment_stopped(&self, id: Uuid) -> Result<Option<Deployment>>;

    /// Mark a deployment superseded. See `mark_deployment_failed` for the
    /// `None` contract.
    async fn mark_deployment_superseded(&self, id: Uuid) -> Result<Option<Deployment>>;

    /// Mark a deployment expired. See `mark_deployment_failed` for the
    /// `None` contract.
    async fn mark_deployment_expired(&self, id: Uuid) -> Result<Option<Deployment>>;

    /// Mark a deployment healthy. See `mark_deployment_failed` for the
    /// `None` contract.
    async fn mark_deployment_healthy(&self, id: Uuid) -> Result<Option<Deployment>>;

    /// Mark a deployment unhealthy with a reason. See `mark_deployment_failed`
    /// for the `None` contract.
    async fn mark_deployment_unhealthy(
        &self,
        id: Uuid,
        reason: String,
    ) -> Result<Option<Deployment>>;

    // --- container observations ---

    /// Every replica of a deployment as the backend last saw it, for the
    /// derivation to compare against.
    async fn list_container_observations(
        &self,
        deployment_id: Uuid,
    ) -> Result<Vec<crate::observation::ContainerObservation>>;

    /// Replace the recorded observations and append the events they imply, in
    /// one transaction — the events say what changed, the observations become
    /// the baseline the next tick compares against, and a crash between the two
    /// would either lose the events or write them twice.
    ///
    /// `observations` is the complete current set: replicas absent from it are
    /// forgotten, which is how a scale-down or a replaced task leaves the
    /// baseline as well as the timeline.
    async fn record_container_observations(
        &self,
        deployment_id: Uuid,
        source: crate::events::EventSource,
        observations: &[crate::observation::ContainerObservation],
        events: &[crate::observation::DerivedEvent],
    ) -> Result<()>;

    /// Forward backend-originated events into the deployment's log, skipping any
    /// already recorded.
    ///
    /// Deduplicated on `dedupe_key`, because a backend re-reads the same window
    /// on every tick: ECS returns roughly the last hour of service messages,
    /// and a Kubernetes Event lives well beyond one sync. Re-reporting is the
    /// normal case, not an error.
    async fn forward_backend_events(
        &self,
        deployment_id: Uuid,
        source: crate::events::EventSource,
        events: &[crate::events::ForwardedEvent],
    ) -> Result<u64>;

    /// Mark a deployment terminating with a termination reason. See
    /// `mark_deployment_failed` for the `None` contract.
    ///
    /// `superseded_by` names the replacement deployment (by its
    /// `deployment_id`) and is only meaningful with
    /// [`TerminationReason::Superseded`]. It is taken here because this is the
    /// only point where the replacement is in scope.
    async fn mark_deployment_terminating(
        &self,
        id: Uuid,
        reason: TerminationReason,
        superseded_by: Option<&str>,
    ) -> Result<Option<Deployment>>;

    /// Atomically mark a deployment healthy and, if some other deployment is
    /// currently the group's active (`Healthy`) deployment, mark that one
    /// `Terminating(Superseded)` in the same database transaction — so a
    /// crash between the two writes can never leave both non-terminal.
    ///
    /// `became_healthy` is `false` (no row touched at all) when the
    /// deployment had already moved to a status this write must not
    /// overwrite (e.g. a concurrent stop request already moved it to
    /// `Terminating`) — the caller should stop there: no hook, no straggler
    /// cleanup, no activation.
    async fn mark_deployment_healthy_and_supersede(
        &self,
        deployment_id: Uuid,
        project_id: Uuid,
        deployment_group: &str,
    ) -> Result<SupersessionOutcome>;

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
