CREATE TABLE resources (
    uid                UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    api_version        TEXT        NOT NULL,
    kind               TEXT        NOT NULL,
    parent_uid         UUID        NULL REFERENCES resources(uid),
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

ALTER TABLE resources
    ADD CONSTRAINT resources_discriminator_format
    CHECK (discriminator ~ '^[a-z0-9][a-z0-9-]{6}[a-z0-9]$');

-- Names follow the DNS-label format for regular resources (my-org) and the DNS-subdomain
-- format for ResourceDefinitions (widgets.example.dev). Both share the same rule: starts
-- and ends with an alphanumeric character, no consecutive dots, no leading/trailing hyphens
-- per segment, max 253 chars (DNS subdomain limit).
ALTER TABLE resources
    ADD CONSTRAINT resources_name_format
    CHECK (
        name ~ '^[a-z0-9]([a-z0-9.-]{0,251}[a-z0-9])?$'
        AND position('..' in name) = 0
    );

ALTER TABLE resources
    ADD CONSTRAINT resources_metadata_is_object
    CHECK (jsonb_typeof(metadata) = 'object');

ALTER TABLE resources
    ADD CONSTRAINT resources_spec_is_object
    CHECK (jsonb_typeof(spec) = 'object');

ALTER TABLE resources
    ADD CONSTRAINT resources_status_is_object
    CHECK (jsonb_typeof(status) = 'object');

-- Same-level name uniqueness
CREATE UNIQUE INDEX resources_child_kind_name_unique
    ON resources (parent_uid, kind, name)
    WHERE parent_uid IS NOT NULL;

CREATE UNIQUE INDEX resources_root_kind_name_unique
    ON resources (kind, name)
    WHERE parent_uid IS NULL;

-- Same-level discriminator uniqueness
CREATE UNIQUE INDEX resources_child_discriminator_unique
    ON resources (parent_uid, discriminator)
    WHERE parent_uid IS NOT NULL;

CREATE UNIQUE INDEX resources_root_discriminator_unique
    ON resources (discriminator)
    WHERE parent_uid IS NULL;

CREATE OR REPLACE FUNCTION resources_set_updated_at()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = NOW();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER resources_updated_at
    BEFORE UPDATE ON resources
    FOR EACH ROW EXECUTE FUNCTION resources_set_updated_at();
