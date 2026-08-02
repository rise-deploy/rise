CREATE INDEX workload_trust_parent_issuer
    ON resource_store.resources (
        parent_uid,
        ((spec->>'issuer') COLLATE "C")
    )
    WHERE split_part(api_version, '/', 1) = 'rise.dev'
      AND kind IN ('ControllerTrustPolicy', 'ServiceAccountTrustPolicy')
      AND deletion_timestamp IS NULL;
