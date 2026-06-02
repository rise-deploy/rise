use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::interval;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::db::models::DeploymentStatus;
use crate::db::{
    deployments as db_deployments, extensions as db_extensions, projects as db_projects,
};
use crate::server::deployment::state_machine;
use crate::server::state::ControllerState;
use rise_runtime_sync::{GlobalSchedule, LeaderElection};

/// Project controller handles project lifecycle operations
///
/// Currently implements:
/// - Deletion loop: processes projects in Deleting status
pub struct ProjectController {
    state: Arc<ControllerState>,
    deletion_interval: Duration,
    cleanup_tick: AtomicU64,
    election: LeaderElection,
    deletion_schedule: GlobalSchedule,
}

impl ProjectController {
    /// Create a new project controller
    pub fn new(state: Arc<ControllerState>) -> Self {
        let deletion_interval = Duration::from_secs(5);
        let election = LeaderElection::spawn(
            state.db_pool.clone(),
            "rise-project-controller",
            Uuid::new_v4(),
            Duration::from_secs(60),
        );
        let deletion_schedule = GlobalSchedule::new(
            state.db_pool.clone(),
            "rise-project-deletion",
            deletion_interval,
        );
        Self {
            state,
            deletion_interval,
            cleanup_tick: AtomicU64::new(1),
            election,
            deletion_schedule,
        }
    }

    /// Start deletion loop
    pub fn start(self: Arc<Self>) {
        tokio::spawn(async move {
            self.deletion_loop().await;
        });
    }

    /// Deletion loop - processes projects in Deleting status
    ///
    /// Runs every 5 seconds and:
    /// 1. Finds projects marked as Deleting
    /// 2. For each project, checks all deployments
    /// 3. Cancels pre-infrastructure deployments (Pending/Building/Pushing)
    /// 4. Terminates post-infrastructure deployments (Deploying/Healthy/Unhealthy)
    /// 5. Once all deployments are terminal, deletes the project
    async fn deletion_loop(&self) {
        info!("Project deletion loop started");
        let mut ticker = interval(self.deletion_interval);

        loop {
            ticker.tick().await;

            if !self.election.is_leader() {
                continue;
            }

            // Global cadence gate: ensures the new leader's first deletion
            // pass after a transition waits the full interval since the
            // previous leader's last pass. `_as_leader` re-verifies
            // leadership against the DB (fast-path on the cached lease
            // horizon) before the schedule UPSERT — see `GlobalSchedule` docs.
            if !self
                .deletion_schedule
                .try_claim_or_skip_as_leader("project deletion", &self.election)
                .await
            {
                continue;
            }

            if let Err(e) = self.process_deleting_projects().await {
                error!("Error in deletion loop: {}", e);
            }

            if let Err(e) = self.cleanup_expired_transient_state().await {
                warn!("Error cleaning up expired transient state: {:?}", e);
            }
        }
    }

    /// Periodically clean up expired OAuth transient state rows (runs on leader only)
    async fn cleanup_expired_transient_state(&self) -> anyhow::Result<()> {
        // Run cleanup roughly once per hour (every 720 ticks × 5s = 3600s)
        let tick = self.cleanup_tick.fetch_add(1, Ordering::Relaxed);
        if tick.is_multiple_of(720) {
            let n = crate::db::oauth_transient_state::delete_expired(&self.state.db_pool).await?;
            if n > 0 {
                debug!("Cleaned up {} expired OAuth transient state rows", n);
            }
        }
        Ok(())
    }

    /// Process all projects in Deleting status
    async fn process_deleting_projects(&self) -> anyhow::Result<()> {
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
                        self.election.assert_leader().await?;
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
                        self.election.assert_leader().await?;
                        info!(
                            "Terminating post-infrastructure deployment {} (status={:?})",
                            deployment.deployment_id, deployment.status
                        );
                        db_deployments::mark_terminating(
                            &self.state.db_pool,
                            deployment.id,
                            crate::db::models::TerminationReason::UserStopped,
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
                self.election.assert_leader().await?;
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

                self.election.assert_leader().await?;
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
