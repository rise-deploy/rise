-- no-transaction
-- Keep this file to one SQL statement: PostgreSQL treats multiple statements
-- in one query message as an implicit transaction, which CONCURRENTLY forbids.
CREATE INDEX CONCURRENTLY group_memberships_user_name
    ON resource_store.resources ((name COLLATE "C"))
    WHERE split_part(api_version, '/', 1) = 'rise.dev'
      AND kind = 'GroupMembership'
      AND deletion_timestamp IS NULL;
