//! Session-scoped, install-wide mutex backed by Postgres advisory locks.
//!
//! Wraps `pg_advisory_lock` / `pg_advisory_unlock` so callers don't have to
//! manage connections, hash keys, or release paths themselves. The lock is
//! held by a single dedicated [`PoolConnection`] for the lifetime of the
//! [`GlobalLock`] value; [`GlobalLock::release`] performs the explicit
//! `pg_advisory_unlock` before returning the connection to the pool.
//!
//! # Why the explicit `release()` is required
//!
//! Postgres session-scoped advisory locks are released only when the SQL
//! session ends. `sqlx`'s [`PoolConnection::Drop`] does **not** close the
//! session — it returns the connection to the pool, where the next checkout
//! reuses the same session and inherits the lock. If a caller drops a
//! `GlobalLock` without calling [`release`](Self::release), the lock can
//! persist until the pool evicts the connection, which on a busy backend may
//! be never. Other replicas waiting on the same lock would then deadlock.
//!
//! `Drop` can't `await`, so the only enforcement is a logged error. Callers
//! must always `release().await` explicitly. If you find yourself wanting
//! true RAII semantics, consider wrapping the critical section in a
//! transaction and using `pg_advisory_xact_lock` directly — Postgres
//! releases transaction-scoped locks on commit/rollback. That doesn't suit
//! bootstrap (which calls non-transactional resource-store APIs), so it's
//! not provided here.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use sqlx::pool::PoolConnection;
use sqlx::{PgPool, Postgres};
use tracing::error;

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
/// [`try_acquire`](Self::try_acquire) (non-blocking). Always finish with
/// [`release`](Self::release).
pub struct GlobalLock {
    conn: PoolConnection<Postgres>,
    key: i64,
    name: String,
    released: bool,
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
            conn,
            key,
            name: name.to_string(),
            released: false,
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
            conn,
            key,
            name: name.to_string(),
            released: false,
        }))
    }

    /// Release the lock and return the underlying connection to the pool.
    ///
    /// Always call this — see the module docs for why `Drop` can't substitute.
    /// On unlock failure `released` stays `false` so `Drop` still emits the
    /// safety log; the caller is expected to handle the returned `Err`.
    pub async fn release(mut self) -> Result<()> {
        sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(self.key)
            .execute(&mut *self.conn)
            .await
            .with_context(|| format!("Failed to release GlobalLock '{}'", self.name))?;
        self.released = true;
        Ok(())
    }
}

impl Drop for GlobalLock {
    fn drop(&mut self) {
        if !self.released {
            error!(
                lock = %self.name,
                "GlobalLock dropped without release() — session-scoped advisory lock \
                 may persist until the pool evicts this connection. This is a programmer \
                 error: always call release().await."
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
