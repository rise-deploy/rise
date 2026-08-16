-- Add generic resource labels.
--
-- Labels are a dedicated column rather than a key inside `metadata`. The
-- `metadata` column is not a generic envelope: it stores exactly the
-- annotations map, and every read deserializes the whole column as a flat
-- string-to-string map. Nesting a `labels` object inside it would break every
-- existing read and require rewriting every stored row; a new column is
-- additive, needs no backfill, and can carry its own index when a
-- selector-driven query eventually justifies one.
--
-- Labels are ordinary resource data here. ADR-0001 §6.6 gates *writes* to a
-- label that some binding selects on behind the authorization grant gate;
-- until that gate exists, the generic resource API remains operator-only, so
-- the only writers are operators.

ALTER TABLE resource_store.resources
    ADD COLUMN labels JSONB NOT NULL DEFAULT '{}';

ALTER TABLE resource_store.resources
    ADD CONSTRAINT resources_labels_is_object
    CHECK (jsonb_typeof(labels) = 'object');

-- Values are strings, mirroring the annotations contract. Key syntax is the
-- Kubernetes-shaped `LabelKey` grammar and is enforced by typed validation
-- before the write; this constraint keeps the stored shape honest against
-- direct SQL.
ALTER TABLE resource_store.resources
    ADD CONSTRAINT resources_labels_are_strings
    CHECK (NOT jsonb_path_exists(labels, '$.* ? (@.type() != "string")'));
