-- Per-deployment schedule for re-minting the workload-identity token Secret.
--
-- The Kubernetes sync webhook sets this to (mint time + 2/3 * the configured
-- identity_token_ttl_seconds) whenever it mints/re-mints a deployment's identity
-- tokens. The identity-refresh controller resyncs a project only when one of its
-- deployments is due (rather than resyncing every project on a fixed timer), so
-- it touches projects only when needed and the work is naturally staggered —
-- deployments minted at different times come due at different times.
--
-- NULL = no identity tokens to refresh (no [identity].audiences) or not yet
-- minted. The Docker backend re-mints on its own reconcile loop and does not use
-- this column.
ALTER TABLE deployments
    ADD COLUMN identity_token_refresh_due_at TIMESTAMPTZ;

-- Drives the "which deployments are due now?" query; partial so only the
-- identity-bearing rows occupy the index.
CREATE INDEX idx_deployments_identity_refresh_due
    ON deployments (identity_token_refresh_due_at)
    WHERE identity_token_refresh_due_at IS NOT NULL;
