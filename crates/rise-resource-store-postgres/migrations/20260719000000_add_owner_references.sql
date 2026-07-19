-- Owner references are optional, UID-authoritative lifecycle edges. Keep one
-- authoritative JSONB representation on the dependent resource and index it
-- for reverse containment lookups; transactional owner locking prevents a
-- reference from racing owner deletion.
ALTER TABLE resource_store.resources
    ADD COLUMN owner_references JSONB NOT NULL DEFAULT '[]'::jsonb;

ALTER TABLE resource_store.resources
    ADD CONSTRAINT resources_owner_references_is_array
    CHECK (jsonb_typeof(owner_references) = 'array') NOT VALID;
