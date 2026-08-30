-- An append-only log of what happened to a deployment.
--
-- Distinct from `deployments.status` (one value) and
-- `deployments.controller_metadata` (a snapshot rewritten each reconcile tick):
-- both describe what is true now, neither can express a sequence. A container
-- that restarted eleven times has a counter saying so and nothing saying when.
CREATE TABLE deployment_events (
    -- A position in a stream rather than an addressable object, which is why
    -- this is a sequence and not the UUID every other table uses. It is NOT a
    -- commit order: values are assigned at tuple formation, so an event written
    -- inside a slow transaction can carry a lower id than one that commits
    -- first. Readers page on (recorded_at, id) and tolerate that.
    id            BIGSERIAL   PRIMARY KEY,
    deployment_id UUID        NOT NULL REFERENCES deployments(id) ON DELETE CASCADE,
    -- When it happened, per the clock of whatever observed it. Not necessarily
    -- trustworthy or monotonic, which is why nothing pages on it.
    occurred_at   TIMESTAMPTZ NOT NULL,
    -- When Rise found out. Pagination cuts on this, because it is the only one
    -- of the two clocks Rise controls.
    recorded_at   TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    kind          TEXT        NOT NULL,
    severity      TEXT        NOT NULL,
    source        TEXT        NOT NULL,
    message       TEXT,
    attributes    JSONB       NOT NULL DEFAULT '{}'::jsonb,
    -- Reserved for events derived from a repeated observation, where
    -- re-deriving the same edge must not write twice. Nothing sets it yet:
    -- status transitions leave it NULL, because a repeated transition is
    -- legitimate and must not be collapsed.
    dedupe_key    TEXT,

    CONSTRAINT deployment_events_severity_known
        CHECK (severity IN ('debug', 'info', 'warning', 'error')),
    -- Bounded because `backend_event` passes runtime strings through verbatim,
    -- and those name image references, ARNs and SSM parameter paths.
    CONSTRAINT deployment_events_message_bounded
        CHECK (message IS NULL OR length(message) <= 4096),
    CONSTRAINT deployment_events_attributes_bounded
        CHECK (pg_column_size(attributes) <= 16384)
);

-- There is deliberately no CHECK on `kind`: the vocabulary lives in
-- rise-backend-core, and a value list here would be a second copy needing a
-- migration per kind. `deployments.status`'s own value-list CHECK has already
-- been rewritten by four migrations.

-- Reads and the per-(deployment, kind) cap, which filters `kind` in the scan.
CREATE INDEX deployment_events_by_recorded
    ON deployment_events (deployment_id, recorded_at DESC, id DESC);

-- Reserved for a global age sweep, which does not exist yet: `recorded_at`
-- leads because a sweep cuts across deployments.
CREATE INDEX deployment_events_sweep ON deployment_events (recorded_at);

CREATE UNIQUE INDEX deployment_events_dedupe
    ON deployment_events (deployment_id, kind, dedupe_key)
    WHERE dedupe_key IS NOT NULL;

-- With the log in place, the controller-observed pod snapshot is no longer
-- produced: a deployment's observability is its event log.
-- Existing rows still carry the last snapshot each controller wrote, and
-- `controller_metadata` is surfaced verbatim for introspection, so leaving them
-- would display state that stopped advancing at upgrade time.
--
-- Only the two retired keys are removed. `controller_metadata` remains the
-- controllers' own bookkeeping: the ECS reconciler keeps its converged
-- task-definition hash under `ecs`, and dropping that would make every ECS
-- service read as drifted and roll once on the next reconcile.
UPDATE deployments
SET controller_metadata = controller_metadata - 'pod_status' - 'health'
WHERE controller_metadata ?| ARRAY['pod_status', 'health'];
