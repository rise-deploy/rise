-- ADR-0007 Phase 2: the Rollback Guard.
ALTER TABLE deployments
    -- When the deployment last moved into Unhealthy; NULL while Healthy. Unlike
    -- first_healthy_at it resets on recovery, so it measures *continuous*
    -- unhealthiness.
    ADD COLUMN unhealthy_since TIMESTAMPTZ,
    -- Set only on a Rollback Guard replacement: the unhealthy deployment it replaces.
    ADD COLUMN rollback_guard_for_deployment_id UUID REFERENCES deployments(id) ON DELETE SET NULL,
    ADD CONSTRAINT deployments_rollback_guard_candidate_via
        CHECK (rollback_guard_for_deployment_id IS NULL OR created_via = 'rollback_guard');

-- The guard timer starts at upgrade for anything already Unhealthy.
UPDATE deployments SET unhealthy_since = NOW() WHERE status = 'Unhealthy';

-- At most one Rollback Guard replacement per unhealthy deployment, ever.
CREATE UNIQUE INDEX deployments_one_rollback_guard_candidate
    ON deployments (rollback_guard_for_deployment_id)
    WHERE rollback_guard_for_deployment_id IS NOT NULL;
CREATE INDEX deployments_unhealthy_active
    ON deployments (unhealthy_since) WHERE status = 'Unhealthy' AND is_active;

ALTER TABLE environments
    ADD COLUMN rollback_guard_enabled BOOLEAN NOT NULL DEFAULT false,
    ADD COLUMN rollback_guard_unhealthy_timeout_seconds INTEGER NOT NULL DEFAULT 300,
    ADD CONSTRAINT environments_rollback_guard_timeout_range
        CHECK (rollback_guard_unhealthy_timeout_seconds BETWEEN 60 AND 86400);
