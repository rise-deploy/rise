-- All resource-store tables (and the `_sqlx_migrations` tracking table written by
-- sqlx during this crate's migration run) live in a dedicated `resource_store`
-- schema so they stay isolated from the root rise-deploy crate, which owns its
-- own migrations against the same database.
CREATE SCHEMA IF NOT EXISTS resource_store;

-- ---------------------------------------------------------------------------
-- resources
-- ---------------------------------------------------------------------------

CREATE TABLE resource_store.resources (
    uid                UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    api_version        TEXT        NOT NULL,
    kind               TEXT        NOT NULL,
    parent_uid         UUID        NULL REFERENCES resource_store.resources(uid),
    name               TEXT        NOT NULL,
    discriminator      VARCHAR(8)  NOT NULL,
    metadata           JSONB       NOT NULL DEFAULT '{}',
    spec               JSONB       NOT NULL DEFAULT '{}',
    status             JSONB       NOT NULL DEFAULT '{}',
    revision           BIGINT      NOT NULL DEFAULT 1,
    finalizers         TEXT[]      NOT NULL DEFAULT '{}',
    deletion_timestamp TIMESTAMPTZ NULL,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

ALTER TABLE resource_store.resources
    ADD CONSTRAINT resources_discriminator_format
    CHECK (discriminator ~ '^[a-z0-9][a-z0-9-]{6}[a-z0-9]$');

-- Names follow the DNS-label format for regular resources (my-org) and the DNS-subdomain
-- format for ResourceDefinitions (widgets.example.dev). Per RFC 1123: each dot-separated
-- segment is a DNS label (starts and ends with alphanumeric, hyphens allowed in the middle,
-- max 63 chars). Total length capped at 253 (DNS subdomain limit).
ALTER TABLE resource_store.resources
    ADD CONSTRAINT resources_name_format
    CHECK (
        name ~ '^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?(\.[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?)*$'
        AND length(name) <= 253
    );

ALTER TABLE resource_store.resources
    ADD CONSTRAINT resources_metadata_is_object
    CHECK (jsonb_typeof(metadata) = 'object');

ALTER TABLE resource_store.resources
    ADD CONSTRAINT resources_spec_is_object
    CHECK (jsonb_typeof(spec) = 'object');

ALTER TABLE resource_store.resources
    ADD CONSTRAINT resources_status_is_object
    CHECK (jsonb_typeof(status) = 'object');

-- Same-level name uniqueness, keyed on the API group (the substring of api_version
-- before the first '/') rather than the full api_version. api_version is "group/version",
-- so keying on it would let the same logical resource exist twice under two versions of
-- one group; name resolution matches every served version and could then resolve to an
-- arbitrary row. split_part is IMMUTABLE, so it is safe in an index expression. Resources
-- from different groups may still share (kind, name) within a parent scope.
CREATE UNIQUE INDEX resources_child_kind_name_unique
    ON resource_store.resources (parent_uid, split_part(api_version, '/', 1), kind, name)
    WHERE parent_uid IS NOT NULL;

CREATE UNIQUE INDEX resources_root_kind_name_unique
    ON resource_store.resources (split_part(api_version, '/', 1), kind, name)
    WHERE parent_uid IS NULL;

-- Same-level discriminator uniqueness
CREATE UNIQUE INDEX resources_child_discriminator_unique
    ON resource_store.resources (parent_uid, discriminator)
    WHERE parent_uid IS NOT NULL;

CREATE UNIQUE INDEX resources_root_discriminator_unique
    ON resource_store.resources (discriminator)
    WHERE parent_uid IS NULL;

-- Backs `list_pending_collection` and the GC sweep that calls `try_collect` on
-- tombstoned rows. Partial so the index stays small relative to total resource count.
CREATE INDEX resources_pending_collection
    ON resource_store.resources (deletion_timestamp)
    WHERE deletion_timestamp IS NOT NULL;

CREATE OR REPLACE FUNCTION resource_store.resources_set_updated_at()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = NOW();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER resources_updated_at
    BEFORE UPDATE ON resource_store.resources
    FOR EACH ROW EXECUTE FUNCTION resource_store.resources_set_updated_at();

-- ---------------------------------------------------------------------------
-- resource_definitions
--
-- Projection table holding the indexed/queryable identity fields of
-- `ResourceDefinition` rows; kept in sync with the backing `resources` row.
-- ---------------------------------------------------------------------------

CREATE TABLE resource_store.resource_definitions (
    uid                          UUID        PRIMARY KEY REFERENCES resource_store.resources(uid) ON DELETE RESTRICT,
    group_name                   TEXT        NOT NULL,
    kind                         TEXT        NOT NULL,
    plural                       TEXT        NOT NULL,
    scope                        JSONB       NOT NULL,
    versions                     JSONB       NOT NULL,
    allowed_status_controller_ids TEXT[]     NOT NULL DEFAULT '{}',
    created_at                   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at                   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE UNIQUE INDEX resource_definitions_plural_unique
    ON resource_store.resource_definitions (plural);

CREATE UNIQUE INDEX resource_definitions_group_kind_unique
    ON resource_store.resource_definitions (group_name, kind);

CREATE OR REPLACE FUNCTION resource_store.resource_definitions_set_updated_at()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = NOW();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER resource_definitions_updated_at
    BEFORE UPDATE ON resource_store.resource_definitions
    FOR EACH ROW EXECUTE FUNCTION resource_store.resource_definitions_set_updated_at();
