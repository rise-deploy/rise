-- ADR-0007 Phases 0–1: promotion provenance, migration compatibility, promotion policy.
ALTER TABLE deployments
    -- How the deployment came to be: a fresh build or image (`deploy`), a
    -- create-from within one environment (`redeploy`), a create-from across
    -- environments (`promote`), or an automatic Rollback Guard replacement.
    ADD COLUMN created_via TEXT NOT NULL DEFAULT 'deploy',
    -- The deployment this one was created from when it crossed environments
    -- (the immediate source, not the image origin, which is
    -- rolled_back_from_deployment_id).
    ADD COLUMN promoted_from_deployment_id UUID REFERENCES deployments(id) ON DELETE SET NULL,
    ADD COLUMN promoted_from_environment_id UUID REFERENCES environments(id) ON DELETE SET NULL,
    -- Declared by rise.toml [project].migration_version; inherited by every
    -- deployment created from another one. NULL = undeclared.
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

-- Promotion source policy (admin-set). The composite foreign key keeps the
-- source inside the same project.
ALTER TABLE environments
    ADD CONSTRAINT environments_project_id_id_key UNIQUE (project_id, id),
    ADD COLUMN promotion_source_environment_id UUID,
    ADD CONSTRAINT environments_promotion_source_fkey
        FOREIGN KEY (project_id, promotion_source_environment_id)
        REFERENCES environments (project_id, id)
        ON DELETE SET NULL (promotion_source_environment_id),
    ADD CONSTRAINT environments_promotion_source_not_self
        CHECK (promotion_source_environment_id IS DISTINCT FROM id);
-- The API refuses to delete an environment that is another's promotion
-- source (409); SET NULL only keeps project deletion's cascade working.

-- Versions whose migrations break older code. Append-only: a barrier can only
-- restrict rollbacks, so recording one never needs undoing.
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
