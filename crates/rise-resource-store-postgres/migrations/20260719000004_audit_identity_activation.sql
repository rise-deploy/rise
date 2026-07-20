-- Include tombstoned definitions: route activation would shadow them if they
-- were later restored or inspected through their collection identity. Bound
-- diagnostics to a count plus ten examples so a large legacy installation
-- cannot generate an unbounded startup error.
DO $$
DECLARE
    conflict_count bigint;
    conflict_sample text;
BEGIN
    WITH conflicts AS (
        SELECT name, uid, spec
        FROM resource_store.resources
        WHERE kind = 'ResourceDefinition'
          AND (
              spec->>'group' = 'rise.dev'
              OR spec->>'plural' IN (
                  'users', 'useridentities', 'controllers',
                  'controllertrustpolicies', 'groups', 'groupmemberships',
                  'serviceaccounts', 'serviceaccounttrustpolicies'
              )
          )
    ), sample AS (
        SELECT * FROM conflicts ORDER BY name, uid LIMIT 10
    )
    SELECT (SELECT count(*) FROM conflicts),
           (SELECT string_agg(
               format('%s (uid=%s, group=%s, plural=%s)', name, uid,
                   COALESCE(spec->>'group', '<missing>'),
                   COALESCE(spec->>'plural', '<missing>')),
               ', ' ORDER BY name, uid)
            FROM sample)
    INTO conflict_count, conflict_sample;

    IF conflict_count > 0 THEN
        RAISE EXCEPTION USING
            MESSAGE = format(
                'identity built-in activation is blocked by %s legacy ResourceDefinition(s); sample: %s',
                conflict_count, conflict_sample),
            HINT = 'remove each conflicting ResourceDefinition with the previously deployed Rise version, then retry the upgrade';
    END IF;
END $$;

-- Reject orphan/legacy rows that would be silently reinterpreted as trusted
-- built-ins once the immutable registry starts routing these exact identities.
DO $$
DECLARE
    conflict_count bigint;
    conflict_sample text;
BEGIN
    WITH conflicts AS (
        SELECT api_version, kind, name, uid
        FROM resource_store.resources
        WHERE split_part(api_version, '/', 1) = 'rise.dev'
          AND kind IN (
              'User', 'UserIdentity', 'Controller', 'ControllerTrustPolicy',
              'Group', 'GroupMembership', 'ServiceAccount',
              'ServiceAccountTrustPolicy'
          )
    ), sample AS (
        SELECT * FROM conflicts ORDER BY api_version, kind, name, uid LIMIT 10
    )
    SELECT (SELECT count(*) FROM conflicts),
           (SELECT string_agg(
               format('%s/%s %s (uid=%s)', api_version, kind, name, uid),
               ', ' ORDER BY api_version, kind, name, uid)
            FROM sample)
    INTO conflict_count, conflict_sample;

    IF conflict_count > 0 THEN
        RAISE EXCEPTION USING
            MESSAGE = format(
                'identity built-in activation is blocked by %s legacy resource row(s); sample: %s',
                conflict_count, conflict_sample),
            HINT = 'remove the legacy rows with the previously deployed Rise version, then retry the upgrade';
    END IF;
END $$;
