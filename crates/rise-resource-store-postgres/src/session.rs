//! Where a statement runs: on the pool, or inside one caller-owned transaction.
//!
//! ADR-0001 §5 requires every authorization-changing write to check and mutate
//! in a single `SERIALIZABLE` transaction, re-reading the writer, the recipient,
//! the relevant memberships, and the before/after resource state inside it. The
//! grant gate reads through the ordinary [`ResourceStore`] contract, so the only
//! way those reads can join the mutation's transaction is for the store instance
//! itself to be transaction-scoped.
//!
//! [`PgSession`] is that scoping. A store built over the pool behaves exactly as
//! before — each operation takes its own connection and opens its own
//! transaction where it needs one. A store built over a
//! [`SerializableTransaction`]'s session runs every statement on that one
//! connection, and the nested `begin()` calls the write paths already make
//! become `SAVEPOINT`s rather than independent transactions, so a failed
//! sub-step still unwinds without ending the caller's unit of work.
//!
//! [`ResourceStore`]: rise_resource_api::ResourceStore

use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use rise_resource_api::StoreError;
use sqlx::pool::PoolConnection;
use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use tokio::sync::{Mutex, MutexGuard};

/// A live transaction, or `None` once it has been committed.
type SharedTransaction = Arc<Mutex<Option<Transaction<'static, Postgres>>>>;

/// A database handle for the store: the pool, or one shared transaction.
#[derive(Clone)]
pub struct PgSession(Backend);

#[derive(Clone)]
enum Backend {
    Pool(PgPool),
    Transaction(SharedTransaction),
}

impl PgSession {
    /// Statements take a connection from the pool, as they always have.
    pub fn pool(pool: PgPool) -> Self {
        Self(Backend::Pool(pool))
    }

    /// Whether this session runs inside a caller-owned transaction.
    pub fn is_transactional(&self) -> bool {
        matches!(self.0, Backend::Transaction(_))
    }

    /// The pool this session draws from, if it is pool-backed.
    ///
    /// A transaction-scoped session deliberately answers `None`: reaching around
    /// it to the pool would run the statement outside the caller's transaction,
    /// which is the one mistake this type exists to prevent.
    pub fn pool_handle(&self) -> Option<&PgPool> {
        match &self.0 {
            Backend::Pool(pool) => Some(pool),
            Backend::Transaction(_) => None,
        }
    }

    /// Borrow a connection for one statement or one nested transaction.
    ///
    /// A transaction has exactly one connection, so the borrow is exclusive and
    /// deliberately *fails* rather than waits: the only way to ask for it while
    /// it is already lent out is to hold a guard across a call back into the
    /// store, and waiting for a guard the caller itself holds is a deadlock. An
    /// error names the bug at the call site instead of hanging the request.
    pub async fn acquire(&self) -> Result<PgSessionConnection<'_>, StoreError> {
        match &self.0 {
            Backend::Pool(pool) => Ok(PgSessionConnection::Pooled(
                pool.acquire().await.map_err(map_sqlx_error)?,
            )),
            Backend::Transaction(shared) => {
                let guard = shared.try_lock().map_err(|_| {
                    StoreError::backend(std::io::Error::other(
                        "the transaction's connection is already borrowed; release it before \
                         calling back into the store",
                    ))
                })?;
                if guard.is_none() {
                    return Err(StoreError::backend(std::io::Error::other(
                        "the transaction backing this store has already been committed",
                    )));
                }
                Ok(PgSessionConnection::Transaction(guard))
            }
        }
    }
}

/// A borrowed connection, from the pool or from the shared transaction.
pub enum PgSessionConnection<'a> {
    Pooled(PoolConnection<Postgres>),
    Transaction(MutexGuard<'a, Option<Transaction<'static, Postgres>>>),
}

impl PgSessionConnection<'_> {
    /// Open a nested unit of work on this connection.
    ///
    /// On a pooled connection this is an ordinary transaction. Inside a
    /// caller-owned transaction sqlx issues a `SAVEPOINT` instead, so a write
    /// path that rolls its own step back leaves the surrounding transaction
    /// intact — which is what lets the existing write paths run unchanged inside
    /// the grant gate's transaction.
    pub async fn begin(&mut self) -> Result<Transaction<'_, Postgres>, sqlx::Error> {
        use sqlx::Acquire;
        Acquire::begin(&mut **self).await
    }
}

impl Deref for PgSessionConnection<'_> {
    type Target = PgConnection;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Pooled(connection) => connection,
            // `acquire` refuses a committed transaction and holds the lock from
            // that check until this guard is dropped.
            Self::Transaction(guard) => guard.as_ref().expect("live transaction"),
        }
    }
}

impl DerefMut for PgSessionConnection<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            Self::Pooled(connection) => connection,
            Self::Transaction(guard) => guard.as_mut().expect("live transaction"),
        }
    }
}

/// One `SERIALIZABLE` unit of work spanning authorization checks and the
/// mutation they authorize (ADR-0001 §5).
///
/// Dropping without [`Self::commit`] rolls back: sqlx issues the `ROLLBACK` when
/// the transaction is dropped, so an early return on a refused write leaves
/// nothing behind.
pub struct SerializableTransaction {
    shared: SharedTransaction,
}

impl SerializableTransaction {
    /// Open a transaction at `SERIALIZABLE` isolation.
    ///
    /// The isolation level is set on the open transaction rather than on the
    /// session or the role, so a pooled connection carries nothing into the next
    /// checkout and ordinary reads keep their default isolation.
    pub async fn begin(pool: &PgPool) -> Result<Self, StoreError> {
        let mut transaction = pool.begin().await.map_err(map_sqlx_error)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx_error)?;
        Ok(Self {
            shared: Arc::new(Mutex::new(Some(transaction))),
        })
    }

    /// A session every store and lookup built from it shares.
    pub fn session(&self) -> PgSession {
        PgSession(Backend::Transaction(self.shared.clone()))
    }

    /// Commit. Returns [`StoreError::Serialization`] when PostgreSQL refuses the
    /// commit because the transaction's reads and writes admit no serial order —
    /// the caller retries the whole unit of work.
    pub async fn commit(&self) -> Result<(), StoreError> {
        let transaction = self.shared.lock().await.take().ok_or_else(|| {
            StoreError::backend(std::io::Error::other(
                "the transaction has already been committed",
            ))
        })?;
        transaction.commit().await.map_err(map_sqlx_error)
    }
}

/// Map a sqlx error, promoting the two SQLSTATEs that mean "replay me".
///
/// `40001` (serialization_failure) and `40P01` (deadlock_detected) are the only
/// errors a correct caller answers by retrying rather than by reporting; both
/// arrive from either a statement or the commit.
pub(crate) fn map_sqlx_error(error: sqlx::Error) -> StoreError {
    if is_serialization_failure(&error) {
        return StoreError::Serialization;
    }
    StoreError::backend(error)
}

/// Whether this error is PostgreSQL telling the caller to retry.
pub(crate) fn is_serialization_failure(error: &sqlx::Error) -> bool {
    let sqlx::Error::Database(database) = error else {
        return false;
    };
    matches!(database.code().as_deref(), Some("40001") | Some("40P01"))
}
