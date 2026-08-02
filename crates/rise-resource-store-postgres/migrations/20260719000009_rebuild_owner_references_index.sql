-- Rebuild the owner-reference index inside a transaction.
--
-- It was originally created with CONCURRENTLY, which cannot run in one. That
-- split the index build from SQLx's bookkeeping insert, leaving a window where
-- a crash could record no migration for an index that exists (wedging the next
-- startup on "relation already exists") or leave an INVALID index that silently
-- indexes nothing. `resource_store.resources` is small enough that the brief
-- write lock a plain build takes is not worth that failure mode; revisit if the
-- typed-object migration makes this table large.
--
-- IF EXISTS, and an unconditional recreate, so this also heals a database left
-- in either of those states.
DROP INDEX IF EXISTS resource_store.resources_owner_references_gin;

CREATE INDEX resources_owner_references_gin
    ON resource_store.resources
    USING GIN (owner_references jsonb_path_ops);
