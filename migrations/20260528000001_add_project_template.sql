-- Remember which quickstart template a project was created from, plus the
-- exact image:tag reference at deploy time. Lets the UI show a "Template"
-- badge on project details and surface an "Update from template" affordance
-- when the catalog's image no longer matches what's deployed.

ALTER TABLE projects
    ADD COLUMN template_id TEXT,
    ADD COLUMN template_image TEXT;
