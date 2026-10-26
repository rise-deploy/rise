-- Index `resources.labels` for key-existence lookups.
--
-- `list_label_setters` (the audit's label-backed detectors) finds every live
-- resource that sets one label key, across the whole store, with `labels ?
-- $1`. That operator is JSONB key-existence, which only the default
-- `jsonb_ops` GIN operator class supports; `jsonb_path_ops` (used by
-- `owner_references`) does not implement `?` at all, so it is not a model
-- here. Partial on live rows: a tombstoned resource's labels are never a
-- setter the detectors care about.
CREATE INDEX resources_labels_gin
    ON resource_store.resources
    USING GIN (labels)
    WHERE deletion_timestamp IS NULL;
