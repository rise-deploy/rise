-- The leader-election/synchronization tables moved into the `rise-runtime-sync`
-- crate, which now owns them in the dedicated `runtime_sync` Postgres schema
-- (see crates/rise-runtime-sync/migrations). Drop the now-unused copies in the
-- `public` schema that were created by 20260502000001_ha_support.sql and
-- 20260524000001_leader_schedules.sql.
--
-- These tables only ever held ephemeral coordination state: leases re-elect and
-- schedules re-initialize within one interval, so there is no data to preserve.
-- `oauth_transient_state` (also created by the ha_support migration) is
-- intentionally left in place — it remains owned by the main crate.
--
-- Schema-qualified to `public` so the drop targets the old tables regardless of
-- the session `search_path` — never the new `runtime_sync.leader_*` tables.
DROP TABLE IF EXISTS public.leader_schedules;
DROP TABLE IF EXISTS public.leader_leases;
