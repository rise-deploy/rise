-- Activate the eight rise.dev/v1alpha1 identity built-ins.
--
-- One transaction, so a rejected upgrade leaves the database exactly as it was.
-- The compatibility audit runs before the reservation constraint is added: an
-- installation holding conflicting rows gets a counted, sampled diagnostic
-- naming what to remove, rather than a bare constraint violation.

-- ---------------------------------------------------------------------------
-- Compatibility audit
-- ---------------------------------------------------------------------------

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

-- ---------------------------------------------------------------------------
-- Durable reservation guard
-- ---------------------------------------------------------------------------

-- Reserves the whole rise.dev group against external ResourceDefinitions, plus
-- the eight identity collection names in any group. Collection names are
-- already globally unique (see resource_definitions_plural_unique), so the
-- cross-group half of this reserves nothing that was addressable anyway.
ALTER TABLE resource_store.resources
    ADD CONSTRAINT resource_definitions_identity_reservations
    CHECK (
        kind <> 'ResourceDefinition'
        OR (
            COALESCE(spec->>'group', '') <> 'rise.dev'
            AND COALESCE(spec->>'plural', '') NOT IN (
                'users',
                'useridentities',
                'controllers',
                'controllertrustpolicies',
                'groups',
                'groupmemberships',
                'serviceaccounts',
                'serviceaccounttrustpolicies'
            )
        )
    );

-- ---------------------------------------------------------------------------
-- Identity storage projections
-- ---------------------------------------------------------------------------

-- Scoped to the group and kind, deliberately not to one api_version: "one
-- external identity maps to at most one User" is a property of the kind, and a
-- version-scoped predicate would stop enforcing it the moment rows exist at a
-- second stored version.
CREATE UNIQUE INDEX user_identities_issuer_subject_unique
    ON resource_store.resources (
        ((spec->>'issuer') COLLATE "C"),
        ((spec->>'subject') COLLATE "C")
    )
    WHERE split_part(api_version, '/', 1) = 'rise.dev'
      AND kind = 'UserIdentity'
      AND deletion_timestamp IS NULL;

CREATE INDEX workload_trust_parent_issuer
    ON resource_store.resources (
        parent_uid,
        ((spec->>'issuer') COLLATE "C")
    )
    WHERE split_part(api_version, '/', 1) = 'rise.dev'
      AND kind IN ('ControllerTrustPolicy', 'ServiceAccountTrustPolicy')
      AND deletion_timestamp IS NULL;

CREATE INDEX group_memberships_user_name
    ON resource_store.resources ((name COLLATE "C"))
    WHERE split_part(api_version, '/', 1) = 'rise.dev'
      AND kind = 'GroupMembership'
      AND deletion_timestamp IS NULL;
