# rise-runtime-sync

Backend-only internal plumbing for global state and cross-replica
synchronization. Provides three Postgres-backed primitives used to coordinate
background work safely across multiple backend replicas:

- **`GlobalLock`** — install-wide mutex backed by Postgres advisory locks
  (`pg_advisory_lock`). Held for the lifetime of the value; release explicitly.
- **`LeaderElection`** — DB-backed leader lease with a background heartbeat.
  Exactly one replica holds a named lease at a time; a crashed holder's lease is
  reclaimed automatically once it expires.
- **`GlobalSchedule`** — globally-coordinated cadence gate for interval work,
  so a leadership handover doesn't produce a burst of duplicate runs.

## Usage

Prefer the scope-based helpers in the [`safe`](src/safe.rs) module over driving
the raw types by hand — they release the lock/lease on every exit path without a
manual call (and without relying on `Drop`, which can't `await`).

### `GlobalLock` — run a critical section under an install-wide lock

```rust
use rise_runtime_sync::with_global_lock;

with_global_lock(&pool, "bootstrap/default-organization", || async move {
    // serialized across all replicas; lock released when this returns
    seed_defaults(&pool).await?;
    Ok(())
})
.await?;
```

### `LeaderElection` — run a loop only on the elected leader

```rust
use rise_runtime_sync::with_leader_election;

with_leader_election(pool, "rise-my-controller", Uuid::new_v4(), ttl, shutdown.clone(),
    |election| async move {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,   // exit => lease released
                _ = ticker.tick() => {}
            }
            if !election.is_leader() { continue; }
            reconcile().await;
        }
        Ok(())
    })
    .await?;
```

### `GlobalSchedule` via `leader_controller!` — leader + cross-replica cadence

`GlobalSchedule` fences interval work so a leadership handover can't double-run
it. The `leader_controller!` macro composes one or more schedules under a single
lease into a shutdown-aware loop:

```rust
use rise_runtime_sync::leader_controller;

leader_controller! {
    pool: pool,
    lease: "rise-ecr-controller",
    holder: Uuid::new_v4(),
    ttl: Duration::from_secs(60),
    shutdown: shutdown,
    election: election,
    schedules: {
        "rise-ecr-provision" every Duration::from_secs(10) => self.provision(&election).await,
        "rise-ecr-cleanup"   every Duration::from_secs(5)  => self.cleanup(&election).await,
    },
}
.await
```

## Storage model

The crate owns its own migrations in the dedicated `runtime_sync` Postgres
schema (`leader_leases`, `leader_schedules`), kept isolated from the root
rise-deploy crate. Apply them via `rise_runtime_sync::run_migrations(&pool)`,
which switches `search_path` so the `_sqlx_migrations` tracking table also lands
in `runtime_sync`.

The query macros are compile-time verified and schema-qualify their tables
(`runtime_sync.leader_leases`, …). The offline `.sqlx` cache is crate-local;
regenerate it with `mise run runtime-sync:sqlx:prepare` (after
`mise run runtime-sync:db:migrate` has created the schema in the dev DB).
