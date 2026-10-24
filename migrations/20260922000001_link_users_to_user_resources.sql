-- The Rise `User` resource (ADR-0001 §1) a typed users row last logged in as.
--
-- A login resolves its exact upstream identity to a `User` resource and then
-- finds or creates this row by email, so the two identities are recorded side
-- by side here. The link moves with the latest login: at most one row points
-- at a given User resource. It is a cross-reference, not an authorization
-- fact — sessions authenticate the resource by its own UID.
ALTER TABLE users ADD COLUMN resource_user_uid UUID;
ALTER TABLE users ADD CONSTRAINT users_resource_user_uid_unique UNIQUE (resource_user_uid);
