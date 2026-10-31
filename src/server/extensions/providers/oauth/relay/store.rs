//! PostgreSQL-backed `oauthrelay_core::TransientStore` for replay markers and refresh families.
//!
//! Every operation is a single statement in [`crate::db::oauth_relay_store`], so the
//! conditional writes are linearizable per key across replicas, and expiry is judged by the
//! database clock. A call that fails or exceeds [`STORE_TIMEOUT`] returns
//! [`StoreError::Backend`]: its outcome is unknown and the relay fails closed. Because each
//! write is one statement, a lost response never needs the value-comparison disambiguation the
//! trait describes for retrying backends.

use std::{future::Future, time::Duration};

use oauthrelay_core::{validate_key, validate_ttl, Entry, StoreError, TransientStore};
use sqlx::PgPool;

use crate::db::oauth_relay_store as db;

/// Upper bound on one store call, so a stalled database cannot hold a relay request open.
pub const STORE_TIMEOUT: Duration = Duration::from_secs(5);

/// Transient key-value store backed by the `oauth_relay_store` table.
pub struct PgTransientStore {
    pool: PgPool,
    timeout: Duration,
}

impl PgTransientStore {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            timeout: STORE_TIMEOUT,
        }
    }

    /// Runs one database call under the store timeout, mapping every failure to
    /// [`StoreError::Backend`].
    async fn bounded<T>(
        &self,
        operation: &'static str,
        call: impl Future<Output = anyhow::Result<T>>,
    ) -> Result<T, StoreError> {
        let error = match tokio::time::timeout(self.timeout, call).await {
            Ok(Ok(value)) => return Ok(value),
            Ok(Err(error)) => error,
            Err(_) => anyhow::anyhow!("timed out after {:?}", self.timeout),
        };
        let error = error.context(format!("oauth relay store {operation}"));
        tracing::warn!(error = ?error, "OAuth relay transient store call failed");
        Err(StoreError::Backend(error))
    }
}

#[async_trait::async_trait]
impl TransientStore for PgTransientStore {
    async fn put_if_absent(
        &self,
        key: &str,
        value: &[u8],
        ttl: Duration,
    ) -> Result<bool, StoreError> {
        validate_key(key)?;
        validate_ttl(ttl)?;
        self.bounded(
            "put_if_absent",
            db::put_if_absent(&self.pool, key, value, ttl),
        )
        .await
    }

    async fn get(&self, key: &str) -> Result<Option<Entry>, StoreError> {
        validate_key(key)?;
        let Some((value, expires_in)) = self.bounded("get", db::get(&self.pool, key)).await? else {
            return Ok(None);
        };
        let expires_in = Duration::try_from_secs_f64(expires_in).map_err(|error| {
            StoreError::Backend(anyhow::anyhow!(
                "oauth relay store get: invalid remaining TTL {expires_in}: {error}"
            ))
        })?;
        Ok(Some(Entry { value, expires_in }))
    }

    async fn compare_and_swap(
        &self,
        key: &str,
        expected: &[u8],
        new: &[u8],
        ttl: Duration,
    ) -> Result<bool, StoreError> {
        validate_key(key)?;
        validate_ttl(ttl)?;
        self.bounded(
            "compare_and_swap",
            db::compare_and_swap(&self.pool, key, expected, new, ttl),
        )
        .await
    }

    async fn delete(&self, key: &str) -> Result<(), StoreError> {
        validate_key(key)?;
        self.bounded("delete", db::delete(&self.pool, key)).await
    }
}

/// The `TransientStore` contract tests of oauthrelay's `MemoryStore`, run against Postgres.
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use oauthrelay_core::MAX_KEY_BYTES;

    use super::*;
    use crate::db::oauth_relay_store::test_support;

    const TTL: Duration = Duration::from_secs(60);
    const CONCURRENCY: usize = 16;

    #[sqlx::test]
    async fn put_if_absent_admits_only_the_first_writer(pool: PgPool) {
        let store = PgTransientStore::new(pool);
        assert!(store.put_if_absent("replay:a", b"1", TTL).await.unwrap());
        assert!(!store.put_if_absent("replay:a", b"2", TTL).await.unwrap());
        assert!(store.put_if_absent("replay:b", b"1", TTL).await.unwrap());
        let entry = store.get("replay:a").await.unwrap().unwrap();
        assert_eq!(entry.value, b"1");
        assert!(entry.expires_in <= TTL);
        assert!(entry.expires_in > TTL - Duration::from_secs(5));
    }

    #[sqlx::test]
    async fn concurrent_put_if_absent_has_one_winner(pool: PgPool) {
        let store = Arc::new(PgTransientStore::new(pool));
        let winner = race_put_if_absent(&store, "k").await;
        let stored = store.get("k").await.unwrap().unwrap().value;
        assert_eq!(stored, format!("v{winner}").into_bytes());
    }

    #[sqlx::test]
    async fn concurrent_put_if_absent_on_an_expired_record_has_one_winner(pool: PgPool) {
        let store = Arc::new(PgTransientStore::new(pool.clone()));
        assert!(store.put_if_absent("k", b"old", TTL).await.unwrap());
        test_support::expire(&pool, "k").await.unwrap();
        let winner = race_put_if_absent(&store, "k").await;
        let stored = store.get("k").await.unwrap().unwrap().value;
        assert_eq!(stored, format!("v{winner}").into_bytes());
    }

    /// Races `CONCURRENCY` writers with distinct values on `key`; asserts exactly one wins and
    /// returns its index.
    async fn race_put_if_absent(store: &Arc<PgTransientStore>, key: &'static str) -> usize {
        let tasks: Vec<_> = (0..CONCURRENCY)
            .map(|index| {
                let store = store.clone();
                tokio::spawn(async move {
                    let value = format!("v{index}").into_bytes();
                    store.put_if_absent(key, &value, TTL).await.unwrap()
                })
            })
            .collect();
        let mut winners = Vec::new();
        for (index, task) in tasks.into_iter().enumerate() {
            if task.await.unwrap() {
                winners.push(index);
            }
        }
        assert_eq!(winners.len(), 1, "winners: {winners:?}");
        winners[0]
    }

    #[sqlx::test]
    async fn expired_records_are_absent_and_can_be_recreated(pool: PgPool) {
        let store = PgTransientStore::new(pool.clone());
        assert!(store.put_if_absent("k", b"old", TTL).await.unwrap());
        assert!(store.get("k").await.unwrap().is_some());
        assert!(!store.put_if_absent("k", b"x", TTL).await.unwrap());
        test_support::expire(&pool, "k").await.unwrap();
        assert!(store.get("k").await.unwrap().is_none());
        assert!(!store
            .compare_and_swap("k", b"old", b"new", TTL)
            .await
            .unwrap());
        assert!(store.put_if_absent("k", b"fresh", TTL).await.unwrap());
        let entry = store.get("k").await.unwrap().unwrap();
        assert_eq!(entry.value, b"fresh");
        assert!(entry.expires_in > TTL - Duration::from_secs(5));
    }

    #[sqlx::test]
    async fn short_ttls_expire_by_the_database_clock(pool: PgPool) {
        let store = PgTransientStore::new(pool);
        let ttl = Duration::from_millis(300);
        assert!(store.put_if_absent("k", b"v", ttl).await.unwrap());
        let entry = store.get("k").await.unwrap().unwrap();
        assert!(entry.expires_in <= ttl);
        assert!(!store.put_if_absent("k", b"x", ttl).await.unwrap());
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(store.get("k").await.unwrap().is_none());
        assert!(!store.compare_and_swap("k", b"v", b"w", TTL).await.unwrap());
        assert!(store.put_if_absent("k", b"again", TTL).await.unwrap());
    }

    #[sqlx::test]
    async fn compare_and_swap_requires_the_current_value_and_restarts_the_ttl(pool: PgPool) {
        let store = PgTransientStore::new(pool.clone());
        assert!(!store.compare_and_swap("k", b"a", b"b", TTL).await.unwrap());
        store.put_if_absent("k", b"a", TTL).await.unwrap();
        assert!(!store
            .compare_and_swap("k", b"other", b"b", TTL)
            .await
            .unwrap());
        assert_eq!(store.get("k").await.unwrap().unwrap().value, b"a");
        test_support::set_expires_in(&pool, "k", Duration::from_secs(1))
            .await
            .unwrap();
        assert!(store.compare_and_swap("k", b"a", b"b", TTL).await.unwrap());
        // The old value no longer matches, so a second claimant loses.
        assert!(!store.compare_and_swap("k", b"a", b"c", TTL).await.unwrap());
        let entry = store.get("k").await.unwrap().unwrap();
        assert_eq!(entry.value, b"b");
        assert!(entry.expires_in <= TTL);
        assert!(entry.expires_in > TTL - Duration::from_secs(5));
    }

    #[sqlx::test]
    async fn concurrent_compare_and_swap_has_one_winner(pool: PgPool) {
        let store = Arc::new(PgTransientStore::new(pool));
        assert!(store.put_if_absent("k", b"active", TTL).await.unwrap());
        let tasks: Vec<_> = (0..CONCURRENCY)
            .map(|index| {
                let store = store.clone();
                tokio::spawn(async move {
                    let claim = format!("claimed:{index}").into_bytes();
                    store
                        .compare_and_swap("k", b"active", &claim, TTL)
                        .await
                        .unwrap()
                })
            })
            .collect();
        let mut winners = Vec::new();
        for (index, task) in tasks.into_iter().enumerate() {
            if task.await.unwrap() {
                winners.push(index);
            }
        }
        assert_eq!(winners.len(), 1, "winners: {winners:?}");
        let stored = store.get("k").await.unwrap().unwrap().value;
        assert_eq!(stored, format!("claimed:{}", winners[0]).into_bytes());
    }

    #[sqlx::test]
    async fn delete_removes_records_and_tolerates_absent_keys(pool: PgPool) {
        let store = PgTransientStore::new(pool);
        store.delete("missing").await.unwrap();
        store.put_if_absent("k", b"v", TTL).await.unwrap();
        store.delete("k").await.unwrap();
        assert!(store.get("k").await.unwrap().is_none());
        store.delete("k").await.unwrap();
        assert!(store.put_if_absent("k", b"v", TTL).await.unwrap());
    }

    #[sqlx::test]
    async fn values_are_opaque_bytes(pool: PgPool) {
        let store = PgTransientStore::new(pool);
        let value: Vec<u8> = (0..=255).collect();
        assert!(store.put_if_absent("k", &value, TTL).await.unwrap());
        assert_eq!(store.get("k").await.unwrap().unwrap().value, value);
        assert!(store
            .compare_and_swap("k", &value, &[0, 0xff, 0], TTL)
            .await
            .unwrap());
        assert_eq!(store.get("k").await.unwrap().unwrap().value, [0, 0xff, 0]);
    }

    #[sqlx::test]
    async fn invalid_requests_are_rejected(pool: PgPool) {
        let store = PgTransientStore::new(pool);
        let too_long = "k".repeat(MAX_KEY_BYTES + 1);
        for key in ["", too_long.as_str()] {
            assert!(matches!(
                store.put_if_absent(key, b"v", TTL).await,
                Err(StoreError::Invalid(_))
            ));
            assert!(matches!(store.get(key).await, Err(StoreError::Invalid(_))));
            assert!(matches!(
                store.compare_and_swap(key, b"a", b"b", TTL).await,
                Err(StoreError::Invalid(_))
            ));
            assert!(matches!(
                store.delete(key).await,
                Err(StoreError::Invalid(_))
            ));
        }
        assert!(matches!(
            store.put_if_absent("k", b"v", Duration::ZERO).await,
            Err(StoreError::Invalid(_))
        ));
        assert!(matches!(
            store
                .compare_and_swap("k", b"a", b"b", Duration::ZERO)
                .await,
            Err(StoreError::Invalid(_))
        ));
        let longest = "k".repeat(MAX_KEY_BYTES);
        assert!(store.put_if_absent(&longest, b"v", TTL).await.unwrap());
        assert!(store.get(&longest).await.unwrap().is_some());
    }

    #[sqlx::test]
    async fn a_stalled_call_times_out_as_a_backend_error(pool: PgPool) {
        let store = PgTransientStore {
            pool: pool.clone(),
            timeout: Duration::from_millis(200),
        };
        assert!(store.put_if_absent("k", b"a", TTL).await.unwrap());
        let mut blocker = pool.begin().await.unwrap();
        test_support::lock(&mut blocker, "k").await.unwrap();
        assert!(matches!(
            store.compare_and_swap("k", b"a", b"b", TTL).await,
            Err(StoreError::Backend(_))
        ));
        blocker.rollback().await.unwrap();
        // The database may still apply the abandoned statement once the lock is released,
        // which is why a `Backend` error means the outcome is unknown. Either way the record
        // holds one of the two values and the store keeps working.
        let value = store.get("k").await.unwrap().unwrap().value;
        assert!(value == b"a" || value == b"b", "unexpected value {value:?}");
        assert!(store.put_if_absent("other", b"v", TTL).await.unwrap());
    }

    /// Walks the rotation protocol described on [`TransientStore`] to show the four operations
    /// are sufficient.
    #[sqlx::test]
    async fn supports_the_documented_rotation_protocol(pool: PgPool) {
        let store = PgTransientStore::new(pool);
        let handle = "refresh:handle:h1";
        let family = "refresh:revoked:f1";
        let active = b"active:1";
        assert!(store.put_if_absent(handle, active, TTL).await.unwrap());
        // Two presenters race to claim the record they both read: exactly one wins.
        assert!(store
            .compare_and_swap(handle, active, b"claimed:A", TTL)
            .await
            .unwrap());
        assert!(!store
            .compare_and_swap(handle, active, b"claimed:B", TTL)
            .await
            .unwrap());
        // The loser reads the claim; the winner records the successor.
        assert_eq!(
            store.get(handle).await.unwrap().unwrap().value,
            b"claimed:A"
        );
        assert!(store
            .compare_and_swap(handle, b"claimed:A", b"rotated:A", TTL)
            .await
            .unwrap());
        // A stale claimant cannot overwrite the rotation, and a release only matches its own claim.
        assert!(!store
            .compare_and_swap(handle, b"claimed:A", b"active:2", TTL)
            .await
            .unwrap());
        assert_eq!(
            store.get(handle).await.unwrap().unwrap().value,
            b"rotated:A"
        );
        // Reuse revokes the family exactly once.
        assert!(store.get(family).await.unwrap().is_none());
        assert!(store.put_if_absent(family, b"1", TTL).await.unwrap());
        assert!(!store.put_if_absent(family, b"1", TTL).await.unwrap());
        assert!(store.get(family).await.unwrap().is_some());
        store.delete(family).await.unwrap();
        assert!(store.get(family).await.unwrap().is_none());
    }
}
