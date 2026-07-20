-- no-transaction
-- Keep this file to one SQL statement: PostgreSQL treats multiple statements
-- in one query message as an implicit transaction, which CONCURRENTLY forbids.
CREATE UNIQUE INDEX CONCURRENTLY user_identities_issuer_subject_unique
    ON resource_store.resources (
        ((spec->>'issuer') COLLATE "C"),
        ((spec->>'subject') COLLATE "C")
    )
    WHERE api_version = 'rise.dev/v1alpha1'
      AND split_part(api_version, '/', 1) = 'rise.dev'
      AND kind = 'UserIdentity'
      AND deletion_timestamp IS NULL;
