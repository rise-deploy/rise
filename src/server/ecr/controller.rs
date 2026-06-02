use std::sync::Arc;
use std::time::Duration;
use tokio::time::interval;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::db::projects as db_projects;
use crate::server::ecr::{EcrRepoManager, ECR_FINALIZER};
use crate::server::state::ControllerState;
use rise_runtime_sync::{GlobalSchedule, LeaderElection};

/// ECR Controller manages ECR repository lifecycle
///
/// Responsibilities:
/// 1. **Provision loop**: Creates ECR repos for projects that don't have them yet
/// 2. **Cleanup loop**: Handles ECR repo deletion/orphaning when projects are deleted
/// 3. **Drift detection loop**: Detects and fixes missing ECR repositories
///
/// The controller uses the finalizer pattern to coordinate with project deletion:
/// - When a repo is created, the finalizer is added to the project
/// - When the project is marked for deletion, cleanup runs
/// - Only after cleanup completes is the finalizer removed
/// - The project controller waits for all finalizers to be removed before deleting
pub struct EcrController {
    state: Arc<ControllerState>,
    manager: Arc<EcrRepoManager>,
    election: LeaderElection,
    provision_interval: Duration,
    cleanup_interval: Duration,
    drift_interval: Duration,
    provision_schedule: GlobalSchedule,
    cleanup_schedule: GlobalSchedule,
    drift_schedule: GlobalSchedule,
}

impl EcrController {
    /// Create a new ECR controller
    pub fn new(state: Arc<ControllerState>, manager: Arc<EcrRepoManager>) -> Self {
        let provision_interval = Duration::from_secs(10);
        let cleanup_interval = Duration::from_secs(5);
        let drift_interval = Duration::from_secs(60);
        let election = LeaderElection::spawn(
            state.db_pool.clone(),
            "rise-ecr-controller",
            Uuid::new_v4(),
            Duration::from_secs(60),
        );
        // One schedule per loop: the three loops share a lease but have
        // independent cadences. The 60s drift schedule is the most
        // bursting-sensitive — short cadences (5s/10s) make transition
        // bursts harder to notice but the gate is still cheap.
        let provision_schedule = GlobalSchedule::new(
            state.db_pool.clone(),
            "rise-ecr-provision",
            provision_interval,
        );
        let cleanup_schedule =
            GlobalSchedule::new(state.db_pool.clone(), "rise-ecr-cleanup", cleanup_interval);
        let drift_schedule =
            GlobalSchedule::new(state.db_pool.clone(), "rise-ecr-drift", drift_interval);
        Self {
            state,
            manager,
            election,
            provision_interval,
            cleanup_interval,
            drift_interval,
            provision_schedule,
            cleanup_schedule,
            drift_schedule,
        }
    }

    /// Start provision, cleanup, and drift detection loops
    pub fn start(self: Arc<Self>) {
        let provision_self = Arc::clone(&self);
        tokio::spawn(async move {
            provision_self.provision_loop().await;
        });

        let cleanup_self = Arc::clone(&self);
        tokio::spawn(async move {
            cleanup_self.cleanup_loop().await;
        });

        let drift_self = Arc::clone(&self);
        tokio::spawn(async move {
            drift_self.drift_detection_loop().await;
        });
    }

    /// Provision loop - creates ECR repos for active projects
    ///
    /// Runs every 10 seconds and:
    /// 1. Lists all active projects (not Deleting/Terminated)
    /// 2. For each project without the ECR finalizer, creates the repo
    /// 3. Adds the ECR finalizer to track that cleanup is needed
    async fn provision_loop(&self) {
        info!("ECR provision loop started");
        let mut ticker = interval(self.provision_interval);

        loop {
            ticker.tick().await;

            if !self.election.is_leader() {
                continue;
            }

            if !self
                .provision_schedule
                .try_claim_or_skip_as_leader("ECR provision", &self.election)
                .await
            {
                continue;
            }

            if let Err(e) = self.provision_repositories().await {
                error!("Error in ECR provision loop: {}", e);
            }
        }
    }

    /// Process provisioning for all active projects
    async fn provision_repositories(&self) -> anyhow::Result<()> {
        // Get all active projects
        let projects = db_projects::list_active(&self.state.db_pool).await?;

        for project in projects {
            // Skip if project already has ECR finalizer (repo already managed)
            if project.finalizers.contains(&ECR_FINALIZER.to_string()) {
                continue;
            }

            debug!("Provisioning ECR repository for project: {}", project.name);

            // Try to create the repository
            match self.manager.create_repository(&project.name).await {
                Ok(created) => {
                    if created {
                        info!("Created ECR repository for project: {}", project.name);
                    } else {
                        debug!(
                            "ECR repository already exists for project: {}",
                            project.name
                        );
                    }

                    // Add finalizer to indicate ECR cleanup is needed on deletion
                    self.election.assert_leader().await?;
                    db_projects::add_finalizer(&self.state.db_pool, project.id, ECR_FINALIZER)
                        .await?;
                    debug!("Added ECR finalizer to project: {}", project.name);
                }
                Err(e) => {
                    warn!(
                        "Failed to create ECR repository for project {}: {}",
                        project.name, e
                    );
                    // Continue to next project, will retry on next loop
                }
            }
        }

        Ok(())
    }

    /// Cleanup loop - handles ECR repo cleanup for deleted projects
    ///
    /// Runs every 5 seconds and:
    /// 1. Finds projects in Deleting status with ECR finalizer
    /// 2. Deletes or tags the ECR repo based on auto_remove setting
    /// 3. Removes the ECR finalizer so project can be fully deleted
    async fn cleanup_loop(&self) {
        info!("ECR cleanup loop started");
        let mut ticker = interval(self.cleanup_interval);

        loop {
            ticker.tick().await;

            if !self.election.is_leader() {
                continue;
            }

            if !self
                .cleanup_schedule
                .try_claim_or_skip_as_leader("ECR cleanup", &self.election)
                .await
            {
                continue;
            }

            if let Err(e) = self.cleanup_repositories().await {
                error!("Error in ECR cleanup loop: {}", e);
            }
        }
    }

    /// Process cleanup for all deleting projects with ECR finalizer
    async fn cleanup_repositories(&self) -> anyhow::Result<()> {
        // Find projects marked for deletion that still have ECR finalizer
        let projects =
            db_projects::find_deleting_with_finalizer(&self.state.db_pool, ECR_FINALIZER, 10)
                .await?;

        for project in projects {
            debug!("Cleaning up ECR repository for project: {}", project.name);

            // Perform cleanup based on auto_remove setting
            let cleanup_result = if self.manager.auto_remove() {
                // Delete the repository
                match self.manager.delete_repository(&project.name).await {
                    Ok(deleted) => {
                        if deleted {
                            info!("Deleted ECR repository for project: {}", project.name);
                        } else {
                            info!(
                                "ECR repository did not exist for project: {} (already deleted)",
                                project.name
                            );
                        }
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            } else {
                // Tag as orphaned instead of deleting
                match self.manager.tag_as_orphaned(&project.name).await {
                    Ok(tagged) => {
                        if tagged {
                            info!(
                                "Tagged ECR repository as orphaned for project: {}",
                                project.name
                            );
                        } else {
                            info!(
                                "ECR repository did not exist for project: {} (already deleted)",
                                project.name
                            );
                        }
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            };

            match cleanup_result {
                Ok(()) => {
                    // Remove finalizer so project can be deleted
                    self.election.assert_leader().await?;
                    db_projects::remove_finalizer(&self.state.db_pool, project.id, ECR_FINALIZER)
                        .await?;
                    info!(
                        "Removed ECR finalizer from project: {}, cleanup complete",
                        project.name
                    );
                }
                Err(e) => {
                    warn!(
                        "Failed to cleanup ECR repository for project {}: {}",
                        project.name, e
                    );
                    // Continue to next project, will retry on next loop
                }
            }
        }

        Ok(())
    }

    /// Drift detection loop - checks for missing ECR repositories
    ///
    /// Runs every 60 seconds and:
    /// 1. Lists all active projects WITH the ECR finalizer
    /// 2. Checks if the ECR repository actually exists
    /// 3. If missing, removes finalizer so provision loop can recreate it
    async fn drift_detection_loop(&self) {
        info!("ECR drift detection loop started");
        let mut ticker = interval(self.drift_interval);

        loop {
            ticker.tick().await;

            if !self.election.is_leader() {
                continue;
            }

            if !self
                .drift_schedule
                .try_claim_or_skip_as_leader("ECR drift", &self.election)
                .await
            {
                continue;
            }

            if let Err(e) = self.detect_repository_drift().await {
                error!("Error in ECR drift detection loop: {}", e);
            }
        }
    }

    /// Detect and fix ECR repository drift
    async fn detect_repository_drift(&self) -> anyhow::Result<()> {
        // Get all active projects
        let projects = db_projects::list_active(&self.state.db_pool).await?;

        for project in projects {
            // Only check projects that have the ECR finalizer
            if !project.finalizers.contains(&ECR_FINALIZER.to_string()) {
                continue;
            }

            // Check if repository actually exists
            match self.manager.repository_exists(&project.name).await {
                Ok(exists) => {
                    if !exists {
                        warn!(
                            "ECR repository drift detected for project {}: repository missing but finalizer present",
                            project.name
                        );

                        // Remove finalizer so provision loop will recreate the repository
                        self.election.assert_leader().await?;
                        db_projects::remove_finalizer(
                            &self.state.db_pool,
                            project.id,
                            ECR_FINALIZER,
                        )
                        .await?;

                        info!(
                            "Removed ECR finalizer from project {} to trigger repository recreation",
                            project.name
                        );
                    }
                }
                Err(e) => {
                    warn!(
                        "Failed to check ECR repository existence for project {}: {}",
                        project.name, e
                    );
                    // Continue to next project
                }
            }
        }

        Ok(())
    }
}
