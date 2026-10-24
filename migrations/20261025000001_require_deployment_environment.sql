-- Every deployment belongs to an environment.
--
-- Deployments without one are left over from two places: rows from before
-- environments existed in groups other than `default`, and the history of
-- environments that were deleted (`ON DELETE SET NULL`). New deployments
-- always resolve an environment: every project gets `production` when it is
-- created and it cannot be deleted.

-- 1. A project without any environment gets `production` first, so every
--    deployment below has somewhere to go.
INSERT INTO environments (project_id, name, primary_deployment_group, is_production)
SELECT p.id, 'production', 'default', true
FROM projects p
WHERE NOT EXISTS (SELECT 1 FROM environments e WHERE e.project_id = p.id);

-- 2. A deployment in some environment's primary group belongs to it.
UPDATE deployments d
SET environment_id = e.id
FROM environments e
WHERE d.environment_id IS NULL
  AND e.project_id = d.project_id
  AND e.primary_deployment_group = d.deployment_group;

-- 3. The rest predate environments or outlived theirs: they ran with the
--    project-wide variables, which is what production starts from.
UPDATE deployments d
SET environment_id = e.id
FROM environments e
WHERE d.environment_id IS NULL
  AND e.project_id = d.project_id
  AND e.is_production;

-- 4. A project whose production flag was moved away has no production
--    environment; use its oldest one.
UPDATE deployments d
SET environment_id = (
    SELECT e.id FROM environments e
    WHERE e.project_id = d.project_id
    ORDER BY e.created_at, e.id
    LIMIT 1
)
WHERE d.environment_id IS NULL;

-- 5. Deleting an environment deletes its deployments (the API refuses while
--    any of them is still running), instead of leaving them without one.
ALTER TABLE deployments DROP CONSTRAINT deployments_environment_id_fkey;
ALTER TABLE deployments
    ADD CONSTRAINT deployments_environment_id_fkey
    FOREIGN KEY (environment_id) REFERENCES environments(id) ON DELETE CASCADE;
ALTER TABLE deployments ALTER COLUMN environment_id SET NOT NULL;
