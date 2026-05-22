-- Track the Git repository a deployment was created from.
-- Derived by the CLI from common CI environment variables, or falling back to
-- the local `origin` remote, and normalized to an HTTPS URL.
ALTER TABLE deployments ADD COLUMN git_repository_url TEXT;
