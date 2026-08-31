use chrono::Utc;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info};
use uuid::Uuid;

use crate::db::models::DeploymentStatus;
use crate::db::{
    deployments as db_deployments, extensions as db_extensions, projects as db_projects,
};
use crate::server::deployment::state_machine;
use crate::server::state::ControllerState;
use rise_runtime_sync::{leader_controller, LeaderElection};
use tokio_util::sync::CancellationToken;

/// Deployments deleted per retention pass. Bounded so one sweep cannot hold a
/// long transaction over a large backlog on the first run after enabling it.
const DEPLOYMENT_DELETE_BATCH: i64 = 500;

/// Project controller handles project lifecycle operations.
///
/// Runs two leader-gated, globally-scheduled passes (see [`Self::run`]):
/// - **Deletion** (every 5s): advances projects in `Deleting` status toward
///   removal — cancelling/terminating their deployments, then deleting the
///   project once all are terminal and no finalizers/extensions remain.
/// - **Cleanup** (hourly): drops expired OAuth transient-state rows.
pub struct ProjectController {
    state: Arc<ControllerState>,
}

impl ProjectController {
    /// Run the project controller under a leader election until `shutdown` is
    /// cancelled, then release the lease so a peer can take over promptly.
    pub async fn run(
        state: Arc<ControllerState>,
        shutdown: CancellationToken,
    ) -> anyhow::Result<()> {
        let pool = state.db_pool.clone();
        let controller = ProjectController { state };
        leader_controller! {
            pool: pool,
            lease: "rise-project-controller",
            holder: Uuid::new_v4(),
            ttl: Duration::from_secs(60),
            shutdown: shutdown,
            election: election,
            schedules: {
                // Deletion pass: process projects in `Deleting` status. Each
                // destructive write re-verifies leadership against the DB — see
                // `process_deleting_projects`.
                "rise-project-deletion" every Duration::from_secs(5)
                    => controller.process_deleting_projects(&election).await,
                // Hourly housekeeping. Using a `GlobalSchedule` keeps the
                // cadence globally coordinated across replicas and survives
                // leadership handovers (the cadence is anchored on the shared
                // `last_run_at`, not on any single replica's local state).
                "rise-project-cleanup" every Duration::from_secs(3600)
                    => controller.cleanup_expired_transient_state().await,
                // Deployment history retention. Hourly is far more often than
                // the bounds it enforces need: the event cap tolerates a flap
                // accumulating for an hour, and deployment deletion is measured
                // in days.
                "rise-deployment-retention" every Duration::from_secs(3600)
                    => controller.enforce_deployment_retention(&election).await,
            },
        }
        .await
    }

    /// Clean up expired OAuth transient state rows (leader-gated, hourly).
    async fn cleanup_expired_transient_state(&self) -> anyhow::Result<()> {
        let n = crate::db::oauth_transient_state::delete_expired(&self.state.db_pool).await?;
        if n > 0 {
            debug!("Cleaned up {} expired OAuth transient state rows", n);
        }
        Ok(())
    }

    /// Bound the growth of deployment history (leader-gated, hourly).
    ///
    /// The event cap always runs. Deleting whole deployments is opt-in, and
    /// re-verifies leadership immediately before the write: it is irreversible,
    /// so a replica that lost the lease mid-pass must not delete alongside the
    /// new leader.
    async fn enforce_deployment_retention(&self, election: &LeaderElection) -> anyhow::Result<()> {
        let settings = &self.state.deployment_retention;

        let trimmed = crate::db::retention::trim_deployment_events(
            &self.state.db_pool,
            settings.max_events_per_deployment,
        )
        .await?;
        if trimmed > 0 {
            info!(
                "Trimmed {} deployment event(s) beyond the per-deployment cap of {}",
                trimmed, settings.max_events_per_deployment
            );
        }

        if !settings.delete_aged_deployments {
            return Ok(());
        }

        let older_than =
            Utc::now() - chrono::Duration::days(i64::from(settings.max_deployment_age_days));

        // Bounded per pass so one sweep cannot hold a long transaction over a
        // large backlog; the next hour picks up where this left off.
        election.assert_leader().await?;
        let deleted = crate::db::retention::delete_aged_deployments(
            &self.state.db_pool,
            older_than,
            settings.keep_primary_deployments_per_environment,
            DEPLOYMENT_DELETE_BATCH,
        )
        .await?;
        if deleted > 0 {
            info!(
                "Deleted {} deployment(s) finished before {}",
                deleted, older_than
            );
        }

        Ok(())
    }

    /// Process all projects in Deleting status.
    ///
    /// Leadership is re-verified against the DB (`election.assert_leader()`)
    /// immediately before every destructive write, so a replica that lost the
    /// lease mid-pass cannot mutate alongside the new leader.
    async fn process_deleting_projects(&self, election: &LeaderElection) -> anyhow::Result<()> {
        let deleting = db_projects::find_deleting(&self.state.db_pool, 10).await?;

        for project in deleting {
            debug!("Processing deletion for project {}", project.name);

            // Find all deployments for this project
            let deployments =
                db_deployments::list_for_project(&self.state.db_pool, project.id).await?;

            // Check if any non-terminal deployments exist
            let mut has_non_terminal = false;

            for deployment in &deployments {
                if state_machine::is_terminal(&deployment.status) {
                    continue;
                }

                has_non_terminal = true;

                // Distinguish pre-infrastructure vs post-infrastructure
                let is_pre_infrastructure = matches!(
                    deployment.status,
                    DeploymentStatus::Pending
                        | DeploymentStatus::Building
                        | DeploymentStatus::Pushing
                );

                if is_pre_infrastructure {
                    // Cancel pre-infrastructure deployments
                    // These haven't provisioned resources yet
                    if deployment.status != DeploymentStatus::Cancelling {
                        election.assert_leader().await?;
                        info!(
                            "Cancelling pre-infrastructure deployment {} (status={:?})",
                            deployment.deployment_id, deployment.status
                        );
                        db_deployments::mark_cancelling(&self.state.db_pool, deployment.id).await?;
                    }
                } else {
                    // Terminate post-infrastructure deployments
                    // These have containers/resources that need cleanup
                    if deployment.status != DeploymentStatus::Terminating {
                        election.assert_leader().await?;
                        info!(
                            "Terminating post-infrastructure deployment {} (status={:?})",
                            deployment.deployment_id, deployment.status
                        );
                        db_deployments::mark_terminating(
                            &self.state.db_pool,
                            deployment.id,
                            crate::db::models::TerminationReason::UserStopped,
                            None,
                        )
                        .await?;
                    }
                }
            }

            // If all deployments are terminal, check finalizers and extensions before deleting
            if !has_non_terminal {
                // Check if any finalizers remain (e.g., ECR cleanup pending)
                if db_projects::has_finalizers(&self.state.db_pool, project.id).await? {
                    debug!(
                        "Project {} has finalizers remaining, waiting for cleanup controllers",
                        project.name
                    );
                    continue;
                }

                // Check if any extensions remain (including soft-deleted ones)
                // Extensions must be fully cleaned up by their controllers before project deletion
                let extensions =
                    db_extensions::list_by_project(&self.state.db_pool, project.id).await?;
                if !extensions.is_empty() {
                    debug!(
                        "Project {} has {} extension(s) remaining, waiting for extension controllers to clean up",
                        project.name,
                        extensions.len()
                    );
                    continue;
                }

                info!(
                    "All deployments for project {} are terminated and no finalizers or extensions remain, marking as Terminated",
                    project.name
                );

                // Transition to Terminated status before removal
                election.assert_leader().await?;
                db_projects::update_status(
                    &self.state.db_pool,
                    project.id,
                    crate::db::models::ProjectStatus::Terminated,
                )
                .await?;

                info!(
                    "Project {} is Terminated, deleting from database",
                    project.name
                );

                election.assert_leader().await?;
                db_projects::delete(&self.state.db_pool, project.id).await?;
            } else {
                debug!(
                    "Project {} still has non-terminal deployments, waiting",
                    project.name
                );
            }
        }

        Ok(())
    }
}
