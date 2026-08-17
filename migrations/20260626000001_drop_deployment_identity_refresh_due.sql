-- Drop the per-deployment identity re-mint schedule.
--
-- The Kubernetes identity-refresh controller now reads each project's next
-- re-mint time from the `RiseProject` CR's `status.identityRefreshDueAt` (written
-- by the sync webhook) instead of a database column. The schedule is internal
-- bookkeeping owned entirely by the Kubernetes controller backend, so it no
-- longer belongs in the shared deployments table.
DROP INDEX IF EXISTS idx_deployments_identity_refresh_due;

ALTER TABLE deployments
    DROP COLUMN IF EXISTS identity_token_refresh_due_at;
