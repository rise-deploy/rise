-- no-transaction
-- Keep this file to one SQL statement: PostgreSQL treats multiple statements
-- in one query message as an implicit transaction, which CONCURRENTLY forbids.
CREATE INDEX CONCURRENTLY resources_owner_references_gin
    ON resource_store.resources
    USING GIN (owner_references jsonb_path_ops);
