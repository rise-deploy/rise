-- Include api_version in the name-uniqueness indexes so resources from different API
-- groups can share the same (kind, name) within the same parent scope.
DROP INDEX resource_store.resources_child_kind_name_unique;
DROP INDEX resource_store.resources_root_kind_name_unique;

CREATE UNIQUE INDEX resources_child_kind_name_unique
    ON resource_store.resources (parent_uid, api_version, kind, name)
    WHERE parent_uid IS NOT NULL;

CREATE UNIQUE INDEX resources_root_kind_name_unique
    ON resource_store.resources (api_version, kind, name)
    WHERE parent_uid IS NULL;
