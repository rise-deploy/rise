//! Per-deployment RiseProject resync so the sync webhook re-mints
//! workload-identity tokens before they expire.
//!
//! Metacontroller only calls the sync hook on resource changes (and an
//! unreliable periodic resync); a steady `RiseProject` is otherwise never
//! resynced, so the webhook's re-mint (`prepare_identity_secret`) never runs and
//! a long-lived pod's pre-minted identity file token would expire. The webhook
//! records each deployment's next re-mint time (mint + 2/3·TTL) in
//! `deployments.identity_token_refresh_due_at`; this leader-gated loop resyncs a
//! project only once one of its deployments is **due**, by bumping the
//! `RiseProject`'s trigger annotation (`crd::trigger_resync`). Metacontroller
//! observes the parent change → sync → re-mint.
//!
//! Resyncing only due, identity-bearing deployments — rather than every project
//! on a fixed timer — means projects are touched only when needed, and because
//! deployments are minted at different times the work is naturally staggered.
//!
//! Kubernetes only: the Docker controller re-mints on its own reconcile loop.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use rise_runtime_sync::{leader_controller, LeaderElection, LeaderStatus, LEASE_DURATION};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::db::deployments as db_deployments;
use crate::server::deployment::crd;

/// Floor/ceiling on the poll cadence regardless of the configured TTL, so a
/// pathological TTL can't make the loop hammer the DB or sleep for too long.
const MIN_INTERVAL_SECS: u64 = 5;
const MAX_POLL_INTERVAL_SECS: u64 = 300;

/// Lease validity verified before each sweep's external (K8s) writes.
const SWEEP_MIN_VALIDITY: Duration = Duration::from_secs(5);

/// How often to check for due deployments: ~1/6 of the TTL (clamped), small
/// enough that a deployment due at 2/3·TTL is picked up well before it expires.
fn poll_interval_secs(ttl_secs: u64) -> u64 {
    (ttl_secs / 6).clamp(MIN_INTERVAL_SECS, MAX_POLL_INTERVAL_SECS)
}

pub struct IdentityRefreshController {
    kube_client: kube::Client,
    db_pool: PgPool,
    identity_token_ttl_seconds: u64,
    /// Cadence, derived once from the TTL.
    poll_interval: Duration,
}

impl IdentityRefreshController {
    pub fn new(
        kube_client: kube::Client,
        db_pool: PgPool,
        identity_token_ttl_seconds: u64,
    ) -> Self {
        Self {
            poll_interval: Duration::from_secs(poll_interval_secs(identity_token_ttl_seconds)),
            kube_client,
            db_pool,
            identity_token_ttl_seconds,
        }
    }

    /// Run the resync loop under leader election until `shutdown` is cancelled.
    pub async fn run(self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let interval = self.poll_interval;
        info!(
            poll_interval_secs = interval.as_secs(),
            ttl_secs = self.identity_token_ttl_seconds,
            "Starting workload-identity refresh controller"
        );
        let pool = self.db_pool.clone();
        let me = Arc::new(self);
        leader_controller! {
            pool: pool,
            lease: "rise-identity-refresh",
            holder: Uuid::new_v4(),
            ttl: LEASE_DURATION,
            shutdown: shutdown,
            election: election,
            schedules: {
                "rise-identity-refresh" every interval => me.resync_due(&election).await,
            },
        }
        .await
    }

    /// Resync each project that has an identity-bearing deployment due for a
    /// re-mint. The loop macro already gates this on leadership; re-verify with a
    /// short validity before the external K8s writes and skip the tick on a
    /// verify error (don't trust the cached flag) — a brief dual-leader would only
    /// cause harmless duplicate (idempotent) annotation bumps.
    async fn resync_due(&self, election: &LeaderElection) -> anyhow::Result<()> {
        match election.ensure_leader_for(SWEEP_MIN_VALIDITY).await {
            Ok(LeaderStatus::Leader) => {}
            Ok(LeaderStatus::NotLeader) => return Ok(()),
            Err(e) => {
                warn!(error = ?e, "leader verification failed; skipping identity resync tick");
                return Ok(());
            }
        }

        let due = match db_deployments::list_due_identity_refresh(&self.db_pool).await {
            Ok(due) => due,
            Err(e) => {
                warn!(error = ?e, "failed to query deployments due for identity refresh");
                return Ok(());
            }
        };
        if due.is_empty() {
            return Ok(());
        }

        // Trigger one resync per *project* (the sync re-mints all of its
        // deployments), not per deployment.
        let mut by_project: HashMap<String, Vec<uuid::Uuid>> = HashMap::new();
        for (deployment_id, project) in due {
            by_project.entry(project).or_default().push(deployment_id);
        }
        debug!(
            projects = by_project.len(),
            "Resyncing RiseProjects for due workload-identity tokens"
        );

        let mut advance: Vec<uuid::Uuid> = Vec::new();
        let mut orphaned: Vec<uuid::Uuid> = Vec::new();
        for (project, ids) in by_project {
            match crd::trigger_resync_status(&self.kube_client, &project).await {
                Ok(crd::ResyncOutcome::Triggered) => advance.extend(ids),
                Ok(crd::ResyncOutcome::ProjectNotFound) => {
                    // No CR to refresh; stop re-picking these every tick.
                    warn!(project = %project, "RiseProject not found; clearing identity refresh schedule");
                    orphaned.extend(ids);
                }
                // Transient error: leave the schedule untouched so it retries.
                Err(e) => {
                    warn!(project = %project, error = ?e, "failed to trigger identity resync")
                }
            }
        }

        // Push due times out by one poll interval (computed now, after the
        // triggers) so a deployment isn't re-triggered before the webhook
        // re-mints and writes the real 2/3·TTL due time. Advance-only: never drag
        // a webhook-written later time earlier, so the two writers are
        // order-independent. A failed re-mint just retries next tick.
        if !advance.is_empty() {
            let not_before =
                Utc::now() + chrono::Duration::seconds(self.poll_interval.as_secs() as i64);
            if let Err(e) =
                db_deployments::bump_identity_refresh_due_at(&self.db_pool, &advance, not_before)
                    .await
            {
                warn!(error = ?e, "failed to advance identity refresh due times");
            }
        }
        if !orphaned.is_empty() {
            if let Err(e) =
                db_deployments::clear_identity_refresh_due_at(&self.db_pool, &orphaned).await
            {
                warn!(error = ?e, "failed to clear identity refresh schedule for missing projects");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_interval_is_a_sixth_of_ttl_clamped() {
        assert_eq!(poll_interval_secs(60), 10);
        assert_eq!(poll_interval_secs(3600), MAX_POLL_INTERVAL_SECS); // 600 -> clamped to 300
                                                                      // Pathological tiny/zero TTLs clamp up to the floor (never 0 → busy loop).
        assert_eq!(poll_interval_secs(6), MIN_INTERVAL_SECS);
        assert_eq!(poll_interval_secs(0), MIN_INTERVAL_SECS);
    }
}
