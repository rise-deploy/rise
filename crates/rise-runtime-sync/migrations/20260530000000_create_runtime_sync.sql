-- Cross-replica synchronization primitives, isolated in the `runtime_sync`
-- schema. The crate's `run_migrations` switches `search_path` so the
-- `_sqlx_migrations` tracking table also lands here, keeping migration state
-- independent from the root rise-deploy crate (which owns its own migrations
-- against the same database).
CREATE SCHEMA IF NOT EXISTS runtime_sync;

-- Leader leases: prevents background controller loops from running on every replica.
-- Each controller acquires the lease for its name; only one replica holds it at a time.
-- Lease expires automatically, so a crashed replica's lock is reclaimed within expires_at.
CREATE TABLE runtime_sync.leader_leases (
    name         VARCHAR(64) PRIMARY KEY,
    holder_id    UUID        NOT NULL,
    heartbeat_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at   TIMESTAMPTZ NOT NULL
);

-- Leader schedules: globally-coordinated cadence for interval-based work.
--
-- Lease (`leader_leases`) guarantees mutual exclusion — only one replica is
-- leader at a time. But each replica runs its own `tokio::time::interval`,
-- anchored to that replica's start time. When leadership transitions, the
-- new leader's first tick can fire much sooner (or later) than the old
-- leader's next-scheduled work would have, producing a transient burst.
--
-- This table records the last globally-observed run timestamp per schedule
-- name. Workers gate their work on `last_run_at + interval <= NOW()` via an
-- atomic UPSERT in `GlobalSchedule::try_claim`, ensuring the new leader's
-- first run waits the full interval since the previous leader's last run.
--
-- Independent from leases: one controller may hold one lease but run several
-- schedules (e.g. ECR's provision/cleanup/drift loops share a lease but have
-- different cadences).
CREATE TABLE runtime_sync.leader_schedules (
    name        VARCHAR(128) PRIMARY KEY,
    last_run_at TIMESTAMPTZ  NOT NULL
);
