-- ADR-0007: promotion provenance, migration compatibility, promotion policy.
ALTER TABLE deployments
    -- deploy (fresh build/image), redeploy (same environment), promote (across
    -- environments) or rollback_guard.
    ADD COLUMN created_via TEXT NOT NULL DEFAULT 'deploy',
    -- The immediate cross-environment source; the image origin stays
    -- rolled_back_from_deployment_id.
    ADD COLUMN promoted_from_deployment_id UUID REFERENCES deployments(id) ON DELETE SET NULL,
    ADD COLUMN promoted_from_environment_id UUID REFERENCES environments(id) ON DELETE SET NULL,
    -- rise.toml [project].migration_version, inherited on create-from. NULL = undeclared.
    ADD COLUMN migration_version BIGINT,
    ADD CONSTRAINT deployments_created_via_known
        CHECK (created_via IN ('deploy', 'redeploy', 'promote', 'rollback_guard')),
    ADD CONSTRAINT deployments_migration_version_nonnegative
        CHECK (migration_version IS NULL OR migration_version >= 0);

-- Only what can be told apart honestly: rows with an image origin were redeploys.
UPDATE deployments SET created_via = 'redeploy' WHERE rolled_back_from_deployment_id IS NOT NULL;

CREATE INDEX idx_deployments_promoted_from ON deployments (promoted_from_deployment_id)
    WHERE promoted_from_deployment_id IS NOT NULL;
-- Environment migration high-water mark.
CREATE INDEX idx_deployments_env_migration_version
    ON deployments (environment_id, migration_version DESC)
    WHERE migration_version IS NOT NULL AND deploying_started_at IS NOT NULL;

-- Admin-set promotion source, kept inside the project by the composite key.
-- The API refuses to delete a promotion source (409); SET NULL only keeps
-- project deletion's cascade working.
ALTER TABLE environments
    ADD CONSTRAINT environments_project_id_id_key UNIQUE (project_id, id),
    ADD COLUMN promotion_source_environment_id UUID,
    ADD CONSTRAINT environments_promotion_source_fkey
        FOREIGN KEY (project_id, promotion_source_environment_id)
        REFERENCES environments (project_id, id)
        ON DELETE SET NULL (promotion_source_environment_id),
    ADD CONSTRAINT environments_promotion_source_not_self
        CHECK (promotion_source_environment_id IS DISTINCT FROM id);

-- Versions whose migrations break older code. Append-only: a barrier only
-- restricts rollbacks, so recording one never needs undoing.
CREATE TABLE migration_barriers (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id     UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    version        BIGINT NOT NULL CHECK (version > 0),
    reason         TEXT NOT NULL CHECK (length(reason) BETWEEN 1 AND 1024),
    recorded_by_id UUID REFERENCES users(id) ON DELETE SET NULL,
    recorded_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (project_id, version)
);

CREATE FUNCTION migration_barriers_append_only() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'migration_barriers is append-only';
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER migration_barriers_no_update BEFORE UPDATE ON migration_barriers
    FOR EACH ROW EXECUTE FUNCTION migration_barriers_append_only();
