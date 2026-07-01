-- Internal-form registry image reference for a deployment, captured at
-- deploy-creation time. Decouples the controller from the in-memory tag
-- layout used by the registry provider: once set, the controller pulls
-- from this exact reference instead of reconstructing one from
-- (registry_url, project, deployment_id). NULL for pre-built deployments
-- (the `image_digest` column covers the full ref) and for rows persisted
-- before this column existed (controller falls back to reconstruction).

ALTER TABLE deployments
ADD COLUMN IF NOT EXISTS image_path TEXT NULL;

COMMENT ON COLUMN deployments.image_path IS
    'Internal-form registry image reference (e.g. "host/repo/path:tag") '
    'allocated at deploy creation. NULL for pre-built deployments and for '
    'legacy rows persisted before this column existed.';
