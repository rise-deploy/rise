//! Periodic RiseProject resync so the sync webhook re-mints workload-identity
//! tokens before they expire.
//!
//! Metacontroller only calls the sync hook on resource changes (and an
//! unreliable periodic resync); a steady `RiseProject` is otherwise never
//! resynced, so the webhook's half-TTL re-mint (`prepare_identity_secret`) never
//! runs and a long-lived pod's pre-minted identity file token would expire. This
//! leader-gated loop bumps each `RiseProject`'s trigger annotation every ~2/3 of
//! the identity-token TTL, which Metacontroller observes as a parent change →
//! sync → re-mint of any stale token. The webhook only re-mints *stale* tokens,
//! so resyncing every project is self-limiting (a no-op sync for fresh or
//! non-identity projects).
//!
//! Kubernetes only: the Docker controller re-mints on its own reconcile loop.

use std::sync::Arc;
use std::time::Duration;

use rise_runtime_sync::{leader_controller, LeaderElection, LeaderStatus, LEASE_DURATION};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::server::deployment::crd;

/// Floor/ceiling on the resync cadence regardless of the configured TTL, so a
/// pathological TTL can't make the loop hammer the API or sleep for hours.
const MIN_INTERVAL_SECS: u64 = 5;
const MAX_INTERVAL_SECS: u64 = 1800;

/// Lease validity verified before each sweep's external (K8s) writes.
const SWEEP_MIN_VALIDITY: Duration = Duration::from_secs(5);

/// Resync cadence in seconds for a given token TTL: ~2/3 of the TTL, clamped, so
/// each token is re-minted with roughly a third of its lifetime left for
/// metacontroller sync + kubelet propagation.
fn resync_interval_secs(ttl_secs: u64) -> u64 {
    (ttl_secs * 2 / 3).clamp(MIN_INTERVAL_SECS, MAX_INTERVAL_SECS)
}

pub struct IdentityRefreshController {
    kube_client: kube::Client,
    db_pool: PgPool,
    identity_token_ttl_seconds: u64,
    /// Restricts the resync to this controller's own `RiseProject`s when set.
    controller_class: Option<String>,
}

impl IdentityRefreshController {
    pub fn new(
        kube_client: kube::Client,
        db_pool: PgPool,
        identity_token_ttl_seconds: u64,
        controller_class: Option<String>,
    ) -> Self {
        Self {
            kube_client,
            db_pool,
            identity_token_ttl_seconds,
            controller_class,
        }
    }

    /// Resync interval: ~2/3 of the token TTL (clamped), so each token is
    /// re-minted with roughly a third of its lifetime to spare for propagation.
    fn interval(&self) -> Duration {
        Duration::from_secs(resync_interval_secs(self.identity_token_ttl_seconds))
    }

    /// Run the resync loop under leader election until `shutdown` is cancelled.
    pub async fn run(self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let interval = self.interval();
        info!(
            interval_secs = interval.as_secs(),
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
                "rise-identity-refresh" every interval => me.resync_all(&election).await,
            },
        }
        .await
    }

    /// Trigger a resync on every managed `RiseProject`. The loop macro already
    /// gates this on leadership; re-verify with a short validity before the
    /// external K8s writes and skip the tick on a verify error (don't trust the
    /// cached flag) — a brief dual-leader would only cause harmless duplicate
    /// (idempotent) annotation bumps, but skipping keeps the contract uniform.
    async fn resync_all(&self, election: &LeaderElection) -> anyhow::Result<()> {
        match election.ensure_leader_for(SWEEP_MIN_VALIDITY).await {
            Ok(LeaderStatus::Leader) => {}
            Ok(LeaderStatus::NotLeader) => return Ok(()),
            Err(e) => {
                warn!(error = ?e, "leader verification failed; skipping identity resync tick");
                return Ok(());
            }
        }

        let names =
            match crd::list_rise_project_names(&self.kube_client, self.controller_class.as_deref())
                .await
            {
                Ok(names) => names,
                Err(e) => {
                    // Transient API error: log and retry on the next tick rather than
                    // aborting the controller.
                    warn!(error = ?e, "failed to list RiseProjects for identity resync");
                    return Ok(());
                }
            };

        debug!(
            count = names.len(),
            "Resyncing RiseProjects to refresh workload-identity tokens"
        );
        for name in names {
            // A per-project failure must not abort the sweep.
            if let Err(e) = crd::trigger_resync(&self.kube_client, &name).await {
                warn!(project = %name, error = ?e, "failed to trigger identity resync");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resync_interval_is_two_thirds_of_ttl_clamped() {
        // The CI-only short TTL and the production default both land at 2/3.
        assert_eq!(resync_interval_secs(60), 40);
        assert_eq!(resync_interval_secs(3600), MAX_INTERVAL_SECS); // 2400 -> clamped to 1800
                                                                   // Pathological tiny/zero TTLs clamp up to the floor (never 0 → busy loop).
        assert_eq!(resync_interval_secs(3), MIN_INTERVAL_SECS);
        assert_eq!(resync_interval_secs(0), MIN_INTERVAL_SECS);
    }
}
