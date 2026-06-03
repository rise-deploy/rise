//! Internal plumbing for global state and cross-replica synchronization.
//!
//! Provides three Postgres-backed primitives used by the rise-deploy backend to
//! coordinate work safely across multiple replicas:
//!
//! - [`GlobalLock`] — install-wide mutex backed by Postgres advisory locks.
//! - [`LeaderElection`] — DB-backed leader lease with a background heartbeat.
//! - [`GlobalSchedule`] — globally-coordinated cadence gate for interval work.
//!
//! The crate owns its own migrations in a dedicated `runtime_sync` Postgres
//! schema (see [`run_migrations`]), keeping its tables and migration state
//! isolated from the root rise-deploy crate against the same database.

pub mod global_lock;
pub mod leader_leases;
pub mod leader_schedules;
pub mod safe;

pub use global_lock::GlobalLock;
pub use leader_leases::{LeaderElection, LeaderStatus, LeaseError, LEASE_DURATION};
pub use leader_schedules::GlobalSchedule;
pub use safe::{with_global_lock, with_leader_election};

/// Run runtime-sync migrations in their own Postgres schema (`runtime_sync`),
/// keeping both the application tables and the `_sqlx_migrations` tracking table
/// isolated from the root rise-deploy crate, which owns its own migrations
/// against the same database.
///
/// sqlx 0.8 hard-codes the unqualified `_sqlx_migrations` name, so we switch
/// `search_path` to make the migrator resolve it inside `runtime_sync`. That
/// mutates session state, so we run on a *detached* connection — closed when
/// we're done rather than returned to the pool — which guarantees the modified
/// `search_path` can never leak to another pool consumer (no best-effort reset
/// that a failure could skip).
pub async fn run_migrations(pool: &sqlx::PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    use sqlx::Executor;

    // Detach from the pool: this connection is sacrificed (closed on drop), so
    // the `SET search_path` below cannot poison a recycled connection.
    let mut conn = pool.acquire().await?.detach();

    conn.execute("CREATE SCHEMA IF NOT EXISTS runtime_sync")
        .await?;
    conn.execute("SET search_path TO runtime_sync, public")
        .await?;

    sqlx::migrate!("./migrations").run(&mut conn).await
}
