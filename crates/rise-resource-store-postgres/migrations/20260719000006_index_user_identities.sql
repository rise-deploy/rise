-- no-transaction
-- Keep this file to one SQL statement: PostgreSQL treats multiple statements
-- in one query message as an implicit transaction, which CONCURRENTLY forbids.
-- Scoped to the group and kind, deliberately not to one api_version: "one
-- external identity maps to at most one User" is a property of the kind, and a
-- version-scoped predicate would stop enforcing it the moment rows exist at a
-- second stored version.
CREATE UNIQUE INDEX CONCURRENTLY user_identities_issuer_subject_unique
    ON resource_store.resources (
        ((spec->>'issuer') COLLATE "C"),
        ((spec->>'subject') COLLATE "C")
    )
    WHERE split_part(api_version, '/', 1) = 'rise.dev'
      AND kind = 'UserIdentity'
      AND deletion_timestamp IS NULL;
