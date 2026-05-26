-- Add organization linkage to existing typed tables.
--
-- Phase 1 of a two-phase migration: introduce nullable `organization_resource_uid`
-- columns on `teams` and `projects`, plus a user_organization_memberships join table.
-- The backend bootstrap pass (under an advisory lock, after both root and
-- rise-resource-store migrations have run) creates the default Organization
-- resource and backfills these columns. A future PR will add NOT NULL and
-- foreign-key constraints once backfill is reliably complete.
--
-- The columns intentionally do NOT declare a FOREIGN KEY to
-- `resource_store.resources(uid)`: that schema is owned by the
-- rise-resource-store crate's migrations, which run AFTER these root
-- migrations. The application layer (and a future root migration) is
-- responsible for ensuring the referential integrity. Treat these columns as
-- soft references to `resource_store.resources(uid)` for the time being.

-- ---------------------------------------------------------------------------
-- user_organization_memberships
-- ---------------------------------------------------------------------------

CREATE TABLE user_organization_memberships (
    user_id                  UUID        NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    organization_resource_uid UUID       NOT NULL,
    created_at               TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (user_id, organization_resource_uid)
);

CREATE INDEX idx_user_org_memberships_org
    ON user_organization_memberships (organization_resource_uid);

-- ---------------------------------------------------------------------------
-- teams.organization_resource_uid
-- ---------------------------------------------------------------------------

ALTER TABLE teams
    ADD COLUMN organization_resource_uid UUID NULL;

CREATE INDEX idx_teams_organization_resource_uid
    ON teams (organization_resource_uid)
    WHERE organization_resource_uid IS NOT NULL;

-- ---------------------------------------------------------------------------
-- projects.organization_resource_uid
-- ---------------------------------------------------------------------------

ALTER TABLE projects
    ADD COLUMN organization_resource_uid UUID NULL;

CREATE INDEX idx_projects_organization_resource_uid
    ON projects (organization_resource_uid)
    WHERE organization_resource_uid IS NOT NULL;
