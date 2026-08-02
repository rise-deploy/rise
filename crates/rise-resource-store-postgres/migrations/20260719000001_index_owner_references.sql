CREATE INDEX resources_owner_references_gin
    ON resource_store.resources
    USING GIN (owner_references jsonb_path_ops);
