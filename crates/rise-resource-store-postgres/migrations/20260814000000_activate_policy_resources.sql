-- Activate the four rise.dev/v1alpha1 policy built-ins.
--
-- One transaction, so a rejected upgrade leaves the database exactly as it was.
-- The compatibility audit runs before the reservation constraint is added: an
-- installation holding conflicting rows gets a counted, sampled diagnostic
-- naming what to remove, rather than a bare constraint violation.
--
-- No storage projection accompanies this activation. The identity indexes exist
-- because authentication resolves a login through them on every request;
-- bindings are read by the authorization engine through ordinary tree and
-- collection reads, so an index here would be speculative until that engine
-- lands and its access patterns are measurable.

-- ---------------------------------------------------------------------------
-- Compatibility audit
-- ---------------------------------------------------------------------------

-- The identity activation already closed the whole rise.dev group to external
-- ResourceDefinitions, so only the four policy collection names in other groups
-- can still conflict. Collection names are globally unique
-- (resource_definitions_plural_unique), so this reserves nothing that was
-- addressable anyway -- it keeps the guard and the audit describing the same
-- rule. Tombstoned definitions are included: route activation would shadow them
-- if they were later restored or inspected through their collection identity.
DO $$
DECLARE
    conflict_count bigint;
    conflict_sample text;
BEGIN
    WITH conflicts AS (
        SELECT name, uid, spec
        FROM resource_store.resources
        WHERE kind = 'ResourceDefinition'
          AND spec->>'plural' IN (
              'roles', 'rolebindings', 'platformroles', 'platformrolebindings'
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
                'policy built-in activation is blocked by %s legacy ResourceDefinition(s); sample: %s',
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
              'Role', 'RoleBinding', 'PlatformRole', 'PlatformRoleBinding'
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
                'policy built-in activation is blocked by %s legacy resource row(s); sample: %s',
                conflict_count, conflict_sample),
            HINT = 'remove the legacy rows with the previously deployed Rise version, then retry the upgrade';
    END IF;
END $$;

-- ---------------------------------------------------------------------------
-- Durable reservation guard
-- ---------------------------------------------------------------------------

ALTER TABLE resource_store.resources
    ADD CONSTRAINT resource_definitions_policy_reservations
    CHECK (
        kind <> 'ResourceDefinition'
        OR COALESCE(spec->>'plural', '') NOT IN (
            'roles',
            'rolebindings',
            'platformroles',
            'platformrolebindings'
        )
    );
