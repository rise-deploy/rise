-- Install the durable write guard in its own short transaction. NOT VALID
-- protects all new writes immediately without scanning existing rows; the
-- bounded compatibility audit and validation run in following migrations.
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
    ) NOT VALID;
