-- Support controller authentication resolving live ControllerTrustPolicy
-- candidates by issuer alone (the controller identity is what authentication
-- is trying to determine, so this cannot be scoped to one Controller uid the
-- way `workload_trust_parent_issuer` is for a target-bound lookup).
CREATE INDEX controller_trust_policies_issuer
    ON resource_store.resources (((spec->>'issuer') COLLATE "C"))
    WHERE split_part(api_version, '/', 1) = 'rise.dev'
      AND kind = 'ControllerTrustPolicy'
      AND deletion_timestamp IS NULL;
