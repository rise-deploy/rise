use super::models::*;
use crate::db::{extensions as db_extensions, projects};
use crate::server::auth::context::AuthContext;
use crate::server::error::{ServerError, ServerErrorExt};
use crate::server::state::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};

/// List all available extension types (registered providers)
pub async fn list_extension_types(
    State(state): State<AppState>,
    auth: AuthContext,
) -> Result<Json<ListExtensionTypesResponse>, ServerError> {
    let _user = auth.user()?;
    // Note: This endpoint doesn't require project access - it lists all available
    // extension types that any authenticated user can see and potentially enable on their projects

    let extension_types: Vec<ExtensionTypeMetadata> = state
        .extension_registry
        .iter()
        .map(|(_registry_key, extension)| ExtensionTypeMetadata {
            extension_type: extension.extension_type().to_string(),
            display_name: extension.display_name().to_string(),
            description: extension.description().to_string(),
            documentation: extension.documentation().to_string(),
            spec_schema: extension.spec_schema(),
        })
        .collect();

    Ok(Json(ListExtensionTypesResponse { extension_types }))
}

/// Create or upsert extension for project
pub async fn create_extension(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((project_name, extension_name)): Path<(String, String)>,
    Json(payload): Json<CreateExtensionRequest>,
) -> Result<Json<CreateExtensionResponse>, ServerError> {
    // Get project and verify access
    let project = projects::find_by_name(&state.db_pool, &project_name)
        .await
        .internal_err("Failed to look up project")?
        .ok_or_else(|| ServerError::not_found("Project not found"))?;

    let user = auth.user()?;
    let has_access = check_project_access(&state, user, project.id).await?;
    if !has_access {
        return Err(ServerError::forbidden("Access denied"));
    }

    // Get extension handler by type
    let extension = state
        .extension_registry
        .get(&payload.extension_type)
        .ok_or_else(|| {
            ServerError::bad_request(format!(
                "Unknown extension type: {}",
                payload.extension_type
            ))
        })?;

    // Validate spec
    extension
        .validate_spec(&payload.spec)
        .await
        .map_err(|e| ServerError::bad_request(format!("Invalid spec: {}", e)))?;

    // Create extension (will fail if already exists)
    let _ext_record = db_extensions::create(
        &state.db_pool,
        project.id,
        &extension_name,
        &payload.extension_type,
        &payload.spec,
    )
    .await
    .map_err(|e| {
        // Check if it's a unique constraint violation
        let error_msg = e.to_string();
        if error_msg.contains("duplicate key") || error_msg.contains("unique constraint") {
            ServerError::conflict(format!("Extension '{}' already exists", extension_name))
        } else {
            ServerError::internal_anyhow(e, "Failed to create extension")
        }
    })?;

    // Call extension's spec update hook (with empty old_spec for new extensions)
    extension
        .on_spec_updated(
            &serde_json::json!({}),
            &payload.spec,
            project.id,
            &extension_name,
            &state.db_pool,
        )
        .await
        .internal_err("Failed to run extension spec update hook")?;

    // Fetch updated extension to get the latest status (may have been initialized by on_spec_updated)
    let ext_record =
        db_extensions::find_by_project_and_name(&state.db_pool, project.id, &extension_name)
            .await
            .internal_err("Failed to fetch extension after creation")?
            .ok_or_else(|| ServerError::not_found("Extension not found"))?;

    // Format status using the extension provider
    let status_summary = extension.format_status(&ext_record.status);

    Ok(Json(CreateExtensionResponse {
        extension: Extension {
            extension: ext_record.extension,
            extension_type: extension.extension_type().to_string(),
            spec: ext_record.spec,
            status: ext_record.status,
            status_summary,
            created: ext_record.created_at.to_rfc3339(),
            updated: ext_record.updated_at.to_rfc3339(),
            deleted: ext_record.deleted_at.is_some(),
        },
    }))
}

/// Update extension (PUT for full replace, PATCH for partial update)
pub async fn update_extension(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((project_name, extension_name)): Path<(String, String)>,
    Json(payload): Json<UpdateExtensionRequest>,
) -> Result<Json<UpdateExtensionResponse>, ServerError> {
    // Get project and verify access
    let project = projects::find_by_name(&state.db_pool, &project_name)
        .await
        .internal_err("Failed to look up project")?
        .ok_or_else(|| ServerError::not_found("Project not found"))?;

    let user = auth.user()?;
    let has_access = check_project_access(&state, user, project.id).await?;
    if !has_access {
        return Err(ServerError::forbidden("Access denied"));
    }

    // Get existing extension to determine its type
    let existing =
        db_extensions::find_by_project_and_name(&state.db_pool, project.id, &extension_name)
            .await
            .internal_err("Failed to look up extension")?
            .ok_or_else(|| ServerError::not_found("Extension not found"))?;

    // Get extension handler by type
    let extension = state
        .extension_registry
        .get(&existing.extension_type)
        .ok_or_else(|| {
            ServerError::bad_request(format!(
                "Unknown extension type: {}",
                existing.extension_type
            ))
        })?;

    // Validate new spec
    extension
        .validate_spec(&payload.spec)
        .await
        .map_err(|e| ServerError::bad_request(format!("Invalid spec: {}", e)))?;

    // Update extension spec (preserving deleted_at and other fields)
    db_extensions::update_spec(&state.db_pool, project.id, &extension_name, &payload.spec)
        .await
        .internal_err("Failed to update extension")?;

    // Call extension's spec update hook
    extension
        .on_spec_updated(
            &existing.spec,
            &payload.spec,
            project.id,
            &extension_name,
            &state.db_pool,
        )
        .await
        .internal_err("Failed to run extension spec update hook")?;

    // Fetch updated extension to get the latest status (may have been modified by on_spec_updated)
    let ext_record =
        db_extensions::find_by_project_and_name(&state.db_pool, project.id, &extension_name)
            .await
            .internal_err("Failed to fetch extension after update")?
            .ok_or_else(|| ServerError::not_found("Extension not found"))?;

    // Format status using the extension provider
    let status_summary = extension.format_status(&ext_record.status);

    Ok(Json(UpdateExtensionResponse {
        extension: Extension {
            extension: ext_record.extension,
            extension_type: extension.extension_type().to_string(),
            spec: ext_record.spec,
            status: ext_record.status,
            status_summary,
            created: ext_record.created_at.to_rfc3339(),
            updated: ext_record.updated_at.to_rfc3339(),
            deleted: ext_record.deleted_at.is_some(),
        },
    }))
}

/// Patch extension (merge with nulls removing fields)
pub async fn patch_extension(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((project_name, extension_name)): Path<(String, String)>,
    Json(payload): Json<UpdateExtensionRequest>,
) -> Result<Json<UpdateExtensionResponse>, ServerError> {
    // Get project and verify access
    let project = projects::find_by_name(&state.db_pool, &project_name)
        .await
        .internal_err("Failed to look up project")?
        .ok_or_else(|| ServerError::not_found("Project not found"))?;

    let user = auth.user()?;
    let has_access = check_project_access(&state, user, project.id).await?;
    if !has_access {
        return Err(ServerError::forbidden("Access denied"));
    }

    // Get existing extension
    let existing =
        db_extensions::find_by_project_and_name(&state.db_pool, project.id, &extension_name)
            .await
            .internal_err("Failed to look up extension")?
            .ok_or_else(|| ServerError::not_found("Extension not found"))?;

    // Merge specs (null values in payload remove fields from existing)
    let merged_spec = merge_json_with_nulls(&existing.spec, &payload.spec);

    // Get extension handler by type
    let extension = state
        .extension_registry
        .get(&existing.extension_type)
        .ok_or_else(|| {
            ServerError::bad_request(format!(
                "Unknown extension type: {}",
                existing.extension_type
            ))
        })?;

    // Validate merged spec
    extension
        .validate_spec(&merged_spec)
        .await
        .map_err(|e| ServerError::bad_request(format!("Invalid spec after merge: {}", e)))?;

    // Update extension spec (preserving deleted_at and other fields)
    db_extensions::update_spec(&state.db_pool, project.id, &extension_name, &merged_spec)
        .await
        .internal_err("Failed to update extension")?;

    // Call extension's spec update hook
    extension
        .on_spec_updated(
            &existing.spec,
            &merged_spec,
            project.id,
            &extension_name,
            &state.db_pool,
        )
        .await
        .internal_err("Failed to run extension spec update hook")?;

    // Fetch updated extension to get the latest status (may have been modified by on_spec_updated)
    let ext_record =
        db_extensions::find_by_project_and_name(&state.db_pool, project.id, &extension_name)
            .await
            .internal_err("Failed to fetch extension after patch")?
            .ok_or_else(|| ServerError::not_found("Extension not found"))?;

    // Format status using the extension provider
    let status_summary = extension.format_status(&ext_record.status);

    Ok(Json(UpdateExtensionResponse {
        extension: Extension {
            extension: ext_record.extension,
            extension_type: extension.extension_type().to_string(),
            spec: ext_record.spec,
            status: ext_record.status,
            status_summary,
            created: ext_record.created_at.to_rfc3339(),
            updated: ext_record.updated_at.to_rfc3339(),
            deleted: ext_record.deleted_at.is_some(),
        },
    }))
}

/// List extensions for project
pub async fn list_extensions(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(project_name): Path<String>,
) -> Result<Json<ListExtensionsResponse>, ServerError> {
    let project = projects::find_by_name(&state.db_pool, &project_name)
        .await
        .internal_err("Failed to look up project")?
        .ok_or_else(|| ServerError::not_found("Project not found"))?;

    let user = auth.user()?;
    let has_access = check_project_access(&state, user, project.id).await?;
    if !has_access {
        return Err(ServerError::forbidden("Access denied"));
    }

    let extensions = db_extensions::list_by_project(&state.db_pool, project.id)
        .await
        .internal_err("Failed to list extensions")?;

    let extensions: Vec<Extension> = extensions
        .into_iter()
        .map(|e| {
            // Get extension provider by type to format status
            let status_summary = state
                .extension_registry
                .get(&e.extension_type)
                .map(|ext| ext.format_status(&e.status))
                .unwrap_or_else(|| "Unknown".to_string());

            Extension {
                deleted: e.deleted_at.is_some(),
                extension: e.extension,
                extension_type: e.extension_type,
                spec: e.spec,
                status: e.status,
                status_summary,
                created: e.created_at.to_rfc3339(),
                updated: e.updated_at.to_rfc3339(),
            }
        })
        .collect();

    Ok(Json(ListExtensionsResponse { extensions }))
}

/// Get extension by name
pub async fn get_extension(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((project_name, extension_name)): Path<(String, String)>,
) -> Result<Json<Extension>, ServerError> {
    let project = projects::find_by_name(&state.db_pool, &project_name)
        .await
        .internal_err("Failed to look up project")?
        .ok_or_else(|| ServerError::not_found("Project not found"))?;

    let user = auth.user()?;
    let has_access = check_project_access(&state, user, project.id).await?;
    if !has_access {
        return Err(ServerError::forbidden("Access denied"));
    }

    let ext = db_extensions::find_by_project_and_name(&state.db_pool, project.id, &extension_name)
        .await
        .internal_err("Failed to look up extension")?
        .ok_or_else(|| ServerError::not_found("Extension not found"))?;

    // Get extension provider by type to format status
    let status_summary = state
        .extension_registry
        .get(&ext.extension_type)
        .map(|ext_provider| ext_provider.format_status(&ext.status))
        .unwrap_or_else(|| "Unknown".to_string());

    Ok(Json(Extension {
        extension: ext.extension,
        extension_type: ext.extension_type,
        spec: ext.spec,
        status: ext.status,
        status_summary,
        created: ext.created_at.to_rfc3339(),
        updated: ext.updated_at.to_rfc3339(),
        deleted: ext.deleted_at.is_some(),
    }))
}

/// Delete extension (mark for deletion)
pub async fn delete_extension(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((project_name, extension_name)): Path<(String, String)>,
) -> Result<StatusCode, ServerError> {
    let project = projects::find_by_name(&state.db_pool, &project_name)
        .await
        .internal_err("Failed to look up project")?
        .ok_or_else(|| ServerError::not_found("Project not found"))?;

    let user = auth.user()?;
    let has_access = check_project_access(&state, user, project.id).await?;
    if !has_access {
        return Err(ServerError::forbidden("Access denied"));
    }

    db_extensions::mark_deleted(&state.db_pool, project.id, &extension_name)
        .await
        .internal_err("Failed to delete extension")?;

    Ok(StatusCode::NO_CONTENT)
}

/// Helper to check if user has access to project
async fn check_project_access(
    state: &AppState,
    user: &crate::db::models::User,
    project_id: uuid::Uuid,
) -> Result<bool, ServerError> {
    // Check if user is admin
    if state.is_admin(user).await {
        return Ok(true);
    }

    // Check if user has access to project (owner or team member)
    let accessible_projects = projects::list_accessible_by_user(&state.db_pool, user.id)
        .await
        .internal_err("Failed to check project access")?;

    Ok(accessible_projects.iter().any(|p| p.id == project_id))
}

/// Merge JSON values, treating null in update as field deletion
fn merge_json_with_nulls(
    existing: &serde_json::Value,
    update: &serde_json::Value,
) -> serde_json::Value {
    use serde_json::Value;

    match (existing, update) {
        (Value::Object(existing_map), Value::Object(update_map)) => {
            let mut result = existing_map.clone();
            for (key, value) in update_map.iter() {
                if value.is_null() {
                    // Null means remove the field
                    result.remove(key);
                } else if let Some(existing_value) = existing_map.get(key) {
                    // Recursively merge nested objects
                    result.insert(key.clone(), merge_json_with_nulls(existing_value, value));
                } else {
                    // New field
                    result.insert(key.clone(), value.clone());
                }
            }
            Value::Object(result)
        }
        _ => {
            // For non-objects, just replace with update value
            update.clone()
        }
    }
}
