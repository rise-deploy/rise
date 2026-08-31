-- The last thing each backend saw of each replica.
--
-- Deliberately a *current-state* table, not a log: one row per replica, updated
-- in place. The history of what happened lives in `deployment_events`, derived
-- by comparing this row against the next observation. Keeping both a history
-- and a growing observation trail would mean two records of the same thing that
-- can disagree.
CREATE TABLE deployment_container_observations (
    deployment_id UUID        NOT NULL REFERENCES deployments(id) ON DELETE CASCADE,

    -- The backend's own stable handle for the replica: `web[0]` on Docker (a
    -- replica index it keeps in a label), the pod name on Kubernetes, the task
    -- id on ECS. Not a shared ordinal — two of the three backends have none,
    -- and inventing one would report replicas moving when they had not.
    subject       TEXT        NOT NULL,

    -- The declared container this is an instance of.
    container     TEXT        NOT NULL,

    -- The runtime's identity for the current incarnation of the subject, where
    -- the two differ. On Docker the subject is a slot that survives recreates
    -- while the container filling it is replaced — and a replacement's restart
    -- counter starts at zero, so without this a recreate is indistinguishable
    -- from nothing happening. On Kubernetes and ECS the subject already is the
    -- instance, and this simply repeats it.
    instance      TEXT,
    -- Only where the backend has a stable ordinal, which is Docker alone.
    replica       INTEGER,

    state         TEXT        NOT NULL,
    started_at    TIMESTAMPTZ,
    finished_at   TIMESTAMPTZ,
    exit_code     BIGINT,
    -- The runtime's own counter, where it keeps one. NULL on ECS, which has no
    -- in-place restart to count.
    restart_count BIGINT,
    health        TEXT,
    reason        TEXT,
    image         TEXT,

    -- When the backend looked, not when the replica entered this state.
    observed_at   TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),

    PRIMARY KEY (deployment_id, subject),

    CONSTRAINT deployment_container_observations_state_known
        CHECK (state IN ('pending', 'running', 'exited', 'unknown'))
);

-- Reads are always "every replica of this deployment", which the primary key
-- already serves. No second index: this table is written once per replica per
-- reconcile tick, so an index that earns nothing still costs every write.
