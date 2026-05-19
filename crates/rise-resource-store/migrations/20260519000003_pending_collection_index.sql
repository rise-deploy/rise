-- Backs `list_pending_collection` and the GC sweep that calls `try_collect` on
-- tombstoned rows. Partial so the index stays small relative to total resource count.
CREATE INDEX resources_pending_collection
    ON resource_store.resources (deletion_timestamp)
    WHERE deletion_timestamp IS NOT NULL;
