//! RiseProject resync so the sync webhook re-mints workload-identity tokens
//! before they expire.
//!
//! Metacontroller only calls the sync hook on resource changes (and an
//! unreliable periodic resync); a steady `RiseProject` is otherwise never
//! resynced, so the webhook's re-mint (`prepare_identity_secret`) never runs and
//! a long-lived pod's pre-minted identity file token would expire. The webhook
//! records each project's earliest next re-mint time (mint + 2/3·TTL) in the
//! `RiseProject`'s `status.identityRefreshDueAt`; this leader-gated loop resyncs
//! a project only once that time is **due**, by bumping the `RiseProject`'s
//! trigger annotation (`crd::trigger_resync_status`). Metacontroller observes the
//! parent change → sync → re-mint, which also writes a fresh (future) due time
//! back into the status, so the project isn't re-triggered next tick.
//!
//! Resyncing only due, identity-bearing projects — rather than every project on a
//! fixed timer — means projects are touched only when needed, and because
//! deployments are minted at different times projects come due at different times.
//!
//! Kubernetes only: the Docker controller re-mints on its own reconcile loop.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use kube::api::{Api, ListParams};
use kube::ResourceExt;
use rise_runtime_sync::{leader_controller, LeaderElection, LeaderStatus, LEASE_DURATION};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::server::deployment::crd::{self, RiseProject};

/// Floor/ceiling on the poll cadence regardless of the configured TTL, so a
/// pathological TTL can't make the loop hammer the K8s API or sleep for too long.
const MIN_INTERVAL_SECS: u64 = 5;
const MAX_POLL_INTERVAL_SECS: u64 = 300;

/// Lease validity verified before each sweep's external (K8s) writes.
const SWEEP_MIN_VALIDITY: Duration = Duration::from_secs(5);

/// Worst-case delay for the kubelet to project an updated Secret into a running
/// pod's mounted volume: the kubelet sync period (default 60s) plus the
/// configmap/secret cache propagation delay (watch delay, or up to the cache TTL
/// — default 60s). If the re-mint margin (TTL − 2/3·TTL = TTL/3) is below this, a
/// refreshed token may not reach the pod before the old one expires. See the
/// Kubernetes docs on automatic Secret-volume updates.
const KUBELET_SECRET_PROPAGATION_CEILING_SECS: u64 = 120;

/// How often to check for due deployments: ~1/6 of the TTL (clamped), small
/// enough that a deployment due at 2/3·TTL is picked up well before it expires.
fn poll_interval_secs(ttl_secs: u64) -> u64 {
    (ttl_secs / 6).clamp(MIN_INTERVAL_SECS, MAX_POLL_INTERVAL_SECS)
}

pub struct IdentityRefreshController {
    kube_client: kube::Client,
    /// Connection pool — used solely for the leader-election lease, which needs a
    /// concrete `PgPool`. The due schedule is read from `RiseProject` status, not
    /// the database.
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

        // A short TTL can leave less re-mint margin than the kubelet takes to
        // project a refreshed Secret into the pod — warn the operator once.
        let margin = self.identity_token_ttl_seconds.saturating_sub(
            crate::server::workload_tokens::refresh_due_after_secs(self.identity_token_ttl_seconds),
        );
        if margin < KUBELET_SECRET_PROPAGATION_CEILING_SECS {
            warn!(
                ttl_secs = self.identity_token_ttl_seconds,
                refresh_margin_secs = margin,
                propagation_ceiling_secs = KUBELET_SECRET_PROPAGATION_CEILING_SECS,
                "identity_token_ttl_seconds is short: the workload-identity re-mint \
                 margin may be smaller than the kubelet Secret-volume propagation \
                 delay, so a refreshed token can expire in the pod before the new one \
                 is projected; consider raising identity_token_ttl_seconds"
            );
        }

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

    /// Resync each project whose `RiseProject` status reports a due
    /// workload-identity re-mint. The loop macro already gates this on
    /// leadership; re-verify with a short validity before the external K8s writes
    /// and skip the tick on a verify error (don't trust the cached flag) — a
    /// brief dual-leader would only cause harmless duplicate (idempotent)
    /// annotation bumps.
    async fn resync_due(&self, election: &LeaderElection) -> anyhow::Result<()> {
        match election.ensure_leader_for(SWEEP_MIN_VALIDITY).await {
            Ok(LeaderStatus::Leader) => {}
            Ok(LeaderStatus::NotLeader) => return Ok(()),
            Err(e) => {
                warn!(error = ?e, "leader verification failed; skipping identity resync tick");
                return Ok(());
            }
        }

        // List every RiseProject and read its status-reported due time. A project
        // with no identity-bearing deployment carries no `identityRefreshDueAt`
        // and is skipped; a vanished project simply isn't in the list.
        let api: Api<RiseProject> = Api::all(self.kube_client.clone());
        let projects = match api.list(&ListParams::default()).await {
            Ok(list) => list,
            Err(e) => {
                warn!(error = ?e, "failed to list RiseProjects for identity refresh");
                return Ok(());
            }
        };

        let now = Utc::now();
        let due_projects: Vec<String> = projects
            .items
            .iter()
            .filter(|rp| {
                rp.status
                    .as_ref()
                    .and_then(|s| s.identity_refresh_due_at.as_deref())
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .is_some_and(|due| due.with_timezone(&Utc) <= now)
            })
            .map(|rp| rp.name_any())
            .collect();

        if due_projects.is_empty() {
            return Ok(());
        }
        debug!(
            projects = due_projects.len(),
            "Resyncing RiseProjects for due workload-identity tokens"
        );

        // Triggering a resync makes the webhook re-mint and write a fresh (future)
        // due time back into the status, so a project isn't re-triggered on the
        // next tick. A transient trigger failure just retries next tick.
        for project in due_projects {
            match crd::trigger_resync_status(&self.kube_client, &project).await {
                Ok(crd::ResyncOutcome::Triggered) => {}
                Ok(crd::ResyncOutcome::ProjectNotFound) => {
                    debug!(project = %project, "RiseProject vanished before identity resync");
                }
                Err(e) => {
                    warn!(project = %project, error = ?e, "failed to trigger identity resync")
                }
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
