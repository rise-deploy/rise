//! Globally-coordinated cadence for interval-based work running under a
//! leader lease.
//!
//! `LeaderElection` guarantees mutual exclusion (only one replica is leader
//! at a time), but each replica still runs its own `tokio::time::interval`
//! anchored to its own start time. At a leadership transition the new
//! leader's ticker can fire far sooner than the previous leader's next
//! scheduled run, producing a transient burst — e.g. old leader sweeps at
//! t=70 and dies; new leader's ticker fires at t=73, so two sweeps happen
//! 3s apart when the configured interval is 10s.
//!
//! `GlobalSchedule::try_claim` closes that gap with a single atomic UPSERT
//! against `leader_schedules` keyed by schedule name. The next slot is
//! granted only when `last_run_at + interval <= NOW()`, so the new leader's
//! first run waits the full interval since the previous leader's last run.
//!
//! Decoupled from `LeaderElection` on purpose: a single controller may hold
//! one lease but run several schedules at different cadences (e.g. ECR's
//! provision/cleanup/drift loops). Callers compose the two via
//! [`GlobalSchedule::try_claim_or_skip_as_leader`], which verifies
//! leadership against the DB (fast-path: cached lease horizon) immediately
//! before the schedule UPSERT — closing the TOCTOU window where a
//! stale-cached `is_leader()` could let a non-leader advance `last_run_at`
//! and stall the true leader for a full interval:
//!
//! ```ignore
//! loop {
//!     ticker.tick().await;
//!     if !election.is_leader() { continue; }
//!     if !schedule.try_claim_or_skip_as_leader("my work", &election).await {
//!         continue;
//!     }
//!     do_work().await;
//! }
//! ```

use std::time::Duration;

use sqlx::PgPool;

use crate::db::leader_leases::{LeaderElection, LeaderStatus, LeaseError};

/// Per-task globally-coordinated cadence gate. Cheap to clone (`PgPool` is
/// reference-counted, `name` is owned). Construct one per loop.
#[derive(Clone, Debug)]
pub struct GlobalSchedule {
    pool: PgPool,
    name: String,
    interval: Duration,
}

impl GlobalSchedule {
    pub fn new(pool: PgPool, name: impl Into<String>, interval: Duration) -> Self {
        Self {
            pool,
            name: name.into(),
            interval,
        }
    }

    /// Try to claim the next run slot.
    ///
    /// Returns `Ok(true)` if at least `interval` has elapsed since the last
    /// globally-recorded run for this schedule name — `last_run_at` is
    /// updated to `NOW()` in the same statement, fencing other callers
    /// (across replicas) from claiming the same slot.
    ///
    /// Returns `Ok(false)` if the interval has not yet elapsed. The caller
    /// should skip this tick and try again on the next one.
    ///
    /// This method does not check leadership. When composing with a
    /// `LeaderElection`, prefer [`Self::try_claim_or_skip_as_leader`] over
    /// gating on `is_leader()` alone — the atomic flag can be briefly stale
    /// during a leadership handover, letting a non-leader's claim succeed,
    /// advance `last_run_at`, and stall the true leader for a full interval.
    pub async fn try_claim(&self) -> Result<bool, sqlx::Error> {
        let interval_secs = self.interval.as_secs_f64();
        // First run for an unseen schedule name inserts a row with
        // `last_run_at = NOW()` and the slot is granted. Subsequent runs go
        // through `ON CONFLICT DO UPDATE`, gated by the `WHERE` clause: if
        // the interval has not elapsed, no row is updated and `RETURNING`
        // yields nothing.
        let row = sqlx::query_scalar!(
            "INSERT INTO leader_schedules (name, last_run_at)
             VALUES ($1, NOW())
             ON CONFLICT (name) DO UPDATE
               SET last_run_at = NOW()
               WHERE leader_schedules.last_run_at + ($2 * INTERVAL '1 second') <= NOW()
             RETURNING name",
            &self.name,
            interval_secs,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    /// Convenience wrapper around [`Self::try_claim`] that logs the outcome
    /// and returns `bool` directly — `true` if the caller should run the
    /// work now, `false` if it should skip this tick. Used by interval
    /// loops where the response is uniform across all three branches
    /// (claimed / not-yet-due / DB error → skip with appropriate log).
    ///
    /// Does not check leadership. Prefer
    /// [`Self::try_claim_or_skip_as_leader`] in loops gated by a
    /// `LeaderElection` — it closes the TOCTOU window where a stale-cached
    /// `is_leader()` could let a non-leader advance `last_run_at` and stall
    /// the true leader for a full interval.
    pub async fn try_claim_or_skip(&self, context: &str) -> bool {
        match self.try_claim().await {
            Ok(true) => true,
            Ok(false) => {
                tracing::debug!(
                    schedule = %self.name,
                    "{context} slot not yet due globally; skipping tick"
                );
                false
            }
            Err(e) => {
                tracing::warn!(
                    schedule = %self.name,
                    error = ?e,
                    "{context} schedule claim failed; skipping tick"
                );
                false
            }
        }
    }

    /// Like [`Self::try_claim_or_skip`], but first verifies leadership
    /// against the DB via [`LeaderElection::ensure_leader_for`] with a zero
    /// validity window. On the steady-state fast path (cached lease horizon
    /// still in the future) this costs no DB round-trip — only when the
    /// horizon has decayed does it fall back to a `verify_leader` query.
    ///
    /// Required when composing with `LeaderElection`: gating only on
    /// `is_leader()` (atomic flag, can be briefly stale during a leadership
    /// handover) lets a non-leader's claim succeed, advancing
    /// `last_run_at` and delaying the true leader's next run by up to one
    /// full interval. `ensure_leader_for` reads the horizon published only
    /// after a DB-confirmed acquire/renew, so it cannot return `Leader`
    /// for a holder that has actually lost the lease.
    pub async fn try_claim_or_skip_as_leader(
        &self,
        context: &str,
        election: &LeaderElection,
    ) -> bool {
        match election.ensure_leader_for(Duration::ZERO).await {
            Ok(LeaderStatus::Leader) => {}
            Ok(LeaderStatus::NotLeader) => {
                tracing::debug!(
                    schedule = %self.name,
                    "{context} leadership lost before claim; skipping tick"
                );
                return false;
            }
            Err(LeaseError::Db(e)) => {
                tracing::warn!(
                    schedule = %self.name,
                    error = ?e,
                    "{context} leadership verification failed; skipping tick"
                );
                return false;
            }
            Err(e @ LeaseError::InvalidMinValidity { .. }) => {
                // Unreachable: Duration::ZERO is always within the half-lease
                // bound. Treat as programmer error and skip defensively.
                tracing::error!(
                    schedule = %self.name,
                    error = ?e,
                    "{context} ensure_leader_for misconfigured; skipping tick"
                );
                return false;
            }
        }
        self.try_claim_or_skip(context).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test]
    async fn first_call_claims_slot(pool: PgPool) -> sqlx::Result<()> {
        let s = GlobalSchedule::new(pool, "test-first", Duration::from_secs(10));
        assert!(s.try_claim().await?, "first call must claim the slot");
        Ok(())
    }

    #[sqlx::test]
    async fn second_call_within_interval_is_denied(pool: PgPool) -> sqlx::Result<()> {
        let s = GlobalSchedule::new(pool, "test-deny", Duration::from_secs(60));
        assert!(s.try_claim().await?);
        assert!(
            !s.try_claim().await?,
            "second call inside the interval must be denied"
        );
        Ok(())
    }

    #[sqlx::test]
    async fn call_after_interval_elapses_is_granted(pool: PgPool) -> sqlx::Result<()> {
        // Shortest interval that's still robust under CI clock jitter: 1s.
        let s = GlobalSchedule::new(pool, "test-elapse", Duration::from_secs(1));
        assert!(s.try_claim().await?);
        assert!(!s.try_claim().await?, "still inside the interval");
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(
            s.try_claim().await?,
            "call after interval elapses must be granted"
        );
        Ok(())
    }

    #[sqlx::test]
    async fn different_names_are_independent(pool: PgPool) -> sqlx::Result<()> {
        let a = GlobalSchedule::new(pool.clone(), "test-a", Duration::from_secs(60));
        let b = GlobalSchedule::new(pool, "test-b", Duration::from_secs(60));
        assert!(a.try_claim().await?);
        assert!(
            b.try_claim().await?,
            "claiming one schedule must not affect another"
        );
        assert!(!a.try_claim().await?);
        assert!(!b.try_claim().await?);
        Ok(())
    }

    #[sqlx::test]
    async fn as_leader_claims_when_holder(pool: PgPool) -> sqlx::Result<()> {
        use uuid::Uuid;

        let holder = LeaderElection::spawn(
            pool.clone(),
            "rise-test-as-leader-claim",
            Uuid::new_v4(),
            Duration::from_millis(1500),
        );
        // Wait for the background task to win the election.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !holder.is_leader() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(holder.is_leader(), "holder should win election");

        let s = GlobalSchedule::new(pool, "test-as-leader-claim", Duration::from_secs(60));
        assert!(
            s.try_claim_or_skip_as_leader("test", &holder).await,
            "leader's first claim must succeed"
        );
        assert!(
            !s.try_claim_or_skip_as_leader("test", &holder).await,
            "second claim inside the interval must be denied"
        );
        Ok(())
    }

    #[sqlx::test]
    async fn as_leader_skips_when_not_holder_and_leaves_schedule_untouched(
        pool: PgPool,
    ) -> sqlx::Result<()> {
        use uuid::Uuid;

        let holder = LeaderElection::spawn(
            pool.clone(),
            "rise-test-as-leader-skip",
            Uuid::new_v4(),
            Duration::from_millis(1500),
        );
        let non_holder = LeaderElection::spawn(
            pool.clone(),
            "rise-test-as-leader-skip",
            Uuid::new_v4(),
            Duration::from_millis(1500),
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !holder.is_leader() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(holder.is_leader(), "holder should win election");
        assert!(!non_holder.is_leader(), "non-holder should not be leader");

        let s = GlobalSchedule::new(pool, "test-as-leader-skip", Duration::from_secs(60));
        assert!(
            !s.try_claim_or_skip_as_leader("test", &non_holder).await,
            "non-holder must not claim"
        );
        // Critical: the non-holder's failed attempt must not have written the
        // schedule row. The true holder's next claim should still succeed.
        assert!(
            s.try_claim_or_skip_as_leader("test", &holder).await,
            "holder's claim must succeed after non-holder was skipped"
        );
        Ok(())
    }

    #[sqlx::test]
    async fn concurrent_callers_only_one_wins(pool: PgPool) -> sqlx::Result<()> {
        // Simulates a split-brain transient where N replicas all try to
        // claim the slot simultaneously. Each claimer runs on its own task
        // and waits on a barrier so they all hit the DB at ~the same instant;
        // the atomic UPSERT must serialize so exactly one wins.
        use std::sync::Arc;
        use tokio::sync::Barrier;

        const N: usize = 8;
        let barrier = Arc::new(Barrier::new(N));
        let mut handles = Vec::with_capacity(N);
        for _ in 0..N {
            let s = GlobalSchedule::new(pool.clone(), "test-race", Duration::from_secs(60));
            let barrier = barrier.clone();
            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                s.try_claim().await
            }));
        }

        let mut wins = 0;
        for h in handles {
            if h.await.expect("task panicked")? {
                wins += 1;
            }
        }
        assert_eq!(wins, 1, "exactly one concurrent claimer must win");
        Ok(())
    }
}
