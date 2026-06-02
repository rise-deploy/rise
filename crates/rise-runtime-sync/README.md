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
