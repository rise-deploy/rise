-- Multi-container deployment support.
--
-- When `containers` IS NOT NULL, the deployment is multi-container and the
-- existing single-image columns (image, http_port, replicas, cpu, memory) are
-- ignored. Otherwise we fall back to the legacy single-container path so
-- existing rows keep reconciling identically.
--
-- Container env vars live inside the containers JSON (each ContainerSpec
-- carries its own env_overrides). The existing `deployment_env_vars` table
-- continues to hold project-wide overrides; the reconciler injects those into
-- every container's pod.
--
-- Both columns store a versioned envelope `{ "version": N, "items": [...] }`
-- (not a bare array) so the persisted shape can evolve — see
-- `models::CONTAINER_SIDE_DATA_VERSION` and `encode_side_data`/`decode_side_data`.
ALTER TABLE deployments
    ADD COLUMN containers JSONB,
    ADD COLUMN routes JSONB;
