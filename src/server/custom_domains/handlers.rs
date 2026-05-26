use super::models::{
    AddCustomDomainRequest, CustomDomainResponse, CustomDomainsResponse, UpdateCustomDomainRequest,
};
use super::validation;
use crate::db::models::{CustomDomain, Environment};
use crate::db::{
    custom_domains as db_custom_domains, deployments as db_deployments,
    environments as db_environments, projects,
};
use crate::server::auth::context::AuthContext;
use crate::server::error::{ServerError, ServerErrorExt};
use crate::server::project::handlers::ensure_project_access_or_admin;
use crate::server::state::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use sqlx::PgPool;
use tracing::info;
use uuid::Uuid;

/// Resolve an environment name to its db row.
///
/// `None` resolves to the project's production environment. Returns `Err` with
/// a `bad_request` if the name was provided but doesn't exist.
async fn resolve_environment(
    pool: &PgPool,
    project_id: Uuid,
    env_name: Option<&str>,
) -> Result<Environment, ServerError> {
    match env_name {
        Some(name) => db_environments::find_by_name(pool, project_id, name)
            .await
            .internal_err("Failed to look up environment")?
            .ok_or_else(|| ServerError::bad_request(format!("Environment '{}' not found", name))),
        None => db_environments::find_production(pool, project_id)
            .await
            .internal_err("Failed to look up production environment")?
            .ok_or_else(|| ServerError::bad_request("Project has no production environment")),
    }
}

/// Look up the environment for a custom domain (the row's environment_id) and
/// return its name. Falls back to `?` for orphaned rows so we never error out
/// of a list response over data consistency.
async fn environment_name_for(pool: &PgPool, env_id: Uuid) -> String {
    db_environments::find_by_id(pool, env_id)
        .await
        .ok()
        .flatten()
        .map(|env| env.name)
        .unwrap_or_else(|| "?".to_string())
}

/// Mark the active deployment in `env`'s primary deployment group for
/// reconciliation so the new domain takes effect. Best-effort: errors are
/// logged but not propagated.
async fn reconcile_for_environment(state: &AppState, project_name: &str, env: &Environment) {
    let group = match env.primary_deployment_group.as_deref() {
        Some(g) => g,
        None => {
            info!(
                "Environment '{}' for project '{}' has no primary deployment group; skipping reconcile",
                env.name, project_name
            );
            return;
        }
    };
    match db_deployments::find_active_for_project_and_group(&state.db_pool, env.project_id, group)
        .await
    {
        Ok(Some(active_deployment)) => {
            if let Err(e) =
                db_deployments::mark_needs_reconcile(&state.db_pool, active_deployment.id).await
            {
                info!(
                    "Failed to mark deployment {} for reconcile after custom-domain change: {:?}",
                    active_deployment.deployment_id, e
                );
            }
        }
        Ok(None) => {
            info!(
                "No active deployment in group '{}' for project '{}' — domain change recorded but nothing to reconcile",
                group, project_name
            );
        }
        Err(e) => {
            info!(
                "Failed to find active deployment for project '{}' group '{}': {:?}",
                project_name, group, e
            );
        }
    }
}

/// Resolve and load the environment row that owns `domain` so we can reconcile
/// the right deployment group when the domain changes.
async fn env_for_domain(pool: &PgPool, domain: &CustomDomain) -> Option<Environment> {
    db_environments::find_by_id(pool, domain.environment_id)
        .await
        .ok()
        .flatten()
}

/// Add a custom domain to a project
pub async fn add_custom_domain(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(project_id_or_name): Path<String>,
    Json(payload): Json<AddCustomDomainRequest>,
) -> Result<(StatusCode, Json<CustomDomainResponse>), ServerError> {
    // Find project by ID or name
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

    // Validate that the custom domain doesn't overlap with project default domain patterns
    if let Some(ref production_template) = state.production_ingress_url_template {
        if let Err(reason) = validation::validate_custom_domain(
            &payload.domain,
            production_template,
            state.staging_ingress_url_template.as_deref(),
            state.environment_ingress_url_template.as_deref(),
            Some(&state.public_url),
        ) {
            return Err(ServerError::bad_request(reason));
        }
    }

    let environment =
        resolve_environment(&state.db_pool, project.id, payload.environment.as_deref()).await?;

    let domain = db_custom_domains::add_custom_domain(
        &state.db_pool,
        project.id,
        environment.id,
        &payload.domain,
    )
    .await
    .map_err(|e| {
        let error_message = e.to_string();
        if error_message.contains("duplicate key") || error_message.contains("unique constraint") {
            ServerError::conflict(format!("Domain '{}' is already in use", payload.domain))
        } else if error_message.contains("check constraint") {
            ServerError::bad_request(format!("Invalid domain format: {}", payload.domain))
        } else {
            ServerError::internal_anyhow(e, "Failed to add custom domain")
        }
    })?;

    reconcile_for_environment(&state, &project.name, &environment).await;

    Ok((
        StatusCode::CREATED,
        Json(CustomDomainResponse::from_db_model(
            &domain,
            &environment.name,
        )),
    ))
}

/// List all custom domains for a project
pub async fn list_custom_domains(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(project_id_or_name): Path<String>,
) -> Result<Json<CustomDomainsResponse>, ServerError> {
    // Find project by ID or name
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
    ensure_project_access_or_admin(&state, user, &project)
        .await
        .map_err(|e| {
            if e.status == StatusCode::FORBIDDEN {
                ServerError::not_found("Project not found")
            } else {
                e
            }
        })?;

    // Get all custom domains for the project plus the envs they belong to.
    let domains = db_custom_domains::list_project_custom_domains(&state.db_pool, project.id)
        .await
        .internal_err("Failed to list custom domains")?;
    let environments: std::collections::HashMap<Uuid, Environment> =
        db_environments::list_for_project(&state.db_pool, project.id)
            .await
            .internal_err("Failed to load environments")?
            .into_iter()
            .map(|e| (e.id, e))
            .collect();

    let unknown_env = String::from("?");
    Ok(Json(CustomDomainsResponse {
        domains: domains
            .iter()
            .map(|d| {
                let env_name = environments
                    .get(&d.environment_id)
                    .map(|e| e.name.as_str())
                    .unwrap_or(&unknown_env);
                CustomDomainResponse::from_db_model(d, env_name)
            })
            .collect(),
    }))
}

/// Get a specific custom domain
pub async fn get_custom_domain(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((project_id_or_name, domain)): Path<(String, String)>,
) -> Result<Json<CustomDomainResponse>, ServerError> {
    // Find project by ID or name
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
    ensure_project_access_or_admin(&state, user, &project)
        .await
        .map_err(|e| {
            if e.status == StatusCode::FORBIDDEN {
                ServerError::not_found("Project not found")
            } else {
                e
            }
        })?;

    // Get the custom domain
    let domain = db_custom_domains::get_custom_domain(&state.db_pool, project.id, &domain)
        .await
        .internal_err("Failed to get custom domain")?
        .ok_or_else(|| ServerError::not_found("Custom domain not found"))?;
    let env_name = environment_name_for(&state.db_pool, domain.environment_id).await;

    Ok(Json(CustomDomainResponse::from_db_model(
        &domain, &env_name,
    )))
}

/// Delete a custom domain
pub async fn delete_custom_domain(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((project_id_or_name, domain)): Path<(String, String)>,
) -> Result<StatusCode, ServerError> {
    // Find project by ID or name
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

    // Look up the domain first so we know which environment to reconcile after deletion.
    let existing = db_custom_domains::get_custom_domain(&state.db_pool, project.id, &domain)
        .await
        .internal_err("Failed to look up custom domain")?
        .ok_or_else(|| ServerError::not_found("Custom domain not found"))?;
    let env_for_reconcile = env_for_domain(&state.db_pool, &existing).await;

    let deleted = db_custom_domains::delete_custom_domain(&state.db_pool, project.id, &domain)
        .await
        .internal_err("Failed to delete custom domain")?;

    if !deleted {
        return Err(ServerError::not_found("Custom domain not found"));
    }

    if let Some(env) = env_for_reconcile {
        reconcile_for_environment(&state, &project.name, &env).await;
    }

    Ok(StatusCode::NO_CONTENT)
}

/// Reassign a custom domain to a different environment.
pub async fn update_custom_domain(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((project_id_or_name, domain)): Path<(String, String)>,
    Json(payload): Json<UpdateCustomDomainRequest>,
) -> Result<Json<CustomDomainResponse>, ServerError> {
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

    let existing = db_custom_domains::get_custom_domain(&state.db_pool, project.id, &domain)
        .await
        .internal_err("Failed to look up custom domain")?
        .ok_or_else(|| ServerError::not_found("Custom domain not found"))?;

    let new_env = resolve_environment(
        &state.db_pool,
        project.id,
        Some(payload.environment.as_str()),
    )
    .await?;

    // No-op when the domain already lives on the requested env.
    if existing.environment_id == new_env.id {
        return Ok(Json(CustomDomainResponse::from_db_model(
            &existing,
            &new_env.name,
        )));
    }

    let old_env = env_for_domain(&state.db_pool, &existing).await;

    let updated = db_custom_domains::update_custom_domain_environment(
        &state.db_pool,
        project.id,
        &domain,
        new_env.id,
    )
    .await
    .internal_err("Failed to move custom domain to new environment")?;

    // Reconcile both the source and the destination so the domain disappears
    // from the old group's ingress and appears on the new one.
    if let Some(env) = old_env {
        reconcile_for_environment(&state, &project.name, &env).await;
    }
    reconcile_for_environment(&state, &project.name, &new_env).await;

    Ok(Json(CustomDomainResponse::from_db_model(
        &updated,
        &new_env.name,
    )))
}

/// Set a custom domain as primary for a project
pub async fn set_primary_domain(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((project_id_or_name, domain)): Path<(String, String)>,
) -> Result<Json<CustomDomainResponse>, ServerError> {
    // Find project by ID or name
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

    // Set the domain as primary
    let updated_domain = db_custom_domains::set_primary_domain(&state.db_pool, project.id, &domain)
        .await
        .map_err(|e| {
            let error_message = e.to_string();
            if error_message.contains("no rows") {
                ServerError::not_found("Custom domain not found")
            } else {
                ServerError::internal_anyhow(e, "Failed to set primary domain")
            }
        })?;

    let env_name = if let Some(env) = env_for_domain(&state.db_pool, &updated_domain).await {
        reconcile_for_environment(&state, &project.name, &env).await;
        env.name
    } else {
        "?".to_string()
    };

    Ok(Json(CustomDomainResponse::from_db_model(
        &updated_domain,
        &env_name,
    )))
}

/// Unset the primary status of a custom domain
pub async fn unset_primary_domain(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((project_id_or_name, domain)): Path<(String, String)>,
) -> Result<StatusCode, ServerError> {
    // Find project by ID or name
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

    // Look up the domain first to know which environment owns it.
    let existing = db_custom_domains::get_custom_domain(&state.db_pool, project.id, &domain)
        .await
        .internal_err("Failed to look up custom domain")?
        .ok_or_else(|| ServerError::not_found("Custom domain not found"))?;
    let env_for_reconcile = env_for_domain(&state.db_pool, &existing).await;

    let unset = db_custom_domains::unset_primary_domain(&state.db_pool, project.id, &domain)
        .await
        .internal_err("Failed to unset primary domain")?;

    if !unset {
        return Err(ServerError::not_found(
            "Custom domain not found or is not primary",
        ));
    }

    if let Some(env) = env_for_reconcile {
        reconcile_for_environment(&state, &project.name, &env).await;
    }

    Ok(StatusCode::NO_CONTENT)
}
