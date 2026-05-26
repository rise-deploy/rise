-- Scope custom domains to environments (default = the project's production environment).
-- One primary domain per (environment_id), not per (project_id).

ALTER TABLE project_custom_domains
    ADD COLUMN environment_id UUID REFERENCES environments(id) ON DELETE CASCADE;

UPDATE project_custom_domains pcd
SET environment_id = e.id
FROM environments e
WHERE e.project_id = pcd.project_id AND e.is_production = true;

ALTER TABLE project_custom_domains
    ALTER COLUMN environment_id SET NOT NULL;

CREATE INDEX idx_custom_domains_environment_id ON project_custom_domains(environment_id);

DROP INDEX idx_custom_domains_primary_unique;

CREATE UNIQUE INDEX idx_custom_domains_primary_unique
    ON project_custom_domains(environment_id)
    WHERE is_primary = true;
