//! Safe, scope-based API over the raw synchronization primitives.
//!
//! Prefer these over driving [`GlobalLock`](crate::GlobalLock) /
//! [`LeaderElection`](crate::LeaderElection) by hand: they tie the lock/lease
//! lifetime to a closure (or, for [`leader_controller!`](crate::leader_controller),
//! a controller loop), so the resource is released on every exit path without a
//! manual call — and without relying on `Drop`, which can't `await`.

use std::future::Future;
use std::time::Duration;

use anyhow::Result;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tracing::warn;
use uuid::Uuid;

use crate::global_lock::GlobalLock;
use crate::leader_leases::LeaderElection;

/// Run `f` while holding the install-wide lock `name`, releasing it afterwards.
///
/// Acquires (blocking), awaits `f`, then [`release`](GlobalLock::release)s. The
/// lock is freed on every exit path: a normal return and an early `?` inside
/// `f` both go through `release()`; a panic unwinds through [`GlobalLock`]'s
/// `Drop`, which closes the connection to free the lock. `f`'s result is always
/// returned as-is — a release error can't leak the lock (the connection is
/// closed on that path), so it's logged rather than propagated.
pub async fn with_global_lock<T, Fut>(
    pool: &PgPool,
    name: &str,
    f: impl FnOnce() -> Fut,
) -> Result<T>
where
    Fut: Future<Output = Result<T>>,
{
    let lock = GlobalLock::acquire(pool, name).await?;
    let result = f().await;
    if let Err(release_err) = lock.release().await {
        warn!(
            lock = %name,
            error = ?release_err,
            "GlobalLock release reported an error (lock freed via connection close)"
        );
    }
    result
}

/// Run `f` for the duration of a leader election, releasing the lease when `f`
/// returns or `shutdown` is cancelled.
///
/// Spawns a [`LeaderElection`] for `name`, passes a handle to `f` (typically a
/// controller run-loop), and — once `f` returns — deletes the lease row so a
/// peer can take over immediately instead of waiting out the TTL.
///
/// Release is **structural**: `f` is raced against `shutdown.cancelled()`, so
/// even if `f` never observes the token (or hangs mid-work), cancelling it
/// drops `f` and the lease is still released. `f` should still observe
/// `shutdown` itself for a clean exit at a safe point (see
/// [`leader_controller!`](crate::leader_controller)); the race is the
/// can't-forget safety net.
///
/// A panic in `f` does not delete the lease here, but it still can't leak — the
/// heartbeat stops and the lease lapses within its TTL. A release error is
/// likewise non-fatal (TTL reclaims it), so it's logged, never propagated.
pub async fn with_leader_election<Fut>(
    pool: PgPool,
    name: &str,
    holder_id: Uuid,
    lease_duration: Duration,
    shutdown: CancellationToken,
    f: impl FnOnce(LeaderElection) -> Fut,
) -> Result<()>
where
    Fut: Future<Output = Result<()>>,
{
    let election = LeaderElection::spawn(pool, name, holder_id, lease_duration);
    let result = tokio::select! {
        r = f(election.clone()) => r,
        _ = shutdown.cancelled() => Ok(()),
    };
    if let Err(e) = election.release().await {
        warn!(
            lease = %name,
            error = ?e,
            "leader lease release failed on scope exit; lease TTL will reclaim it"
        );
    }
    result
}

/// Run a leader-elected controller loop over one or more globally-scheduled
/// work items, releasing the lease when `shutdown` is cancelled.
///
/// Expands to a [`with_leader_election`] call whose body is a `tokio::select!`
/// loop. Each schedule gets its own ticker plus a [`GlobalSchedule`](crate::GlobalSchedule);
/// on each tick the work runs only if this replica is the leader *and* the
/// global schedule grants the slot (which also fences peers across a leadership
/// handover). The work body must evaluate to a `Result` — its `Err` is logged
/// and the loop continues. Cancelling `shutdown` (a
/// `tokio_util::sync::CancellationToken`) breaks the loop, so the lease is
/// released deterministically.
///
/// Each schedule is a string `name` (the `leader_schedules` row key and log
/// label), the keyword `every`, an `interval`, and a `=> work` body. The
/// `election:` binding names the [`LeaderElection`] handle so work bodies can
/// reference it (e.g. to `assert_leader()` before a destructive write):
///
/// ```ignore
/// leader_controller! {
///     pool: pool,
///     lease: "rise-ecr-controller",
///     holder: Uuid::new_v4(),
///     ttl: LEASE_DURATION,
///     shutdown: shutdown_token,
///     election: election,
///     schedules: {
///         "rise-ecr-provision" every provision_interval => self.provision(&election).await,
///         "rise-ecr-cleanup"   every cleanup_interval   => self.cleanup(&election).await,
///         "rise-ecr-drift"     every drift_interval     => self.drift(&election).await,
///     },
/// }
/// ```
///
/// The loop wakes at the shortest schedule interval; each schedule's
/// [`GlobalSchedule`](crate::GlobalSchedule) enforces its own cadence across
/// replicas, so the per-tick `try_claim` is the real gate — a tick that isn't
/// yet due for a given schedule is a cheap no-op.
#[macro_export]
macro_rules! leader_controller {
    (
        pool: $pool:expr,
        lease: $lease:expr,
        holder: $holder:expr,
        ttl: $ttl:expr,
        shutdown: $shutdown:expr,
        election: $election:ident,
        schedules: {
            $( $name:literal every $interval:expr => $work:expr ),+ $(,)?
        } $(,)?
    ) => {{
        let __pool = $pool;
        let __shutdown = $shutdown;
        $crate::safe::with_leader_election(
            ::std::clone::Clone::clone(&__pool),
            $lease,
            $holder,
            $ttl,
            ::std::clone::Clone::clone(&__shutdown),
            move |$election| async move {
                // Wake at the shortest schedule interval; each GlobalSchedule
                // gates its own cadence, so longer schedules simply no-op on the
                // ticks that aren't yet due for them.
                let mut __ticker = ::tokio::time::interval(
                    [$($interval),+]
                        .into_iter()
                        .min()
                        .expect("leader_controller requires at least one schedule"),
                );
                __ticker.set_missed_tick_behavior(::tokio::time::MissedTickBehavior::Delay);
                loop {
                    ::tokio::select! {
                        _ = __shutdown.cancelled() => break,
                        _ = __ticker.tick() => {}
                    }
                    if !$election.is_leader() {
                        continue;
                    }
                    $(
                        if $crate::GlobalSchedule::new(
                            ::std::clone::Clone::clone(&__pool),
                            $name,
                            $interval,
                        )
                        .try_claim_or_skip_as_leader($name, &$election)
                        .await
                        {
                            if let ::core::result::Result::Err(__err) = $work {
                                ::tracing::error!(
                                    schedule = $name,
                                    error = ?__err,
                                    "leader_controller work item failed"
                                );
                            }
                        }
                    )+
                }
                ::core::result::Result::Ok(())
            },
        )
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[sqlx::test]
    async fn with_global_lock_runs_body_then_releases(pool: PgPool) {
        let out = with_global_lock(&pool, "test/with-ok", || async { Ok(42_i32) })
            .await
            .unwrap();
        assert_eq!(out, 42);

        // The lock was released, so a fresh acquire succeeds immediately.
        GlobalLock::acquire(&pool, "test/with-ok")
            .await
            .unwrap()
            .release()
            .await
            .unwrap();
    }

    #[sqlx::test]
    async fn with_global_lock_releases_on_body_error(pool: PgPool) {
        let err = with_global_lock(&pool, "test/with-err", || async {
            Err::<(), _>(anyhow::anyhow!("boom"))
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("boom"));

        // Despite the body error, the lock must be free for the next caller.
        GlobalLock::acquire(&pool, "test/with-err")
            .await
            .unwrap()
            .release()
            .await
            .unwrap();
    }

    /// `with_leader_election` must delete the lease when the body returns, so a
    /// fresh election acquires immediately rather than waiting out the TTL.
    #[sqlx::test]
    async fn with_leader_election_releases_lease_on_exit(pool: PgPool) {
        // Long TTL: if release didn't run, the peer below couldn't acquire in time.
        with_leader_election(
            pool.clone(),
            "test/wle",
            Uuid::new_v4(),
            Duration::from_secs(60),
            CancellationToken::new(),
            |election| async move {
                // Wait until we actually hold the lease before returning.
                let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                while !election.is_leader() && tokio::time::Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                assert!(election.is_leader(), "should have become leader");
                Ok(())
            },
        )
        .await
        .unwrap();

        // A new election must win quickly because the lease row was deleted.
        let next = LeaderElection::spawn(
            pool.clone(),
            "test/wle",
            Uuid::new_v4(),
            Duration::from_secs(60),
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !next.is_leader() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            next.is_leader(),
            "peer should acquire immediately after with_leader_election released the lease"
        );
        next.release().await.unwrap();
    }

    /// Even if the body never observes shutdown, cancelling the token must drop
    /// the body and still release the lease (the structural release guarantee).
    #[sqlx::test]
    async fn with_leader_election_releases_when_body_ignores_shutdown(pool: PgPool) {
        let shutdown = CancellationToken::new();
        let handle = tokio::spawn(with_leader_election(
            pool.clone(),
            "test/wle-ignore",
            Uuid::new_v4(),
            Duration::from_secs(60),
            shutdown.clone(),
            // Never observes shutdown — would hold the lease forever on its own.
            |_election| async move {
                std::future::pending::<()>().await;
                Ok(())
            },
        ));

        // Let it become leader, then cancel; the helper must drop the body and release.
        tokio::time::sleep(Duration::from_millis(500)).await;
        shutdown.cancel();
        handle.await.unwrap().unwrap();

        let next = LeaderElection::spawn(
            pool.clone(),
            "test/wle-ignore",
            Uuid::new_v4(),
            Duration::from_secs(60),
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !next.is_leader() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            next.is_leader(),
            "peer should acquire after cancellation released the lease"
        );
        next.release().await.unwrap();
    }
}
