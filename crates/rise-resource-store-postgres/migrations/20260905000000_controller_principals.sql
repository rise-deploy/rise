-- Support controller authentication resolving live ControllerTrustPolicy
-- candidates by issuer alone (the controller identity is what authentication
-- is trying to determine, so this cannot be scoped to one Controller uid the
-- way `workload_trust_parent_issuer` is for a target-bound lookup).
CREATE INDEX controller_trust_policies_issuer
    ON resource_store.resources (((spec->>'issuer') COLLATE "C"))
    WHERE split_part(api_version, '/', 1) = 'rise.dev'
      AND kind = 'ControllerTrustPolicy'
      AND deletion_timestamp IS NULL;

-- Controllers are now ordinary RBAC principals: ResourceDefinition.spec no
-- longer carries allowedStatusControllerIds, so drop it from the view (a
-- view cannot have a column dropped in place) and strip the now-dead key
-- from stored specs. Nothing reads the key any more, so this does not bump
-- `revision` on the affected rows.
DROP VIEW resource_store.resource_definitions;

CREATE VIEW resource_store.resource_definitions AS
SELECT uid,
       spec->>'group'   AS group_name,
       spec->>'kind'    AS kind,
       spec->>'plural'  AS plural,
       spec->'versions' AS versions,
       created_at,
       updated_at
FROM resource_store.resources
WHERE kind = 'ResourceDefinition';

UPDATE resource_store.resources
   SET spec = spec - 'allowedStatusControllerIds'
 WHERE kind = 'ResourceDefinition'
   AND spec ? 'allowedStatusControllerIds';
