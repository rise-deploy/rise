-- All resource-store tables (and the `_sqlx_migrations` tracking table written by
-- sqlx during this crate's migration run) live in a dedicated `resource_store`
-- schema so they stay isolated from the root rise-deploy crate, which owns its
-- own migrations against the same database.
CREATE SCHEMA IF NOT EXISTS resource_store;
