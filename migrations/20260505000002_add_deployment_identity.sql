-- Workload identity for deployed apps.
--
-- identity_credential_hash: SHA-256 hash of the per-deployment bootstrap
--   credential used to authenticate to the token-exchange endpoint. The
--   credential itself is never stored. NULL until the controller has
--   reconciled the deployment and generated the credential.
-- identity_audiences: map of { in-pod filename -> token audience } for which
--   the controller auto-mints and mounts workload JWTs. Empty = no token
--   files mounted.
ALTER TABLE deployments
  ADD COLUMN identity_credential_hash TEXT,
  ADD COLUMN identity_audiences JSONB NOT NULL DEFAULT '{}'::jsonb;

CREATE INDEX idx_deployments_identity_credential_hash
  ON deployments (identity_credential_hash)
  WHERE identity_credential_hash IS NOT NULL;
