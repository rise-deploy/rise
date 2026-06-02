//! Session-scoped, install-wide mutex backed by Postgres advisory locks.
//!
//! Wraps `pg_advisory_lock` / `pg_advisory_unlock` so callers don't have to
//! manage connections, hash keys, or release paths themselves. The lock is
//! held by a single dedicated [`PoolConnection`] for the lifetime of the
//! [`GlobalLock`] value.
//!
//! # Release semantics
//!
//! Postgres session-scoped advisory locks are released only when the SQL
//! *session* ends. There are two ways that happens here:
//!
//! - [`GlobalLock::release`] runs an explicit `pg_advisory_unlock` and returns
//!   the connection to the pool — the fast, efficient path. Prefer it.
//! - If a `GlobalLock` is dropped **without** `release()` (an early `?`, a
//!   panic, or a forgotten call), [`Drop`] *detaches* the pooled connection and
//!   closes it. Postgres then releases the advisory lock server-side when the
//!   session ends. This sacrifices one pooled connection (it is closed rather
//!   than recycled) but guarantees the lock cannot leak — and needs no `await`.
//!
//! `Drop` can't `await`, so it can't run `pg_advisory_unlock` itself; closing
//! the connection is the async-free equivalent. (The previous design returned
//! the lock-holding connection to the pool on a forgotten `release()`, which
//! could leak the lock until the pool happened to evict that connection.)
//!
//! Because every code path now frees the lock, [`with_global_lock`] can offer a
//! scoped API that callers can't misuse:
//!
//! ```ignore
//! with_global_lock(&pool, "bootstrap/default-organization", || async move {
//!     // ... critical section ...
//!     Ok(())
//! })
//! .await?;
//! ```
//!
//! For a critical section that fits in a single transaction, prefer
//! `pg_advisory_xact_lock` directly — Postgres releases transaction-scoped
//! locks on commit/rollback. That doesn't suit callers (e.g. bootstrap) that
//! invoke non-transactional APIs across the locked region, which is why this
//! session-scoped helper exists.

use std::future::Future;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use sqlx::pool::PoolConnection;
use sqlx::{PgPool, Postgres};
use tracing::{debug, warn};

/// Derive a stable `i64` key from a human-readable lock name.
///
/// Uses the first 8 bytes of SHA-256 so the mapping is deterministic across
/// process restarts, Rust versions, and architectures. Callers should pick
/// scoped names (e.g. `"bootstrap/default-organization"`) — collisions would
/// silently serialize unrelated callers.
fn hash_key(name: &str) -> i64 {
    let digest = Sha256::digest(name.as_bytes());
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    i64::from_be_bytes(bytes)
}

/// Install-wide mutex held for the lifetime of this value.
///
/// Constructed via [`acquire`](Self::acquire) (blocking) or
/// [`try_acquire`](Self::try_acquire) (non-blocking). Prefer
/// [`release`](Self::release) to free the lock promptly and recycle the
/// connection; if the value is dropped without it, [`Drop`] still frees the
/// lock by closing the connection. [`with_global_lock`] wraps both for you.
pub struct GlobalLock {
    /// `Some` while the lock is held. [`release`](Self::release) takes it and
    /// returns the connection to the pool; [`Drop`] detaches and closes it if
    /// it's still present.
    conn: Option<PoolConnection<Postgres>>,
    key: i64,
    name: String,
}

impl GlobalLock {
    /// Acquire a session-scoped advisory lock named `name`. Blocks until the
    /// lock is free.
    pub async fn acquire(pool: &PgPool, name: &str) -> Result<Self> {
        let key = hash_key(name);
        let mut conn = pool
            .acquire()
            .await
            .with_context(|| format!("Failed to acquire DB connection for GlobalLock '{name}'"))?;
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(key)
            .execute(&mut *conn)
            .await
            .with_context(|| format!("Failed to acquire GlobalLock '{name}'"))?;
        Ok(Self {
            conn: Some(conn),
            key,
            name: name.to_string(),
        })
    }

    /// Try to acquire the lock without blocking. Returns `Ok(None)` when the
    /// lock is held by another session.
    #[allow(dead_code)]
    pub async fn try_acquire(pool: &PgPool, name: &str) -> Result<Option<Self>> {
        let key = hash_key(name);
        let mut conn = pool
            .acquire()
            .await
            .with_context(|| format!("Failed to acquire DB connection for GlobalLock '{name}'"))?;
        let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(key)
            .fetch_one(&mut *conn)
            .await
            .with_context(|| format!("Failed to try-acquire GlobalLock '{name}'"))?;
        if !acquired {
            return Ok(None);
        }
        Ok(Some(Self {
            conn: Some(conn),
            key,
            name: name.to_string(),
        }))
    }

    /// Release the lock with an explicit `pg_advisory_unlock` and return the
    /// connection to the pool — the preferred path.
    ///
    /// If the unlock query fails, the connection is *closed* instead of
    /// recycled (so a possibly-still-locked connection never re-enters the
    /// pool); Postgres then releases the lock when that session ends. The
    /// caller still receives the error for visibility.
    pub async fn release(mut self) -> Result<()> {
        let Some(mut conn) = self.conn.take() else {
            return Ok(());
        };
        match sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(self.key)
            .execute(&mut *conn)
            .await
        {
            // Unlocked cleanly: `conn` returns to the pool here, reusable.
            Ok(_) => Ok(()),
            Err(e) => {
                // Don't recycle a connection that may still hold the lock —
                // detach and close it so the session (and the lock) ends.
                drop(conn.detach());
                Err(e).with_context(|| format!("Failed to release GlobalLock '{}'", self.name))
            }
        }
    }
}

impl Drop for GlobalLock {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            // `release()` wasn't called (early `?`, panic, or forgotten). We
            // can't `await` `pg_advisory_unlock` here, so detach the pooled
            // connection and drop it: closing the SQL session releases the
            // advisory lock server-side. The connection is sacrificed rather
            // than recycled — acceptable for the rare, coarse locks this type
            // is meant for.
            drop(conn.detach());
            debug!(
                lock = %self.name,
                "GlobalLock dropped without release(); closed its connection to \
                 free the advisory lock server-side (prefer release() to recycle \
                 the connection)"
            );
        }
    }
}

/// Run `f` while holding the install-wide lock `name`, releasing it afterwards.
///
/// Acquires (blocking), awaits `f`, then [`release`](GlobalLock::release)s. The
/// lock is freed on every exit path: a normal return and an early `?` inside
/// `f` both go through `release()`; a panic unwinds through [`GlobalLock`]'s
/// [`Drop`], which closes the connection to free the lock. `f`'s result is
/// always returned as-is — a release error can't leak the lock (the connection
/// is closed on that path), so it's logged rather than propagated.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn hash_key_is_deterministic() {
        assert_eq!(hash_key("bootstrap"), hash_key("bootstrap"));
        assert_ne!(hash_key("bootstrap"), hash_key("bootstrap/v2"));
    }

    #[sqlx::test]
    async fn acquire_and_release_round_trip(pool: PgPool) {
        let lock = GlobalLock::acquire(&pool, "test/round-trip").await.unwrap();
        lock.release().await.unwrap();

        // Re-acquiring the same name on a fresh connection must succeed
        // immediately — proving the release actually happened.
        let lock2 = GlobalLock::acquire(&pool, "test/round-trip").await.unwrap();
        lock2.release().await.unwrap();
    }

    #[sqlx::test]
    async fn try_acquire_returns_none_when_held(pool: PgPool) {
        let held = GlobalLock::acquire(&pool, "test/contended").await.unwrap();

        // Second attempt on a different connection must observe contention.
        let busy = GlobalLock::try_acquire(&pool, "test/contended")
            .await
            .unwrap();
        assert!(
            busy.is_none(),
            "try_acquire should observe the lock as held"
        );

        held.release().await.unwrap();

        let free = GlobalLock::try_acquire(&pool, "test/contended")
            .await
            .unwrap();
        assert!(free.is_some(), "try_acquire should succeed after release");
        free.unwrap().release().await.unwrap();
    }

    #[sqlx::test]
    async fn distinct_names_do_not_collide(pool: PgPool) {
        let a = GlobalLock::acquire(&pool, "test/independent-a")
            .await
            .unwrap();
        let b = GlobalLock::try_acquire(&pool, "test/independent-b")
            .await
            .unwrap();
        assert!(b.is_some(), "distinct names must not contend");
        a.release().await.unwrap();
        b.unwrap().release().await.unwrap();
    }

    /// Dropping a held lock *without* calling `release()` must still free it:
    /// `Drop` closes the connection, and Postgres releases the session-scoped
    /// advisory lock when that session ends. The release isn't synchronous
    /// (the closed backend is reaped shortly after), so we poll.
    #[sqlx::test]
    async fn drop_without_release_frees_lock(pool: PgPool) {
        {
            let _lock = GlobalLock::acquire(&pool, "test/drop-frees").await.unwrap();
            // Intentionally no release() — fall off the scope and let Drop run.
        }

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(reacquired) = GlobalLock::try_acquire(&pool, "test/drop-frees")
                .await
                .unwrap()
            {
                reacquired.release().await.unwrap();
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "advisory lock was not freed within 5s of dropping the GlobalLock"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

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
}
