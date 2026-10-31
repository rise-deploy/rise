//! PostgreSQL-backed `oauthrelay_core::TransientStore` for replay markers and refresh families.

use std::time::Duration;

use oauthrelay_core::{Entry, StoreError, TransientStore};
use sqlx::PgPool;

/// Transient key-value store backed by the `oauth_relay_store` table.
pub struct PgTransientStore {
    pool: PgPool,
}

impl PgTransientStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn not_implemented() -> StoreError {
    StoreError::Backend(anyhow::anyhow!("not implemented"))
}

#[async_trait::async_trait]
impl TransientStore for PgTransientStore {
    async fn put_if_absent(
        &self,
        _key: &str,
        _value: &[u8],
        _ttl: Duration,
    ) -> Result<bool, StoreError> {
        Err(not_implemented())
    }

    async fn get(&self, _key: &str) -> Result<Option<Entry>, StoreError> {
        Err(not_implemented())
    }

    async fn compare_and_swap(
        &self,
        _key: &str,
        _expected: &[u8],
        _new: &[u8],
        _ttl: Duration,
    ) -> Result<bool, StoreError> {
        Err(not_implemented())
    }

    async fn delete(&self, _key: &str) -> Result<(), StoreError> {
        Err(not_implemented())
    }
}
