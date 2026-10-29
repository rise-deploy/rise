use std::time::Duration;

/// Grace period given to a claimant to finalize or release claimed state.
/// After this duration the claim expires and the row becomes claimable again.
pub const CLAIM_GRACE_TTL: Duration = Duration::from_secs(60);

use anyhow::Result;
use serde::{de::DeserializeOwned, Serialize};
use sqlx::PgPool;
use std::ops::Deref;
use uuid::Uuid;

#[derive(Debug)]
#[must_use = "claimed state must be explicitly finalized or released"]
pub struct ClaimedState<T> {
    pool: PgPool,
    lookup_key: String,
    claimant_id: Uuid,
    data: T,
    settled: bool,
}

impl<T> ClaimedState<T> {
    pub fn data(&self) -> &T {
        &self.data
    }

    pub async fn finalize(mut self) -> Result<()> {
        let affected = finalize(&self.pool, &self.lookup_key, self.claimant_id).await?;
        self.settled = true;
        if !affected {
            anyhow::bail!("finalize affected 0 rows: claim may have expired or been stolen");
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub async fn release(mut self) -> Result<()> {
        let affected = release_claim(&self.pool, &self.lookup_key, self.claimant_id).await?;
        self.settled = true;
        if !affected {
            anyhow::bail!("release affected 0 rows: claim may have expired or been stolen");
        }
        Ok(())
    }
}

impl<T> Deref for ClaimedState<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

impl<T> Drop for ClaimedState<T> {
    fn drop(&mut self) {
        if self.settled {
            return;
        }

        let pool = self.pool.clone();
        let lookup_key = self.lookup_key.clone();
        let claimant_id = self.claimant_id;

        tracing::warn!(
            claimant_id = %claimant_id,
            "claimed oauth transient state dropped without explicit settlement; releasing defensively"
        );

        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Err(error) = release_claim(&pool, &lookup_key, claimant_id).await {
                    tracing::warn!(
                        ?error,
                        claimant_id = %claimant_id,
                        "defensive release for dropped oauth transient state failed"
                    );
                }
            });
        } else {
            tracing::warn!(
                claimant_id = %claimant_id,
                "no tokio runtime available for defensive release of dropped oauth transient state"
            );
        }
    }
}

/// Store a value, overwriting any existing entry for the same lookup key.
pub async fn insert<T: Serialize>(
    pool: &PgPool,
    lookup_key: &str,
    data: &T,
    ttl: Duration,
) -> Result<()> {
    let data = serde_json::to_value(data)?;
    let ttl_secs = ttl.as_secs_f64();
    sqlx::query!(
        "INSERT INTO oauth_transient_state (lookup_key, data, expires_at, claimed_at, claim_expires_at, claimed_by)
         VALUES ($1, $2, NOW() + ($3 * INTERVAL '1 second'), NULL, NULL, NULL)
         ON CONFLICT (lookup_key) DO UPDATE
         SET data = $2,
             expires_at = NOW() + ($3 * INTERVAL '1 second'),
             claimed_at = NULL,
             claim_expires_at = NULL,
             claimed_by = NULL",
        lookup_key,
        data,
        ttl_secs,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn claim<T: DeserializeOwned>(
    pool: &PgPool,
    lookup_key: &str,
    claimant_id: Uuid,
    grace_ttl: Duration,
) -> Result<Option<ClaimedState<T>>> {
    let grace_secs = grace_ttl.as_secs_f64();
    let row = sqlx::query!(
        "UPDATE oauth_transient_state
         SET claimed_at = NOW(),
             claim_expires_at = NOW() + ($3 * INTERVAL '1 second'),
             claimed_by = $2
         WHERE lookup_key = $1
           AND expires_at > NOW()
           AND (claimed_by IS NULL OR claim_expires_at < NOW())
         RETURNING data",
        lookup_key,
        claimant_id,
        grace_secs,
    )
    .fetch_optional(pool)
    .await?;

    match row {
        Some(row) => Ok(Some(ClaimedState {
            pool: pool.clone(),
            lookup_key: lookup_key.to_string(),
            claimant_id,
            data: serde_json::from_value(row.data)?,
            settled: false,
        })),
        None => Ok(None),
    }
}

pub async fn finalize(pool: &PgPool, lookup_key: &str, claimant_id: Uuid) -> Result<bool> {
    let result = sqlx::query!(
        "DELETE FROM oauth_transient_state
         WHERE lookup_key = $1 AND claimed_by = $2",
        lookup_key,
        claimant_id,
    )
    .execute(pool)
    .await?;

    Ok(result.rows_affected() == 1)
}

pub async fn release_claim(pool: &PgPool, lookup_key: &str, claimant_id: Uuid) -> Result<bool> {
    let result = sqlx::query!(
        "UPDATE oauth_transient_state
         SET claimed_at = NULL,
             claim_expires_at = NULL,
             claimed_by = NULL
         WHERE lookup_key = $1 AND claimed_by = $2",
        lookup_key,
        claimant_id,
    )
    .execute(pool)
    .await?;

    Ok(result.rows_affected() == 1)
}

/// Delete all expired rows. Call periodically to keep the table small.
pub async fn delete_expired(pool: &PgPool) -> Result<u64> {
    let result = sqlx::query!("DELETE FROM oauth_transient_state WHERE expires_at <= NOW()")
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    struct TestState {
        value: String,
    }

    #[sqlx::test]
    async fn claim_blocks_other_claimants_until_grace_expires(pool: PgPool) -> Result<()> {
        insert(
            &pool,
            "lookup",
            &TestState {
                value: "hello".to_string(),
            },
            Duration::from_secs(60),
        )
        .await?;

        let first_claimant = Uuid::new_v4();
        let claimed =
            claim::<TestState>(&pool, "lookup", first_claimant, Duration::from_millis(800))
                .await?
                .expect("state should be claimed");
        assert_eq!(claimed.data().value, "hello");

        let second_claim =
            claim::<TestState>(&pool, "lookup", Uuid::new_v4(), Duration::from_millis(800)).await?;
        assert!(second_claim.is_none());

        tokio::time::sleep(Duration::from_millis(900)).await;

        let claimed_again =
            claim::<TestState>(&pool, "lookup", Uuid::new_v4(), Duration::from_millis(800)).await?;
        assert!(claimed_again.is_some());
        Ok(())
    }

    #[sqlx::test]
    async fn release_returns_row_to_claimable_state(pool: PgPool) -> Result<()> {
        insert(
            &pool,
            "lookup",
            &TestState {
                value: "hello".to_string(),
            },
            Duration::from_secs(60),
        )
        .await?;

        let claimant = Uuid::new_v4();
        claim::<TestState>(&pool, "lookup", claimant, Duration::from_secs(60))
            .await?
            .expect("state should be claimed")
            .release()
            .await?;

        let reclaimed =
            claim::<TestState>(&pool, "lookup", Uuid::new_v4(), Duration::from_secs(60)).await?;
        assert!(reclaimed.is_some());
        Ok(())
    }

    #[sqlx::test]
    async fn finalize_removes_the_row(pool: PgPool) -> Result<()> {
        insert(
            &pool,
            "lookup",
            &TestState {
                value: "hello".to_string(),
            },
            Duration::from_secs(60),
        )
        .await?;

        let claimant = Uuid::new_v4();
        claim::<TestState>(&pool, "lookup", claimant, Duration::from_secs(60))
            .await?
            .expect("state should be claimed")
            .finalize()
            .await?;

        let claimed =
            claim::<TestState>(&pool, "lookup", Uuid::new_v4(), Duration::from_secs(60)).await?;
        assert!(claimed.is_none());
        Ok(())
    }

    #[sqlx::test]
    async fn expired_rows_cannot_be_claimed(pool: PgPool) -> Result<()> {
        insert(
            &pool,
            "lookup",
            &TestState {
                value: "hello".to_string(),
            },
            Duration::from_millis(200),
        )
        .await?;

        tokio::time::sleep(Duration::from_millis(250)).await;

        let claimed =
            claim::<TestState>(&pool, "lookup", Uuid::new_v4(), Duration::from_secs(60)).await?;
        assert!(claimed.is_none());
        Ok(())
    }

    #[sqlx::test]
    async fn dropped_unsettled_claim_is_defensively_released(pool: PgPool) -> Result<()> {
        insert(
            &pool,
            "lookup",
            &TestState {
                value: "hello".to_string(),
            },
            Duration::from_secs(60),
        )
        .await?;

        let claimed_state =
            claim::<TestState>(&pool, "lookup", Uuid::new_v4(), Duration::from_secs(60))
                .await?
                .expect("state should be claimed");
        drop(claimed_state);

        tokio::time::sleep(Duration::from_millis(500)).await;

        let reclaimed =
            claim::<TestState>(&pool, "lookup", Uuid::new_v4(), Duration::from_secs(60)).await?;
        assert!(
            reclaimed.is_some(),
            "row should be claimable again after defensive release"
        );
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod test_faults {
    use super::*;

    pub async fn reject_deletes(pool: &PgPool) -> Result<()> {
        sqlx::query(
            "CREATE FUNCTION reject_oauth_transient_state_delete() RETURNS trigger
             LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected transient state deletion failure'; END $$",
        )
        .execute(pool)
        .await?;
        sqlx::query(
            "CREATE TRIGGER reject_oauth_transient_state_delete BEFORE DELETE ON oauth_transient_state
             FOR EACH ROW EXECUTE FUNCTION reject_oauth_transient_state_delete()",
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    pub async fn allow_deletes(pool: &PgPool) -> Result<()> {
        sqlx::query("DROP TRIGGER reject_oauth_transient_state_delete ON oauth_transient_state")
            .execute(pool)
            .await?;
        sqlx::query("DROP FUNCTION reject_oauth_transient_state_delete()")
            .execute(pool)
            .await?;
        Ok(())
    }

    pub async fn wait_until_released(pool: &PgPool, lookup_key: &str) -> Result<()> {
        for _ in 0..100 {
            let released: bool = sqlx::query_scalar(
                "SELECT claimed_by IS NULL FROM oauth_transient_state WHERE lookup_key = $1",
            )
            .bind(lookup_key)
            .fetch_one(pool)
            .await?;
            if released {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        anyhow::bail!("transient state claim was not released after deletion failure")
    }
}
