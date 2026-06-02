use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use thiserror::Error;
use tokio::task::JoinHandle;
use uuid::Uuid;

const MIN_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(100);

/// Production lease duration. All in-process controllers acquire `LeaderElection`
/// with this duration; tests may pass shorter values directly to `spawn`.
/// [`LeaderElection::ensure_leader_for`] caps `min_validity` at `lease_duration / 2`,
/// so production callers get a 30s ceiling.
pub const LEASE_DURATION: Duration = Duration::from_secs(60);

/// Process-uptime anchor for the monotonic clock we publish into
/// `lease_valid_until_ms`. Using `Instant`-relative ms (rather than
/// `SystemTime`) keeps the horizon comparison immune to NTP-driven clock
/// jumps — a backward jump in wall time would otherwise make us trust a
/// stale horizon past its real expiry.
static PROCESS_START: LazyLock<Instant> = LazyLock::new(Instant::now);

fn process_uptime_ms() -> u64 {
    PROCESS_START.elapsed().as_millis() as u64
}

/// Outcome of a DB-backed leadership check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaderStatus {
    /// We are the current lease holder.
    Leader,
    /// Another holder (or no holder) owns the lease. Stop irreversible work.
    NotLeader,
}

/// Error returned by [`LeaderElection::ensure_leader_for`].
#[derive(Debug, Error)]
pub enum LeaseError {
    /// `min_validity` exceeds the lease duration — the fast path could
    /// never short-circuit and the caller is almost certainly misconfigured.
    /// This is a programmer error, not a runtime condition.
    #[error(
        "ensure_leader_for: requested min_validity {requested:?} exceeds maximum {max:?} (lease_duration={lease_duration:?})"
    )]
    InvalidMinValidity {
        requested: Duration,
        max: Duration,
        lease_duration: Duration,
    },
    /// Transport error during fallback DB verification.
    #[error("ensure_leader_for: DB verification failed: {0}")]
    Db(#[from] sqlx::Error),
}

/// Safety margin subtracted from the published lease horizon to absorb the
/// round-trip between when Postgres computed `NOW()` and when we received the
/// response. Conservative — local Postgres round-trips are typically <10ms.
const HORIZON_NETWORK_MARGIN: Duration = Duration::from_secs(1);

struct TaskGuard {
    task: JoinHandle<()>,
    is_leader: Arc<AtomicBool>,
    lease_valid_until_ms: Arc<AtomicU64>,
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        self.task.abort();
        self.is_leader.store(false, Ordering::Release);
        self.lease_valid_until_ms.store(0, Ordering::Release);
    }
}

/// Persistent leader election service backed by the `leader_leases` PostgreSQL table.
///
/// Holds the lease for its entire lifetime via a single background heartbeat task.
/// Clone-safe: all clones share the same task and `is_leader` state; the task is
/// aborted when the last clone is dropped.
#[derive(Clone)]
pub struct LeaderElection {
    is_leader: Arc<AtomicBool>,
    /// Monotonic process-uptime (ms) until which the lease is known to be valid.
    /// Published by the background heartbeat after every successful acquire/renew
    /// (minus [`HORIZON_NETWORK_MARGIN`]). `0` means "horizon unknown" — used to
    /// gate the fast path in [`LeaderElection::ensure_leader_for`].
    lease_valid_until_ms: Arc<AtomicU64>,
    pool: PgPool,
    name: String,
    holder_id: Uuid,
    lease_duration: Duration,
    _task: Arc<TaskGuard>,
}

impl LeaderElection {
    /// Spawn the background lease manager. Returns immediately; the first
    /// acquisition attempt happens asynchronously. `is_leader()` starts `false`
    /// and becomes `true` once the background task wins the election.
    ///
    /// Production callers should pass [`LEASE_DURATION`]; tests may pass
    /// shorter values. `ensure_leader_for` enforces `min_validity <= lease_duration / 2`.
    pub fn spawn(pool: PgPool, name: &str, holder_id: Uuid, lease_duration: Duration) -> Self {
        let is_leader = Arc::new(AtomicBool::new(false));
        let lease_valid_until_ms = Arc::new(AtomicU64::new(0));

        let is_leader_bg = Arc::clone(&is_leader);
        let lease_valid_until_bg = Arc::clone(&lease_valid_until_ms);
        let pool_bg = pool.clone();
        let name_bg = name.to_string();

        let is_leader_guard = Arc::clone(&is_leader);
        let lease_valid_until_guard = Arc::clone(&lease_valid_until_ms);

        let task = tokio::spawn(async move {
            run_election_loop(
                pool_bg,
                name_bg,
                holder_id,
                lease_duration,
                is_leader_bg,
                lease_valid_until_bg,
            )
            .await;
        });

        Self {
            is_leader,
            lease_valid_until_ms,
            pool,
            name: name.to_string(),
            holder_id,
            lease_duration,
            _task: Arc::new(TaskGuard {
                task,
                is_leader: is_leader_guard,
                lease_valid_until_ms: lease_valid_until_guard,
            }),
        }
    }

    /// Returns whether this instance currently holds the leader lease.
    /// O(1), no DB round-trip — safe to call in tight loops or before external API calls.
    pub fn is_leader(&self) -> bool {
        self.is_leader.load(Ordering::Acquire)
    }

    /// Verifies leadership with a DB round-trip.
    /// Call immediately before irreversible DB mutations (deletes, finalizer removals, status updates).
    pub async fn assert_leader(&self) -> Result<()> {
        if is_held_db(&self.pool, &self.name, self.holder_id).await? {
            Ok(())
        } else {
            Err(anyhow!(
                "leader lease '{}' is no longer held by {}",
                self.name,
                self.holder_id
            ))
        }
    }

    /// Verifies leadership against the DB without conflating transient
    /// transport errors with lease loss. Use this when the caller needs to
    /// distinguish "we lost the lease" from "the DB is briefly unreachable".
    ///
    /// On Err(_) the caller should treat the result as inconclusive —
    /// typically: log and retry, or fall back to the cached `is_leader()`
    /// atomic for a conservative decision.
    ///
    /// Prefer [`Self::ensure_leader_for`] in hot loops: it short-circuits to
    /// a local-clock check when the cached lease horizon already guarantees
    /// the requested window, eliminating the DB round-trip on the fast path.
    pub async fn verify_leader(&self) -> Result<LeaderStatus, sqlx::Error> {
        if is_held_db_raw(&self.pool, &self.name, self.holder_id).await? {
            Ok(LeaderStatus::Leader)
        } else {
            Ok(LeaderStatus::NotLeader)
        }
    }

    /// Verifies leadership will hold for at least `min_validity`. Returns
    /// instantly with no DB round-trip when the cached lease horizon already
    /// covers the requested window; otherwise falls back to a [`Self::verify_leader`]
    /// round-trip.
    ///
    /// The fast path is sound because the heartbeat publishes
    /// `lease_valid_until_ms` only after a successful DB-confirmed
    /// acquire/renew (minus a small network-margin haircut). While that
    /// horizon is in the future plus `min_validity`, no peer can have taken
    /// the lease — the underlying `try_acquire` query refuses takeover until
    /// `expires_at < NOW()`.
    ///
    /// `min_validity` must not exceed `lease_duration / 2`, the worst-case
    /// horizon between two successful heartbeats (heartbeat fires every
    /// `lease_duration / 3`, so a healthy horizon oscillates between
    /// `2/3 × lease_duration` and `lease_duration`). For production callers
    /// using [`LEASE_DURATION`] this ceiling is [`MAX_MIN_VALIDITY`] (30s).
    pub async fn ensure_leader_for(
        &self,
        min_validity: Duration,
    ) -> Result<LeaderStatus, LeaseError> {
        let max = self.lease_duration / 2;
        if min_validity > max {
            return Err(LeaseError::InvalidMinValidity {
                requested: min_validity,
                max,
                lease_duration: self.lease_duration,
            });
        }

        let horizon = self.lease_valid_until_ms.load(Ordering::Acquire);
        if horizon > 0 {
            let now = process_uptime_ms();
            let min_validity_ms = min_validity.as_millis() as u64;
            if horizon >= now.saturating_add(min_validity_ms) {
                return Ok(LeaderStatus::Leader);
            }
        }

        Ok(self.verify_leader().await?)
    }

    /// Best-effort lease release for graceful shutdown.
    ///
    /// Must be called BEFORE the `LeaderElection` is dropped (`Drop` only
    /// aborts the heartbeat, it does not release the lease — the row will
    /// otherwise linger until `expires_at`, blocking peers from acquiring
    /// for up to the full lease TTL).
    ///
    /// Idempotent: safe to call multiple times. Returns `Ok(())` even if
    /// the row is already gone or owned by another holder. The
    /// `WHERE holder_id = $2` guard ensures we only delete OUR row, not a
    /// row a peer has since acquired (concurrency safe).
    pub async fn release(&self) -> Result<(), sqlx::Error> {
        sqlx::query!(
            "DELETE FROM runtime_sync.leader_leases WHERE name = $1 AND holder_id = $2",
            self.name,
            self.holder_id,
        )
        .execute(&self.pool)
        .await?;
        self.is_leader.store(false, Ordering::Release);
        self.lease_valid_until_ms.store(0, Ordering::Release);
        Ok(())
    }
}

async fn run_election_loop(
    pool: PgPool,
    name: String,
    holder_id: Uuid,
    lease_duration: Duration,
    is_leader: Arc<AtomicBool>,
    lease_valid_until_ms: Arc<AtomicU64>,
) {
    let retry_interval = heartbeat_interval(lease_duration);
    loop {
        match try_acquire(&pool, &name, holder_id, lease_duration).await {
            Ok(true) => {
                publish_horizon(&lease_valid_until_ms, lease_duration);
                is_leader.store(true, Ordering::Release);
                tracing::debug!(lease = %name, "leader lease acquired");
                heartbeat_loop(
                    &pool,
                    &name,
                    holder_id,
                    lease_duration,
                    &lease_valid_until_ms,
                )
                .await;
                is_leader.store(false, Ordering::Release);
                lease_valid_until_ms.store(0, Ordering::Release);
                tracing::debug!(lease = %name, "leader lease lost; will retry");
            }
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(
                    lease = %name,
                    holder_id = %holder_id,
                    ?error,
                    "leader lease acquisition failed"
                );
            }
        }
        tokio::time::sleep(retry_interval).await;
    }
}

async fn heartbeat_loop(
    pool: &PgPool,
    name: &str,
    holder_id: Uuid,
    lease_duration: Duration,
    lease_valid_until_ms: &Arc<AtomicU64>,
) {
    let mut ticker = tokio::time::interval(heartbeat_interval(lease_duration));
    loop {
        ticker.tick().await;
        match renew(pool, name, holder_id, lease_duration).await {
            Ok(RenewOutcome::Renewed) => {
                publish_horizon(lease_valid_until_ms, lease_duration);
            }
            Ok(RenewOutcome::Expired { expired_at, now }) => {
                // Our row is still ours but `expires_at` slipped into the
                // past — typically because the runtime was paused (laptop
                // sleep, long blocking task) for longer than the lease TTL.
                // The next acquire attempt will re-take the row.
                tracing::warn!(
                    lease = %name,
                    %expired_at,
                    lag_ms = (now - expired_at).num_milliseconds(),
                    "leader lease expired before renewal (likely runtime stall); stepping down"
                );
                break;
            }
            Ok(RenewOutcome::TakenByPeer { peer_id }) => {
                tracing::warn!(
                    lease = %name,
                    %peer_id,
                    "leader lease taken by peer; stepping down"
                );
                break;
            }
            Ok(RenewOutcome::RowMissing) => {
                tracing::warn!(
                    lease = %name,
                    "leader lease row missing (released externally?); stepping down"
                );
                break;
            }
            Err(error) => {
                // Deliberately do NOT update the horizon on transient errors:
                // letting it decay naturally is exactly what makes
                // `ensure_leader_for` fall back to a DB verify once the
                // pre-error horizon runs out.
                tracing::warn!(
                    lease = %name,
                    holder_id = %holder_id,
                    ?error,
                    "leader lease heartbeat failed; will retry next tick"
                );
            }
        }
    }
}

/// Publish the lease horizon: process-uptime (ms) of the moment the lease is
/// guaranteed safe until, subtracting [`HORIZON_NETWORK_MARGIN`] to absorb the
/// round-trip between Postgres setting `expires_at` and us receiving the
/// response. Saturating arithmetic keeps very short test leases sound.
fn publish_horizon(lease_valid_until_ms: &AtomicU64, lease_duration: Duration) {
    let now = process_uptime_ms();
    let validity_ms = lease_duration
        .saturating_sub(HORIZON_NETWORK_MARGIN)
        .as_millis() as u64;
    lease_valid_until_ms.store(now.saturating_add(validity_ms), Ordering::Release);
}

fn heartbeat_interval(lease_duration: Duration) -> Duration {
    Duration::from_secs_f64(
        (lease_duration.as_secs_f64() / 3.0).max(MIN_HEARTBEAT_INTERVAL.as_secs_f64()),
    )
}

async fn try_acquire(
    pool: &PgPool,
    name: &str,
    holder_id: Uuid,
    lease_duration: Duration,
) -> Result<bool> {
    let lease_secs = lease_duration.as_secs_f64();
    let result = sqlx::query_scalar!(
        "INSERT INTO runtime_sync.leader_leases (name, holder_id, heartbeat_at, expires_at)
         VALUES ($1, $2, NOW(), NOW() + ($3 * INTERVAL '1 second'))
         ON CONFLICT (name) DO UPDATE
           SET holder_id = $2, heartbeat_at = NOW(), expires_at = NOW() + ($3 * INTERVAL '1 second')
           WHERE runtime_sync.leader_leases.expires_at < NOW()
              OR runtime_sync.leader_leases.holder_id = $2
         RETURNING holder_id",
        name,
        holder_id,
        lease_secs,
    )
    .fetch_optional(pool)
    .await?;
    Ok(result == Some(holder_id))
}

/// Why a renewal attempt finished the way it did. The non-`Renewed` variants
/// all cause the heartbeat loop to step down, but they describe different
/// underlying events — see the per-variant docs.
enum RenewOutcome {
    /// The UPDATE matched our row and bumped `expires_at`.
    Renewed,
    /// Our row is still ours, but `expires_at` was already in the past when
    /// we tried to renew. Almost always a paused runtime (laptop sleep, long
    /// blocking task) rather than a peer — no one else has acted yet.
    Expired {
        expired_at: DateTime<Utc>,
        now: DateTime<Utc>,
    },
    /// The row exists but `holder_id` is now someone else's — a peer
    /// genuinely took over.
    TakenByPeer { peer_id: Uuid },
    /// The row no longer exists. Either we called `release()` or someone
    /// removed it out-of-band.
    RowMissing,
}

async fn renew(
    pool: &PgPool,
    name: &str,
    holder_id: Uuid,
    lease_duration: Duration,
) -> Result<RenewOutcome> {
    let lease_secs = lease_duration.as_secs_f64();
    let updated = sqlx::query_scalar!(
        "UPDATE runtime_sync.leader_leases
         SET heartbeat_at = NOW(), expires_at = NOW() + ($3 * INTERVAL '1 second')
         WHERE name = $1 AND holder_id = $2 AND expires_at > NOW()
         RETURNING holder_id",
        name,
        holder_id,
        lease_secs,
    )
    .fetch_optional(pool)
    .await?;
    if updated == Some(holder_id) {
        return Ok(RenewOutcome::Renewed);
    }

    // Renewal failed — one extra round-trip to determine why so the log line
    // is accurate. This path is rare (only on stepdown), so the cost is fine.
    let current = sqlx::query!(
        r#"SELECT holder_id, expires_at, NOW() AS "now!" FROM runtime_sync.leader_leases WHERE name = $1"#,
        name,
    )
    .fetch_optional(pool)
    .await?;
    let Some(row) = current else {
        return Ok(RenewOutcome::RowMissing);
    };
    if row.holder_id != holder_id {
        return Ok(RenewOutcome::TakenByPeer {
            peer_id: row.holder_id,
        });
    }
    Ok(RenewOutcome::Expired {
        expired_at: row.expires_at,
        now: row.now,
    })
}

async fn is_held_db_raw(pool: &PgPool, name: &str, holder_id: Uuid) -> Result<bool, sqlx::Error> {
    let row = sqlx::query_scalar!(
        "SELECT holder_id FROM runtime_sync.leader_leases WHERE name = $1 AND expires_at > NOW()",
        name,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row == Some(holder_id))
}

async fn is_held_db(pool: &PgPool, name: &str, holder_id: Uuid) -> Result<bool> {
    Ok(is_held_db_raw(pool, name, holder_id).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generous wait for the background task to complete its first DB round-trip
    /// in cases where we expect it *not* to acquire (negative assertions).
    /// 100ms was too tight under CI load (all 300+ tests share the DB).
    const ACQUIRE_WAIT: Duration = Duration::from_millis(500);

    /// Maximum time to wait for an elector to become leader. Polls in small steps so
    /// the happy path returns nearly instantly while CI jitter doesn't trip the test.
    const LEADER_TIMEOUT: Duration = Duration::from_secs(5);

    async fn wait_for_leader(election: &LeaderElection, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            if election.is_leader() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        election.is_leader()
    }

    #[sqlx::test]
    async fn is_leader_starts_false_then_becomes_true(pool: PgPool) -> Result<()> {
        let election = LeaderElection::spawn(
            pool.clone(),
            "rise-test-lease",
            Uuid::new_v4(),
            Duration::from_millis(1500),
        );
        assert!(
            !election.is_leader(),
            "should be false before first acquisition"
        );
        assert!(
            wait_for_leader(&election, LEADER_TIMEOUT).await,
            "should be true after background task acquires"
        );
        Ok(())
    }

    #[sqlx::test]
    async fn second_cannot_acquire_while_first_holds(pool: PgPool) -> Result<()> {
        let first = LeaderElection::spawn(
            pool.clone(),
            "rise-test-lease",
            Uuid::new_v4(),
            Duration::from_millis(1500),
        );
        assert!(wait_for_leader(&first, LEADER_TIMEOUT).await);

        let second = LeaderElection::spawn(
            pool.clone(),
            "rise-test-lease",
            Uuid::new_v4(),
            Duration::from_millis(1500),
        );
        tokio::time::sleep(ACQUIRE_WAIT).await;
        assert!(
            !second.is_leader(),
            "second should not acquire while first holds"
        );
        Ok(())
    }

    #[sqlx::test]
    async fn second_acquires_after_first_drops(pool: PgPool) -> Result<()> {
        let first = LeaderElection::spawn(
            pool.clone(),
            "rise-test-lease",
            Uuid::new_v4(),
            Duration::from_millis(1500),
        );
        assert!(wait_for_leader(&first, LEADER_TIMEOUT).await);

        drop(first); // aborts heartbeat; the existing row's expires_at decays.

        // The last heartbeat may fire just before drop, pushing expires_at to
        // ~NOW + lease_duration. Wait 2 * lease_duration to leave generous headroom
        // for the row to be considered expired before `second` tries to take over.
        tokio::time::sleep(Duration::from_millis(3000)).await;

        let second = LeaderElection::spawn(
            pool.clone(),
            "rise-test-lease",
            Uuid::new_v4(),
            Duration::from_millis(1500),
        );
        assert!(
            wait_for_leader(&second, LEADER_TIMEOUT).await,
            "second should acquire after first's lease expires"
        );
        Ok(())
    }

    #[sqlx::test]
    async fn assert_leader_passes_for_holder_fails_for_non_holder(pool: PgPool) -> Result<()> {
        let holder = LeaderElection::spawn(
            pool.clone(),
            "rise-test-lease",
            Uuid::new_v4(),
            Duration::from_millis(1500),
        );
        let non_holder = LeaderElection::spawn(
            pool.clone(),
            "rise-test-lease",
            Uuid::new_v4(),
            Duration::from_millis(1500),
        );
        assert!(wait_for_leader(&holder, LEADER_TIMEOUT).await);
        assert!(!non_holder.is_leader());

        holder.assert_leader().await?;
        assert!(
            non_holder.assert_leader().await.is_err(),
            "assert_leader should fail for non-holder"
        );
        Ok(())
    }

    #[sqlx::test]
    async fn verify_leader_returns_leader_for_holder(pool: PgPool) -> Result<()> {
        let holder = LeaderElection::spawn(
            pool.clone(),
            "rise-test-lease",
            Uuid::new_v4(),
            Duration::from_millis(1500),
        );
        assert!(wait_for_leader(&holder, LEADER_TIMEOUT).await);

        let status = holder.verify_leader().await?;
        assert_eq!(status, LeaderStatus::Leader);
        Ok(())
    }

    #[sqlx::test]
    async fn verify_leader_returns_not_leader_for_non_holder(pool: PgPool) -> Result<()> {
        let holder = LeaderElection::spawn(
            pool.clone(),
            "rise-test-lease",
            Uuid::new_v4(),
            Duration::from_millis(1500),
        );
        let non_holder = LeaderElection::spawn(
            pool.clone(),
            "rise-test-lease",
            Uuid::new_v4(),
            Duration::from_millis(1500),
        );
        assert!(wait_for_leader(&holder, LEADER_TIMEOUT).await);
        assert!(!non_holder.is_leader());

        let status = non_holder.verify_leader().await?;
        assert_eq!(status, LeaderStatus::NotLeader);
        Ok(())
    }

    #[sqlx::test]
    async fn release_lets_peer_acquire_immediately(pool: PgPool) -> Result<()> {
        let first = LeaderElection::spawn(
            pool.clone(),
            "rise-test-lease",
            Uuid::new_v4(),
            // Long lease TTL so we can prove the peer acquires via release(),
            // not via expiry.
            Duration::from_secs(60),
        );
        assert!(wait_for_leader(&first, LEADER_TIMEOUT).await);

        first.release().await?;

        let second = LeaderElection::spawn(
            pool.clone(),
            "rise-test-lease",
            Uuid::new_v4(),
            Duration::from_secs(60),
        );
        assert!(
            wait_for_leader(&second, Duration::from_secs(1)).await,
            "second should acquire immediately after release(), \
             not wait for the 60s lease TTL"
        );
        Ok(())
    }

    #[sqlx::test]
    async fn lease_is_maintained_beyond_initial_ttl(pool: PgPool) -> Result<()> {
        let election = LeaderElection::spawn(
            pool.clone(),
            "rise-test-lease",
            Uuid::new_v4(),
            Duration::from_millis(1500),
        );
        assert!(wait_for_leader(&election, LEADER_TIMEOUT).await);

        // Wait longer than the lease TTL — heartbeat should keep it alive
        tokio::time::sleep(Duration::from_millis(2000)).await;
        assert!(
            election.is_leader(),
            "heartbeat should have renewed the lease"
        );
        election.assert_leader().await?;
        Ok(())
    }

    #[sqlx::test]
    async fn ensure_leader_for_fast_path_after_acquire(pool: PgPool) -> Result<()> {
        // Long lease so the horizon comfortably exceeds the requested validity.
        let election = LeaderElection::spawn(
            pool.clone(),
            "rise-test-lease",
            Uuid::new_v4(),
            Duration::from_secs(60),
        );
        assert!(wait_for_leader(&election, LEADER_TIMEOUT).await);

        // Drop the DB pool so any fallback to `verify_leader` would error. If
        // `ensure_leader_for` returns `Ok(Leader)` here, the fast path
        // succeeded — proving it took no DB round-trip.
        pool.close().await;

        let status = election
            .ensure_leader_for(Duration::from_secs(5))
            .await
            .expect("fast path should not hit the DB");
        assert_eq!(status, LeaderStatus::Leader);
        Ok(())
    }

    #[sqlx::test]
    async fn ensure_leader_for_rejects_min_validity_above_half_lease(pool: PgPool) -> Result<()> {
        let lease_duration = Duration::from_secs(10);
        let election = LeaderElection::spawn(
            pool.clone(),
            "rise-test-lease",
            Uuid::new_v4(),
            lease_duration,
        );
        assert!(wait_for_leader(&election, LEADER_TIMEOUT).await);

        // lease_duration / 2 = 5s, so 6s exceeds the cap.
        let err = election
            .ensure_leader_for(Duration::from_secs(6))
            .await
            .expect_err("min_validity > lease_duration/2 must be rejected");
        match err {
            LeaseError::InvalidMinValidity {
                requested,
                max,
                lease_duration: ld,
            } => {
                assert_eq!(requested, Duration::from_secs(6));
                assert_eq!(max, Duration::from_secs(5));
                assert_eq!(ld, lease_duration);
            }
            other => panic!("expected InvalidMinValidity, got {other:?}"),
        }
        Ok(())
    }

    #[sqlx::test]
    async fn ensure_leader_for_falls_back_when_horizon_is_zero(pool: PgPool) -> Result<()> {
        // A non-holder never publishes a horizon, so any positive
        // min_validity must fall through to `verify_leader` — which here
        // resolves to NotLeader (we never acquired).
        let holder = LeaderElection::spawn(
            pool.clone(),
            "rise-test-lease",
            Uuid::new_v4(),
            Duration::from_millis(1500),
        );
        let non_holder = LeaderElection::spawn(
            pool.clone(),
            "rise-test-lease",
            Uuid::new_v4(),
            Duration::from_millis(1500),
        );
        assert!(wait_for_leader(&holder, LEADER_TIMEOUT).await);
        assert!(!non_holder.is_leader());

        let status = non_holder
            .ensure_leader_for(Duration::from_millis(100))
            .await?;
        assert_eq!(status, LeaderStatus::NotLeader);
        Ok(())
    }

    #[sqlx::test]
    async fn release_clears_lease_horizon(pool: PgPool) -> Result<()> {
        let election = LeaderElection::spawn(
            pool.clone(),
            "rise-test-lease",
            Uuid::new_v4(),
            Duration::from_secs(60),
        );
        assert!(wait_for_leader(&election, LEADER_TIMEOUT).await);
        assert!(
            election.lease_valid_until_ms.load(Ordering::Acquire) > 0,
            "horizon should be published after acquire"
        );

        election.release().await?;
        assert_eq!(
            election.lease_valid_until_ms.load(Ordering::Acquire),
            0,
            "release() must clear the horizon so a stale value can't be trusted"
        );
        Ok(())
    }
}
