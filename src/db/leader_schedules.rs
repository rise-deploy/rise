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
//! provision/cleanup/drift loops). Callers compose the two:
//!
//! ```ignore
//! loop {
//!     ticker.tick().await;
//!     if !election.is_leader() { continue; }
//!     if !schedule.try_claim().await? { continue; }
//!     do_work().await;
//! }
//! ```

use std::time::Duration;

use sqlx::PgPool;

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
    /// This method does not check leadership — callers must already be the
    /// leader for the corresponding `LeaderElection` before invoking it.
    /// Composing the two gates (`is_leader()` + `try_claim()`) is the
    /// expected pattern.
    pub async fn try_claim(&self) -> Result<bool, sqlx::Error> {
        let interval_secs = self.interval.as_secs_f64();
        // First run for an unseen schedule name inserts a row with
        // `last_run_at = NOW()` and the slot is granted. Subsequent runs go
        // through `ON CONFLICT DO UPDATE`, gated by the `WHERE` clause: if
        // the interval has not elapsed, no row is updated and `RETURNING`
        // yields nothing.
        let row: Option<(String,)> = sqlx::query_as(
            "INSERT INTO leader_schedules (name, last_run_at)
             VALUES ($1, NOW())
             ON CONFLICT (name) DO UPDATE
               SET last_run_at = NOW()
               WHERE leader_schedules.last_run_at + ($2 * INTERVAL '1 second') <= NOW()
             RETURNING name",
        )
        .bind(&self.name)
        .bind(interval_secs)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    /// Convenience wrapper around [`Self::try_claim`] that logs the outcome
    /// and returns `bool` directly — `true` if the caller should run the
    /// work now, `false` if it should skip this tick. Used by interval
    /// loops where the response is uniform across all three branches
    /// (claimed / not-yet-due / DB error → skip with appropriate log).
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
    async fn concurrent_callers_only_one_wins(pool: PgPool) -> sqlx::Result<()> {
        // Simulates a split-brain transient where two replicas both try to
        // claim the slot simultaneously. The atomic UPSERT must serialize
        // so exactly one wins.
        let s1 = GlobalSchedule::new(pool.clone(), "test-race", Duration::from_secs(60));
        let s2 = GlobalSchedule::new(pool, "test-race", Duration::from_secs(60));

        let (a, b) = tokio::join!(s1.try_claim(), s2.try_claim());
        let wins = [a?, b?].iter().filter(|w| **w).count();
        assert_eq!(wins, 1, "exactly one concurrent claimer must win");
        Ok(())
    }
}
