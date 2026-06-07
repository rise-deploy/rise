-- Make the per-deployment bootstrap-credential hash UNIQUE (defense-in-depth).
--
-- Credentials are 256-bit CSPRNG values, so two deployments sharing a hash is
-- astronomically unlikely. A UNIQUE partial index turns that hypothetical
-- collision into a write-time rejection (the controller's
-- set_identity_credential_hash UPDATE fails loudly) instead of a silent two-row
-- state that the hash-indexed token-exchange lookup would only later surface as
-- an error. Replaces the non-unique partial index added in
-- 20260505000002_add_deployment_identity.sql; same predicate (NULLs excluded, so
-- the many deployments without a credential are unaffected).
DROP INDEX IF EXISTS idx_deployments_identity_credential_hash;

CREATE UNIQUE INDEX idx_deployments_identity_credential_hash
  ON deployments (identity_credential_hash)
  WHERE identity_credential_hash IS NOT NULL;
