use super::models::{CreateEnvironmentRequest, EnvironmentResponse, UpdateEnvironmentRequest};
use crate::db::{environments as db_environments, models::Environment, projects};
use crate::server::auth::context::AuthContext;
use crate::server::auth::session_scope::MaybeSessionScope;
use crate::server::error::{ServerError, ServerErrorExt};
use crate::server::project::handlers::ensure_project_access_or_admin;
use crate::server::state::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use rise_backend_auth::session_scope::{Kind, Operation, Verb};
use std::collections::HashMap;
use uuid::Uuid;

/// Bounds of `rollback_guard.unhealthy_timeout_seconds`, mirrored by the
/// `environments_rollback_guard_timeout_range` CHECK constraint.
const GUARD_TIMEOUT_RANGE: std::ops::RangeInclusive<u32> = 60..=86400;

fn validate_guard_timeout(seconds: u32) -> Result<i32, ServerError> {
    if !GUARD_TIMEOUT_RANGE.contains(&seconds) {
        return Err(ServerError::bad_request(format!(
            "rollback_guard.unhealthy_timeout_seconds must be between {} and {} (got {seconds})",
            GUARD_TIMEOUT_RANGE.start(),
            GUARD_TIMEOUT_RANGE.end(),
        )));
    }
    Ok(seconds as i32)
}

/// The cycle (`[target, source, …, target]`) that making `source` the
/// promotion source of `target` would close. `sources` maps environment names
/// to their promotion source's name; `target`'s own entry is replaced, so it
/// is never followed.
fn promotion_cycle(
    target: &str,
    source: &str,
    sources: &HashMap<String, String>,
) -> Option<Vec<String>> {
    let mut path = vec![target.to_string()];
    let mut current = source;
    loop {
        path.push(current.to_string());
        if current == target {
            return Some(path);
        }
        // A pre-existing cycle that does not pass through `target`.
        if path[1..path.len() - 1].iter().any(|p| p == current) {
            return None;
        }
        current = sources.get(current)?;
    }
}

/// Resolve a requested promotion source for `env`; empty or `None` clears it.
fn resolve_promotion_source(
    env: &Environment,
    requested: Option<&str>,
    all: &[Environment],
) -> Result<Option<Uuid>, ServerError> {
    let Some(name) = requested.filter(|n| !n.is_empty()) else {
        return Ok(None);
    };
    let by_id = |id| all.iter().find(|e| e.id == id);
    let source = all
        .iter()
        .find(|e| e.name == name)
        .ok_or_else(|| ServerError::not_found(format!("Environment '{name}' not found")))?;
    if source.id == env.id {
        return Err(ServerError::bad_request(format!(
            "Environment '{}' cannot be its own promotion source",
            env.name
        )));
    }
    let sources: HashMap<String, String> = all
        .iter()
        .filter_map(|e| {
            Some((
                e.name.clone(),
                by_id(e.promotion_source_environment_id?)?.name.clone(),
            ))
        })
        .collect();
    if let Some(cycle) = promotion_cycle(&env.name, &source.name, &sources) {
        return Err(ServerError::bad_request(format!(
            "Setting '{}' as the promotion source of '{}' would create a promotion cycle: {}",
            source.name,
            env.name,
            cycle.join(" → ")
        )));
    }
    Ok(Some(source.id))
}

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

    let max_deployment_expiration = match payload.max_deployment_expiration.as_deref() {
        Some(raw) => Some(
            rise_backend_core::expiration::ExpirationDuration::parse(raw)
                .map_err(|e| {
                    ServerError::bad_request(format!(
                        "Invalid max_deployment_expiration '{}': {}",
                        raw, e
                    ))
                })?
                .to_string(),
        ),
        None => None,
    };

    let env = db_environments::create_with_flag_swap(
        &state.db_pool,
        project.id,
        &payload.name,
        payload.primary_deployment_group.as_deref(),
        payload.is_production,
        &payload.color,
        max_deployment_expiration.as_deref(),
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
        } else if msg.contains("valid_max_deployment_expiration") {
            ServerError::bad_request(format!(
                "Invalid max_deployment_expiration '{}'. Must be a duration like '7d', '12h' or '30m'.",
                payload.max_deployment_expiration.as_deref().unwrap_or("")
            ))
        } else {
            ServerError::internal_anyhow(e, "Failed to create environment")
        }
    })?;

    // The project's status follows its production environment's primary
    // group, which this change may have moved.
    if let Err(e) = projects::update_calculated_status(&state.db_pool, project.id).await {
        tracing::warn!(project = %project.name, "Failed to recalculate project status: {:?}", e);
    }

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
    MaybeSessionScope(scope): MaybeSessionScope,
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

    let all = db_environments::list_for_project(&state.db_pool, project.id)
        .await
        .internal_err("Failed to list environments")?;

    // A scoped session sees the environments it covers.
    let list = Operation::new(Verb::List, Kind::Environment);
    if let Some(scope) = &scope {
        scope.require_reach(list, &project.name)?;
    }
    Ok(Json(
        all.iter()
            .filter(|e| {
                scope
                    .as_ref()
                    .is_none_or(|s| s.allows(list, &project.name, Some(&e.name)))
            })
            .map(|e| EnvironmentResponse::with_names(e, &all))
            .collect(),
    ))
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

    let all = db_environments::list_for_project(&state.db_pool, project.id)
        .await
        .internal_err("Failed to list environments")?;
    let env = all
        .iter()
        .find(|e| e.name == env_name)
        .ok_or_else(|| ServerError::not_found(format!("Environment '{}' not found", env_name)))?;

    Ok(Json(EnvironmentResponse::with_names(env, &all)))
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
    let changes_policy =
        payload.promotion_source_environment.is_some() || payload.rollback_guard.is_some();
    if changes_policy && !state.is_admin(user).await {
        return Err(ServerError::forbidden(
            "Only administrators can change the promotion source or the Rollback Guard",
        ));
    }

    // Validate the policy before changing anything, so a rejected request
    // leaves the environment as it was.
    let promotion_source = match &payload.promotion_source_environment {
        Some(requested) => {
            let all = db_environments::list_for_project(&state.db_pool, project.id)
                .await
                .internal_err("Failed to list environments")?;
            Some(resolve_promotion_source(&env, requested.as_deref(), &all)?)
        }
        None => None,
    };
    let guard_timeout = payload
        .rollback_guard
        .as_ref()
        .and_then(|g| g.unhealthy_timeout_seconds)
        .map(validate_guard_timeout)
        .transpose()?;

    // Canonicalize before storing, same as on create, so the CHECK constraint
    // can never reject what this parser accepted.
    let max_deployment_expiration = match &payload.max_deployment_expiration {
        Some(Some(raw)) => Some(Some(
            rise_backend_core::expiration::ExpirationDuration::parse(raw)
                .map_err(|e| {
                    ServerError::bad_request(format!(
                        "Invalid max_deployment_expiration '{}': {}",
                        raw, e
                    ))
                })?
                .to_string(),
        )),
        Some(None) => Some(None),
        None => None,
    };

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
        max_deployment_expiration
            .as_ref()
            .map(|o| o.as_deref()),
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
        } else if msg.contains("valid_max_deployment_expiration") {
            ServerError::bad_request("Invalid max_deployment_expiration. Must be a duration like '7d', '12h' or '30m'.")
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

    if changes_policy {
        updated = db_environments::update_promotion_policy(
            &state.db_pool,
            updated.id,
            promotion_source,
            payload.rollback_guard.as_ref().and_then(|g| g.enabled),
            guard_timeout,
        )
        .await
        .internal_err("Failed to update promotion policy")?;
    }

    // The project's status follows its production environment's primary
    // group, which this change may have moved.
    if let Err(e) = projects::update_calculated_status(&state.db_pool, project.id).await {
        tracing::warn!(project = %project.name, "Failed to recalculate project status: {:?}", e);
    }

    tracing::info!(
        "Updated environment '{}' in project '{}'",
        updated.name,
        project.name
    );

    let all = db_environments::list_for_project(&state.db_pool, project.id)
        .await
        .internal_err("Failed to list environments")?;
    Ok(Json(EnvironmentResponse::with_names(&updated, &all)))
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

    // A promotion source stays until the environments promoting from it
    // name another one; deleting it would silently lift their gate.
    let dependents = db_environments::list_promotion_dependents(&state.db_pool, env.id)
        .await
        .internal_err("Failed to list environments promoting from this one")?;
    if let Some(first) = dependents.first() {
        let names = dependents
            .iter()
            .map(|e| format!("'{}'", e.name))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(ServerError::conflict(format!(
            "Environment '{}' is the promotion source of {}. Clear it first: rise environment update {} --promote-from ''",
            env_name, names, first.name
        )));
    }

    // Deleting an environment deletes its deployments; running ones are
    // stopped first, so their workloads are torn down by the controller.
    let live = crate::db::deployments::count_live_for_environment(&state.db_pool, env.id)
        .await
        .internal_err("Failed to count the environment's deployments")?;
    if live > 0 {
        return Err(ServerError::conflict(format!(
            "Environment '{}' has {} deployment{} that {} not stopped. Stop {} first; deleting the environment deletes its deployment history.",
            env_name,
            live,
            if live == 1 { "" } else { "s" },
            if live == 1 { "is" } else { "are" },
            if live == 1 { "it" } else { "them" },
        )));
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

    // The project's status follows its production environment's primary
    // group, which this change may have moved.
    if let Err(e) = projects::update_calculated_status(&state.db_pool, project.id).await {
        tracing::warn!(project = %project.name, "Failed to recalculate project status: {:?}", e);
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

#[cfg(test)]
mod tests {
    use super::{promotion_cycle, validate_guard_timeout};
    use std::collections::HashMap;

    fn cycle(target: &str, source: &str, edges: &[(&str, &str)]) -> Option<String> {
        let sources: HashMap<String, String> = edges
            .iter()
            .map(|(env, src)| (env.to_string(), src.to_string()))
            .collect();
        promotion_cycle(target, source, &sources).map(|c| c.join(" → "))
    }

    #[test]
    fn promotion_cycles() {
        let chain = [("staging", "dev"), ("production", "staging")];
        assert_eq!(
            cycle("production", "staging", &[("staging", "production")]).as_deref(),
            Some("production → staging → production")
        );
        assert_eq!(
            cycle("dev", "production", &chain).as_deref(),
            Some("dev → production → staging → dev")
        );
        assert_eq!(cycle("qa", "production", &chain), None);
        // production's current source is replaced, not followed.
        assert_eq!(cycle("production", "dev", &chain), None);
        // A pre-existing cycle elsewhere does not loop forever.
        assert_eq!(cycle("c", "a", &[("a", "b"), ("b", "a")]), None);
    }

    #[test]
    fn guard_timeout_bounds() {
        assert!(validate_guard_timeout(59).is_err());
        assert_eq!(validate_guard_timeout(60).unwrap(), 60);
        assert_eq!(validate_guard_timeout(86400).unwrap(), 86400);
        assert!(validate_guard_timeout(86401).is_err());
    }
}
