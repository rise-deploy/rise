//! Persistence for the OAuth relay's transient key-value store (`oauth_relay_store`).
//!
//! Each operation is one SQL statement, so it is atomic without explicit locks: the
//! conditional writes take the row lock, and under READ COMMITTED a writer that waited on
//! that lock re-evaluates its condition against the row version the other writer committed.
//! Expiry is always judged by the database clock (`now()`), so every replica agrees on it.
//!
//! The `oauthrelay_core::TransientStore` contract (key/TTL validation, timeouts, error
//! mapping) is implemented on top of these helpers by
//! `server::extensions::providers::oauth::relay::store::PgTransientStore`.

use std::time::Duration;

use anyhow::Result;
use sqlx::PgPool;

/// Rows removed per `delete_expired` statement, so cleanup never holds a long lock.
const DELETE_EXPIRED_BATCH: i64 = 10_000;

/// Store `value` under `key` for `ttl` unless a live record exists; return whether it was
/// stored. An expired record counts as absent and is overwritten.
pub async fn put_if_absent(pool: &PgPool, key: &str, value: &[u8], ttl: Duration) -> Result<bool> {
    let row = sqlx::query_scalar!(
        r#"INSERT INTO oauth_relay_store (key, value, expires_at)
           VALUES ($1, $2, now() + make_interval(secs => $3::float8))
           ON CONFLICT (key) DO UPDATE
           SET value = EXCLUDED.value, expires_at = EXCLUDED.expires_at
           WHERE oauth_relay_store.expires_at <= now()
           RETURNING true AS "stored!""#,
        key,
        value,
        ttl.as_secs_f64(),
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some())
}

/// Read the live record for `key`: its value and the seconds left until it expires.
pub async fn get(pool: &PgPool, key: &str) -> Result<Option<(Vec<u8>, f64)>> {
    let row = sqlx::query!(
        r#"SELECT value,
                  GREATEST(EXTRACT(EPOCH FROM expires_at - now()), 0)::float8 AS "expires_in!"
           FROM oauth_relay_store
           WHERE key = $1 AND expires_at > now()"#,
        key,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| (row.value, row.expires_in)))
}

/// Replace the live record's value with `new` and restart its TTL, only if its current value
/// equals `expected`; return whether it was replaced. An absent or expired record never
/// matches.
pub async fn compare_and_swap(
    pool: &PgPool,
    key: &str,
    expected: &[u8],
    new: &[u8],
    ttl: Duration,
) -> Result<bool> {
    let result = sqlx::query!(
        "UPDATE oauth_relay_store
         SET value = $3, expires_at = now() + make_interval(secs => $4::float8)
         WHERE key = $1 AND value = $2 AND expires_at > now()",
        key,
        expected,
        new,
        ttl.as_secs_f64(),
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Remove the record for `key`. Removing an absent record succeeds.
pub async fn delete(pool: &PgPool, key: &str) -> Result<()> {
    sqlx::query!("DELETE FROM oauth_relay_store WHERE key = $1", key)
        .execute(pool)
        .await?;
    Ok(())
}

/// Delete expired records in batches; return how many were removed.
///
/// Reads and conditional writes already treat expired records as absent, so this only
/// bounds the table's size. Rows another transaction holds are skipped (they are being
/// rewritten), and the outer condition re-checks expiry, so a record revived concurrently
/// by `put_if_absent` is never deleted.
pub async fn delete_expired(pool: &PgPool) -> Result<u64> {
    let mut total = 0;
    loop {
        let deleted = sqlx::query!(
            "DELETE FROM oauth_relay_store
             WHERE key IN (
                 SELECT key FROM oauth_relay_store
                 WHERE expires_at <= now()
                 LIMIT $1
                 FOR UPDATE SKIP LOCKED
             )
             AND expires_at <= now()",
            DELETE_EXPIRED_BATCH,
        )
        .execute(pool)
        .await?
        .rows_affected();
        total += deleted;
        if deleted < DELETE_EXPIRED_BATCH as u64 {
            return Ok(total);
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::time::Duration;

    use anyhow::Result;
    use sqlx::{PgConnection, PgPool};

    /// Moves `key`'s expiry into the past without removing the row.
    pub async fn expire(pool: &PgPool, key: &str) -> Result<()> {
        sqlx::query(
            "UPDATE oauth_relay_store SET expires_at = now() - interval '1 second' WHERE key = $1",
        )
        .bind(key)
        .execute(pool)
        .await?;
        Ok(())
    }

    /// Sets `key` to expire `remaining` from now.
    pub async fn set_expires_in(pool: &PgPool, key: &str, remaining: Duration) -> Result<()> {
        sqlx::query(
            "UPDATE oauth_relay_store SET expires_at = now() + make_interval(secs => $2::float8)
             WHERE key = $1",
        )
        .bind(key)
        .bind(remaining.as_secs_f64())
        .execute(pool)
        .await?;
        Ok(())
    }

    /// Takes `key`'s row lock in `conn`'s open transaction.
    pub async fn lock(conn: &mut PgConnection, key: &str) -> Result<()> {
        sqlx::query("SELECT 1 FROM oauth_relay_store WHERE key = $1 FOR UPDATE")
            .bind(key)
            .execute(conn)
            .await?;
        Ok(())
    }

    /// Number of rows physically present, live or expired.
    pub async fn row_count(pool: &PgPool) -> Result<i64> {
        Ok(sqlx::query_scalar("SELECT count(*) FROM oauth_relay_store")
            .fetch_one(pool)
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: Duration = Duration::from_secs(60);

    #[sqlx::test]
    async fn delete_expired_removes_only_expired_rows(pool: PgPool) -> Result<()> {
        for key in ["live:1", "live:2", "expired:1", "expired:2", "expired:3"] {
            assert!(put_if_absent(&pool, key, b"v", TTL).await?);
        }
        for key in ["expired:1", "expired:2", "expired:3"] {
            test_support::expire(&pool, key).await?;
        }

        assert_eq!(delete_expired(&pool).await?, 3);
        assert_eq!(test_support::row_count(&pool).await?, 2);
        assert!(get(&pool, "live:1").await?.is_some());
        assert!(get(&pool, "live:2").await?.is_some());
        assert_eq!(delete_expired(&pool).await?, 0);
        Ok(())
    }

    #[sqlx::test]
    async fn delete_expired_drains_more_than_one_batch(pool: PgPool) -> Result<()> {
        let rows = DELETE_EXPIRED_BATCH + 5;
        sqlx::query(
            "INSERT INTO oauth_relay_store (key, value, expires_at)
             SELECT 'expired:' || n, '\\x00'::bytea, now() - interval '1 second'
             FROM generate_series(1, $1) AS n",
        )
        .bind(rows)
        .execute(&pool)
        .await?;
        assert!(put_if_absent(&pool, "live", b"v", TTL).await?);

        assert_eq!(delete_expired(&pool).await?, rows as u64);
        assert_eq!(test_support::row_count(&pool).await?, 1);
        Ok(())
    }
}
