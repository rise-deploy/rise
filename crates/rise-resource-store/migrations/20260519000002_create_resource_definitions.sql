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
