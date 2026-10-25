-- Caps expires_at of deployments created into a group other than an
-- environment's primary_deployment_group (or into any group, when the
-- environment has none). NULL means no cap.
ALTER TABLE environments
    ADD COLUMN max_deployment_expiration TEXT,
    ADD CONSTRAINT valid_max_deployment_expiration
        CHECK (max_deployment_expiration IS NULL OR max_deployment_expiration ~ '^[1-9][0-9]*[dhm]$');

COMMENT ON COLUMN environments.max_deployment_expiration IS
    'Caps expires_at of deployments created into a group other than primary_deployment_group. Canonical Nd|Nh|Nm. NULL = no cap.';
