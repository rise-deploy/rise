use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::db::projects as db_projects;
use crate::server::ecr::{EcrRepoManager, ECR_FINALIZER};
use crate::server::state::ControllerState;
use rise_runtime_sync::{leader_controller, LeaderElection};
use tokio_util::sync::CancellationToken;

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
}

impl EcrController {
    /// Run the ECR controller's provision/cleanup/drift loops under a single
    /// leader election until `shutdown` is cancelled, releasing the lease on exit.
    ///
    /// The three loops share one lease but have independent cadences (the
    /// `leader_schedules` rows gate each one's global cadence across replicas).
    pub async fn run(
        state: Arc<ControllerState>,
        manager: Arc<EcrRepoManager>,
        shutdown: CancellationToken,
    ) -> anyhow::Result<()> {
        let controller = Arc::new(EcrController { state, manager });
        let pool = controller.state.db_pool.clone();
        leader_controller! {
            pool: pool,
            lease: "rise-ecr-controller",
            holder: Uuid::new_v4(),
            ttl: Duration::from_secs(60),
            shutdown: shutdown,
            election: election,
            schedules: {
                provision { name: "rise-ecr-provision", interval: Duration::from_secs(10) }
                    => controller.provision_repositories(&election).await,
                cleanup { name: "rise-ecr-cleanup", interval: Duration::from_secs(5) }
                    => controller.cleanup_repositories(&election).await,
                drift { name: "rise-ecr-drift", interval: Duration::from_secs(60) }
                    => controller.detect_repository_drift(&election).await,
            },
        }
        .await
    }

    /// Process provisioning for all active projects
    async fn provision_repositories(&self, election: &LeaderElection) -> anyhow::Result<()> {
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
                    election.assert_leader().await?;
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

    /// Process cleanup for all deleting projects with ECR finalizer
    async fn cleanup_repositories(&self, election: &LeaderElection) -> anyhow::Result<()> {
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
                    election.assert_leader().await?;
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

    /// Detect and fix ECR repository drift
    async fn detect_repository_drift(&self, election: &LeaderElection) -> anyhow::Result<()> {
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
                        election.assert_leader().await?;
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
