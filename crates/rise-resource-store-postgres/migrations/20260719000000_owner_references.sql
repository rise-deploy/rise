-- Owner references are optional, UID-authoritative lifecycle edges. Keep one
-- authoritative JSONB representation on the dependent resource and index it
-- for reverse containment lookups; transactional owner locking prevents a
-- reference from racing owner deletion.
--
-- The column is added with a constant default, so the CHECK below scans rows
-- that all hold '[]' and validating it immediately costs nothing.
ALTER TABLE resource_store.resources
    ADD COLUMN owner_references JSONB NOT NULL DEFAULT '[]'::jsonb;

ALTER TABLE resource_store.resources
    ADD CONSTRAINT resources_owner_references_is_array
    CHECK (jsonb_typeof(owner_references) = 'array');

CREATE INDEX resources_owner_references_gin
    ON resource_store.resources
    USING GIN (owner_references jsonb_path_ops);
