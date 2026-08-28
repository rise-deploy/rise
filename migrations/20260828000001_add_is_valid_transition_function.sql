-- Mirror of crate::state_machine::is_valid_transition, so the `mark_*`
-- deployment status writers can guard their own UPDATEs atomically at the
-- database level instead of relying on a check-then-act read in Rust.
--
-- Every arm here must match state_machine.rs's `is_valid_transition` exactly
-- (crates/rise-backend-core/src/state_machine.rs) -- keep them in sync by
-- hand; `db_is_valid_transition_matches_rust_is_valid_transition` in
-- src/db/deployments.rs enumerates every status pair and fails the build if
-- they drift apart.
CREATE OR REPLACE FUNCTION is_valid_transition(from_status TEXT, to_status TEXT)
RETURNS BOOLEAN AS $$
BEGIN
    -- Same status is always valid (allows an updated_at refresh).
    IF from_status = to_status THEN
        RETURN TRUE;
    END IF;

    -- Terminal states never transition further.
    IF is_terminal(from_status) THEN
        RETURN FALSE;
    END IF;

    RETURN (from_status, to_status) IN (
        -- Pre-infrastructure (cancellation path)
        ('Pending', 'Cancelling'), ('Building', 'Cancelling'),
        ('Pushing', 'Cancelling'), ('Pushed', 'Cancelling'), ('Deploying', 'Cancelling'),
        ('Cancelling', 'Cancelled'),

        -- Build/deploy path
        ('Pending', 'Building'),
        ('Building', 'Pushing'),
        ('Building', 'Pushed'), -- allow skipping Pushing if a status update fails
        ('Pushing', 'Pushed'),
        ('Pushed', 'Deploying'),

        -- Deployment outcomes
        ('Deploying', 'Healthy'), -- health checks pass
        ('Deploying', 'Failed'),  -- health checks fail

        -- Post-infrastructure (running state)
        ('Healthy', 'Unhealthy'), -- health degradation
        ('Unhealthy', 'Healthy'), -- health recovery
        ('Unhealthy', 'Failed'),  -- timeout without recovery

        -- Post-infrastructure (termination path)
        ('Healthy', 'Terminating'), ('Unhealthy', 'Terminating'),
        ('Terminating', 'Stopped'),    -- user-initiated termination
        ('Terminating', 'Superseded'), -- replaced by a newer deployment
        ('Terminating', 'Expired'),    -- deployment expired

        -- Build/deploy failures (before reaching Healthy)
        ('Pending', 'Failed'), ('Building', 'Failed'),
        ('Pushing', 'Failed'), ('Pushed', 'Failed')
    );
END;
$$ LANGUAGE plpgsql IMMUTABLE;
