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
CREATE TABLE leader_schedules (
    name        VARCHAR(128) PRIMARY KEY,
    last_run_at TIMESTAMPTZ  NOT NULL
);
