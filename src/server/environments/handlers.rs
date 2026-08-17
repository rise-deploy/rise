use super::models::{CreateEnvironmentRequest, EnvironmentResponse, UpdateEnvironmentRequest};
use crate::db::{environments as db_environments, projects};
use crate::server::auth::context::AuthContext;
use crate::server::error::{ServerError, ServerErrorExt};
use crate::server::project::handlers::ensure_project_access_or_admin;
use crate::server::state::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};

/// Create a new environment for a project
pub async fn create_environment(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(project_id_or_name): Path<String>,
    Json(payload): Json<CreateEnvironmentRequest>,
) -> Result<(StatusCode, Json<EnvironmentResponse>), ServerError> {
    let project = if let Ok(uuid) = project_id_or_name.parse() {
        projects::find_by_id(&state.db_pool, uuid)
            .await
            .internal_err("Failed to get project")?
    } else {
        projects::find_by_name(&state.db_pool, &project_id_or_name)
            .await
            .internal_err("Failed to get project")?
    }
    .ok_or_else(|| ServerError::not_found("Project not found"))?;

    let user = auth.user()?;
    ensure_project_access_or_admin(&state, user, &project).await?;

    let env = db_environments::create_with_flag_swap(
        &state.db_pool,
        project.id,
        &payload.name,
        payload.primary_deployment_group.as_deref(),
        payload.is_production,
        &payload.color,
    )
    .await
    .map_err(|e| {
        let msg = format!("{e:#}");
        if msg.contains("duplicate key") || msg.contains("unique constraint") {
            if msg.contains("primary_deployment_group") {
                ServerError::new(
                    StatusCode::CONFLICT,
                    format!(
                        "Deployment group '{}' is already the primary group of another environment",
                        payload
                            .primary_deployment_group
                            .as_deref()
                            .unwrap_or("(none)")
                    ),
                )
            } else if msg.contains("idx_environments_production") {
                ServerError::new(
                    StatusCode::CONFLICT,
                    "Only one production environment is allowed per project",
                )
            } else {
                ServerError::new(
                    StatusCode::CONFLICT,
                    format!(
                        "Environment '{}' already exists in project '{}'",
                        payload.name, project.name
                    ),
                )
            }
        } else if msg.contains("valid_environment_name") {
            ServerError::bad_request(format!(
                "Invalid environment name '{}'. Must be lowercase alphanumeric with hyphens, no '--'.",
                payload.name
            ))
        } else if msg.contains("valid_environment_color") {
            ServerError::bad_request(format!(
                "Invalid environment color '{}'. Allowed colors: green, blue, yellow, red, purple, orange, gray.",
                payload.color
            ))
        } else {
            ServerError::internal_anyhow(e, "Failed to create environment")
        }
    })?;

    tracing::info!(
        "Created environment '{}' for project '{}'",
        env.name,
        project.name
    );

    Ok((StatusCode::CREATED, Json(env.into())))
}

/// List all environments for a project
pub async fn list_environments(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(project_id_or_name): Path<String>,
) -> Result<Json<Vec<EnvironmentResponse>>, ServerError> {
    let project = if let Ok(uuid) = project_id_or_name.parse() {
        projects::find_by_id(&state.db_pool, uuid)
            .await
            .internal_err("Failed to get project")?
    } else {
        projects::find_by_name(&state.db_pool, &project_id_or_name)
            .await
            .internal_err("Failed to get project")?
    }
    .ok_or_else(|| ServerError::not_found("Project not found"))?;

    let user = auth.user()?;
    ensure_project_access_or_admin(&state, user, &project).await?;

    let envs = db_environments::list_for_project(&state.db_pool, project.id)
        .await
        .internal_err("Failed to list environments")?;

    Ok(Json(envs.into_iter().map(|e| e.into()).collect()))
}

/// Get a specific environment by name
pub async fn get_environment(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((project_id_or_name, env_name)): Path<(String, String)>,
) -> Result<Json<EnvironmentResponse>, ServerError> {
    let project = if let Ok(uuid) = project_id_or_name.parse() {
        projects::find_by_id(&state.db_pool, uuid)
            .await
            .internal_err("Failed to get project")?
    } else {
        projects::find_by_name(&state.db_pool, &project_id_or_name)
            .await
            .internal_err("Failed to get project")?
    }
    .ok_or_else(|| ServerError::not_found("Project not found"))?;

    let user = auth.user()?;
    ensure_project_access_or_admin(&state, user, &project).await?;

    let env = db_environments::find_by_name(&state.db_pool, project.id, &env_name)
        .await
        .internal_err("Failed to get environment")?
        .ok_or_else(|| ServerError::not_found(format!("Environment '{}' not found", env_name)))?;

    Ok(Json(env.into()))
}

/// Update an environment
pub async fn update_environment(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((project_id_or_name, env_name)): Path<(String, String)>,
    Json(payload): Json<UpdateEnvironmentRequest>,
) -> Result<Json<EnvironmentResponse>, ServerError> {
    let project = if let Ok(uuid) = project_id_or_name.parse() {
        projects::find_by_id(&state.db_pool, uuid)
            .await
            .internal_err("Failed to get project")?
    } else {
        projects::find_by_name(&state.db_pool, &project_id_or_name)
            .await
            .internal_err("Failed to get project")?
    }
    .ok_or_else(|| ServerError::not_found("Project not found"))?;

    let user = auth.user()?;
    ensure_project_access_or_admin(&state, user, &project).await?;

    let env = db_environments::find_by_name(&state.db_pool, project.id, &env_name)
        .await
        .internal_err("Failed to get environment")?
        .ok_or_else(|| ServerError::not_found(format!("Environment '{}' not found", env_name)))?;

    // Reject early if non-admin tries to set deployment constraints
    if payload.deployment_constraints.is_some() && !state.is_admin(user).await {
        return Err(ServerError::forbidden(
            "Only administrators can update deployment constraints",
        ));
    }

    let mut updated = db_environments::update(
        &state.db_pool,
        env.id,
        project.id,
        payload.name.as_deref(),
        payload
            .primary_deployment_group
            .as_ref()
            .map(|o| o.as_deref()),
        payload.is_production,
        payload.color.as_deref(),
    )
    .await
    .map_err(|e| {
        let msg = format!("{e:#}");
        if msg.contains("duplicate key") || msg.contains("unique constraint") {
            if msg.contains("primary_deployment_group") {
                ServerError::new(
                    StatusCode::CONFLICT,
                    "Deployment group is already the primary group of another environment",
                )
            } else if msg.contains("idx_environments_production") {
                ServerError::new(
                    StatusCode::CONFLICT,
                    "Only one production environment is allowed per project",
                )
            } else {
                ServerError::new(
                    StatusCode::CONFLICT,
                    "Conflict: environment name already taken",
                )
            }
        } else if msg.contains("valid_environment_name") {
            ServerError::bad_request("Invalid environment name. Must be lowercase alphanumeric with hyphens, no '--'.")
        } else if msg.contains("valid_environment_color") {
            ServerError::bad_request("Invalid environment color. Allowed colors: green, blue, yellow, red, purple, orange, gray.")
        } else {
            ServerError::internal_anyhow(e, "Failed to update environment")
        }
    })?;

    // Update deployment constraints if provided (admin check already done above)
    if let Some(ref constraints) = payload.deployment_constraints {
        // Validate constraint values if provided
        if let (Some(min), Some(max)) = (constraints.min_replicas, constraints.max_replicas) {
            if min > max {
                return Err(ServerError::bad_request(format!(
                    "min_replicas ({}) must be <= max_replicas ({})",
                    min, max
                )));
            }
        }

        #[cfg(feature = "backend")]
        {
            use crate::server::deployment::quantity;
            if let Some(ref min_cpu) = constraints.min_cpu {
                quantity::parse_cpu_millicores(min_cpu)
                    .map_err(|e| ServerError::bad_request(format!("Invalid min_cpu: {}", e)))?;
            }
            if let Some(ref max_cpu) = constraints.max_cpu {
                quantity::parse_cpu_millicores(max_cpu)
                    .map_err(|e| ServerError::bad_request(format!("Invalid max_cpu: {}", e)))?;
            }
            if let (Some(ref min_cpu), Some(ref max_cpu)) =
                (&constraints.min_cpu, &constraints.max_cpu)
            {
                let min_val = quantity::parse_cpu_millicores(min_cpu).unwrap();
                let max_val = quantity::parse_cpu_millicores(max_cpu).unwrap();
                if min_val > max_val {
                    return Err(ServerError::bad_request(format!(
                        "min_cpu ({}) must be <= max_cpu ({})",
                        min_cpu, max_cpu
                    )));
                }
            }
            if let Some(ref min_memory) = constraints.min_memory {
                quantity::parse_memory_bytes(min_memory)
                    .map_err(|e| ServerError::bad_request(format!("Invalid min_memory: {}", e)))?;
            }
            if let Some(ref max_memory) = constraints.max_memory {
                quantity::parse_memory_bytes(max_memory)
                    .map_err(|e| ServerError::bad_request(format!("Invalid max_memory: {}", e)))?;
            }
            if let (Some(ref min_memory), Some(ref max_memory)) =
                (&constraints.min_memory, &constraints.max_memory)
            {
                let min_val = quantity::parse_memory_bytes(min_memory).unwrap();
                let max_val = quantity::parse_memory_bytes(max_memory).unwrap();
                if min_val > max_val {
                    return Err(ServerError::bad_request(format!(
                        "min_memory ({}) must be <= max_memory ({})",
                        min_memory, max_memory
                    )));
                }
            }
        }

        updated = db_environments::update_deployment_constraints(
            &state.db_pool,
            updated.id,
            constraints.min_replicas.map(|v| v as i32),
            constraints.max_replicas.map(|v| v as i32),
            constraints.min_cpu.clone(),
            constraints.max_cpu.clone(),
            constraints.min_memory.clone(),
            constraints.max_memory.clone(),
        )
        .await
        .internal_err("Failed to update deployment constraints")?;
    }

    tracing::info!(
        "Updated environment '{}' in project '{}'",
        updated.name,
        project.name
    );

    Ok(Json(updated.into()))
}

/// Delete an environment
pub async fn delete_environment(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((project_id_or_name, env_name)): Path<(String, String)>,
) -> Result<StatusCode, ServerError> {
    let project = if let Ok(uuid) = project_id_or_name.parse() {
        projects::find_by_id(&state.db_pool, uuid)
            .await
            .internal_err("Failed to get project")?
    } else {
        projects::find_by_name(&state.db_pool, &project_id_or_name)
            .await
            .internal_err("Failed to get project")?
    }
    .ok_or_else(|| ServerError::not_found("Project not found"))?;

    let user = auth.user()?;
    ensure_project_access_or_admin(&state, user, &project).await?;

    let env = db_environments::find_by_name(&state.db_pool, project.id, &env_name)
        .await
        .internal_err("Failed to get environment")?
        .ok_or_else(|| ServerError::not_found(format!("Environment '{}' not found", env_name)))?;

    if env.is_production {
        return Err(ServerError::bad_request(
            "Cannot delete the production environment. Set another environment as production first.",
        ));
    }

    let deleted = db_environments::delete(&state.db_pool, env.id)
        .await
        .internal_err("Failed to delete environment")?;

    if !deleted {
        return Err(ServerError::not_found(format!(
            "Environment '{}' not found",
            env_name
        )));
    }

    tracing::info!(
        "Deleted environment '{}' from project '{}'",
        env_name,
        project.name
    );

    // Best-effort cleanup of per-environment Kubernetes resources (e.g. ServiceAccount).
    // Failures are logged but don't fail the request — the SA will be cleaned up on
    // project deletion (namespace cascade) anyway.
    if let Err(e) = state
        .deployment_backend
        .cleanup_environment(&project, &env_name)
        .await
    {
        tracing::warn!(
            "Failed to clean up environment resources for '{}' in project '{}': {:?}",
            env_name,
            project.name,
            e
        );
    }

    Ok(StatusCode::NO_CONTENT)
}
