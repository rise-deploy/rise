-- no-transaction
-- Keep this file to one SQL statement: PostgreSQL treats multiple statements
-- in one query message as an implicit transaction, which CONCURRENTLY forbids.
CREATE INDEX CONCURRENTLY workload_trust_parent_issuer
    ON resource_store.resources (
        parent_uid,
        ((spec->>'issuer') COLLATE "C")
    )
    WHERE api_version = 'rise.dev/v1alpha1'
      AND split_part(api_version, '/', 1) = 'rise.dev'
      AND kind IN ('ControllerTrustPolicy', 'ServiceAccountTrustPolicy')
      AND deletion_timestamp IS NULL;
