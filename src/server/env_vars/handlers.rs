use super::models::{
    EnvVarResponse, EnvVarValueResponse, EnvVarsResponse, MoveEnvVarRequest, SetEnvVarRequest,
};
use crate::db::{env_vars as db_env_vars, environments as db_environments, projects};
use crate::server::auth::context::AuthContext;
use crate::server::deployment::models as deployment_models;
use crate::server::error::{ServerError, ServerErrorExt};
use crate::server::extensions::InjectedEnvVarValue;
use crate::server::project::handlers::ensure_project_access_or_admin;
use crate::server::state::AppState;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use std::collections::HashMap;

/// Validate an environment variable key from a URL path segment.
///
/// Keys must match `[-._a-zA-Z0-9]+` (the same pattern Kubernetes enforces for
/// Secret data keys).  Leading/trailing whitespace is rejected rather than
/// silently normalised — if the key contains whitespace the caller likely made
/// a mistake and should fix the request.
///
// TODO(#344): reserve the `RISE_` prefix here so a user-supplied env var can't
// collide with an auto-injected `RISE_CONTAINER_HOST__<NAME>` (which, as an
// explicit pod env, would silently shadow a same-named secret).
fn validate_env_var_key(key: &str) -> Result<(), ServerError> {
    if key != key.trim() {
        return Err(ServerError::bad_request(format!(
            "Invalid environment variable key {:?}: leading/trailing whitespace is not allowed",
            key
        )));
    }
    if key.is_empty()
        || !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        return Err(ServerError::bad_request(format!(
            "Invalid environment variable key {:?}: must consist of alphanumeric characters, '-', '_', or '.'",
            key
        )));
    }
    Ok(())
}

/// Resolve an optional environment name from query params to an environment ID.
async fn resolve_environment_id(
    pool: &sqlx::PgPool,
    project_id: uuid::Uuid,
    params: &HashMap<String, String>,
) -> Result<Option<uuid::Uuid>, ServerError> {
    if let Some(env_name) = params.get("environment") {
        let env = db_environments::find_by_name(pool, project_id, env_name)
            .await
            .internal_err("Failed to find environment")?
            .ok_or_else(|| {
                ServerError::not_found(format!("Environment '{}' not found", env_name))
            })?;
        Ok(Some(env.id))
    } else {
        Ok(None)
    }
}

/// Set or update a project environment variable
pub async fn set_project_env_var(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((project_id_or_name, key)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
    Json(payload): Json<SetEnvVarRequest>,
) -> Result<Json<EnvVarResponse>, ServerError> {
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
    validate_env_var_key(&key)?;

    // Normalize: when is_protected is omitted, infer from is_secret
    // This preserves backward compatibility: secrets default to protected, plain vars default to unprotected
    let is_protected = payload.is_protected.unwrap_or(payload.is_secret);

    // Validate: is_protected requires is_secret (non-secrets cannot be "protected")
    if is_protected && !payload.is_secret {
        return Err(ServerError::bad_request(
            "Non-secret variables cannot be protected. Protection only applies to secrets.",
        ));
    }

    // IMPORTANT: If this is a secret, we must have an encryption provider
    if payload.is_secret && state.encryption_provider.is_none() {
        return Err(ServerError::bad_request(
            "Cannot store secret variables: no encryption provider configured",
        ));
    }

    // Encrypt the value if it's a secret
    let value_to_store = if payload.is_secret {
        let provider = state
            .encryption_provider
            .as_ref()
            .expect("Encryption provider checked above");

        provider
            .encrypt(&payload.value)
            .await
            .internal_err("Failed to encrypt secret")?
    } else {
        payload.value.clone()
    };

    // Resolve environment from query parameter
    let environment_id = resolve_environment_id(&state.db_pool, project.id, &params).await?;

    let env_var = db_env_vars::upsert_project_env_var(
        &state.db_pool,
        project.id,
        &key,
        &value_to_store,
        payload.is_secret,
        is_protected,
        environment_id,
    )
    .await
    .internal_err("Failed to store environment variable")?;

    tracing::info!(
        "Set environment variable '{}' for project '{}' (secret: {}, protected: {}). This will apply to new deployments only.",
        key,
        project.name,
        payload.is_secret,
        is_protected
    );

    // Note: Environment variables are snapshots at deployment time.
    // Changing project env vars does not affect existing deployments.
    // New deployments will use the updated values.

    // Return masked response
    let mut response = EnvVarResponse::from_db_model(
        env_var.key,
        env_var.value,
        env_var.is_secret,
        env_var.is_protected,
        None,
    );
    response.environment = params.get("environment").cloned();
    Ok(Json(response))
}

/// List all environment variables for a project
pub async fn list_project_env_vars(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(project_id_or_name): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<EnvVarsResponse>, ServerError> {
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

    // Check if we should include unprotected values
    let include_unprotected = params
        .get("include_unprotected_values")
        .map(|v| v == "true")
        .unwrap_or(false);

    // Resolve environment from query parameter
    let environment_id = resolve_environment_id(&state.db_pool, project.id, &params).await?;

    let db_env_vars =
        db_env_vars::list_project_env_vars(&state.db_pool, project.id, environment_id)
            .await
            .internal_err("Failed to list environment variables")?;

    // Build environment ID -> name lookup
    let environments = db_environments::list_for_project(&state.db_pool, project.id)
        .await
        .internal_err("Failed to list environments")?;
    let env_name_map: HashMap<uuid::Uuid, String> =
        environments.into_iter().map(|e| (e.id, e.name)).collect();

    // Convert to API response
    let mut env_vars = Vec::new();
    for var in db_env_vars {
        let value = if include_unprotected && var.is_secret && !var.is_protected {
            // Decrypt unprotected secret
            match &state.encryption_provider {
                Some(provider) => provider
                    .decrypt(&var.value)
                    .await
                    .internal_err("Failed to decrypt secret")?,
                None => {
                    return Err(ServerError::internal(
                        "Cannot decrypt secrets: no encryption provider configured",
                    ))
                }
            }
        } else {
            var.value.clone()
        };

        let environment = var
            .environment_id
            .and_then(|id| env_name_map.get(&id).cloned());

        let mut response = if var.is_secret && (!include_unprotected || var.is_protected) {
            // Mask protected secrets
            EnvVarResponse::from_db_model(var.key, var.value, var.is_secret, var.is_protected, None)
        } else {
            // Return plaintext or decrypted value
            EnvVarResponse {
                key: var.key,
                value,
                is_secret: var.is_secret,
                is_protected: var.is_protected,
                environment: None,
                source: None,
            }
        };
        response.environment = environment;
        env_vars.push(response);
    }

    Ok(Json(EnvVarsResponse { env_vars }))
}

/// Delete a project environment variable
pub async fn delete_project_env_var(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((project_id_or_name, key)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
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
    validate_env_var_key(&key)?;

    // Resolve environment from query parameter
    let environment_id = resolve_environment_id(&state.db_pool, project.id, &params).await?;

    let deleted =
        db_env_vars::delete_project_env_var(&state.db_pool, project.id, &key, environment_id)
            .await
            .internal_err("Failed to delete environment variable")?;

    if !deleted {
        return Err(ServerError::not_found(format!(
            "Environment variable '{}' not found",
            key
        )));
    }

    tracing::info!(
        "Deleted environment variable '{}' from project '{}'. This will apply to new deployments only.",
        key,
        project.name
    );

    // Note: Environment variables are snapshots at deployment time.
    // Deleting project env vars does not affect existing deployments.
    // New deployments will not have the deleted variable.

    Ok(StatusCode::NO_CONTENT)
}

/// Move a project environment variable to a different environment
pub async fn move_project_env_var(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((project_id_or_name, key)): Path<(String, String)>,
    Json(payload): Json<MoveEnvVarRequest>,
) -> Result<Json<EnvVarResponse>, ServerError> {
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
    validate_env_var_key(&key)?;

    // Resolve source environment
    let from_env_id = if let Some(ref name) = payload.from_environment {
        let env = db_environments::find_by_name(&state.db_pool, project.id, name)
            .await
            .internal_err("Failed to find source environment")?
            .ok_or_else(|| {
                ServerError::not_found(format!("Source environment '{}' not found", name))
            })?;
        Some(env.id)
    } else {
        None
    };

    // Resolve target environment
    let to_env_id = if let Some(ref name) = payload.to_environment {
        let env = db_environments::find_by_name(&state.db_pool, project.id, name)
            .await
            .internal_err("Failed to find target environment")?
            .ok_or_else(|| {
                ServerError::not_found(format!("Target environment '{}' not found", name))
            })?;
        Some(env.id)
    } else {
        None
    };

    // Check source env var exists
    let existing_source =
        db_env_vars::get_project_env_var(&state.db_pool, project.id, &key, from_env_id)
            .await
            .internal_err("Failed to check source environment")?;
    if existing_source.is_none() {
        let source_label = payload.from_environment.as_deref().unwrap_or("global");
        return Err(ServerError::not_found(format!(
            "Environment variable '{}' not found in environment '{}'",
            key, source_label
        )));
    }

    // Check for conflict at the target environment
    let existing_at_target =
        db_env_vars::get_project_env_var(&state.db_pool, project.id, &key, to_env_id)
            .await
            .internal_err("Failed to check target environment")?;
    if existing_at_target.is_some() {
        let target_label = payload.to_environment.as_deref().unwrap_or("global");
        return Err(ServerError::bad_request(format!(
            "Environment variable '{}' already exists in environment '{}'",
            key, target_label
        )));
    }

    let env_var = db_env_vars::update_env_var_environment(
        &state.db_pool,
        project.id,
        &key,
        from_env_id,
        to_env_id,
    )
    .await
    .internal_err("Failed to move environment variable")?;

    tracing::info!(
        "Moved environment variable '{}' for project '{}' from {:?} to {:?}",
        key,
        project.name,
        payload.from_environment,
        payload.to_environment
    );

    let mut response = EnvVarResponse::from_db_model(
        env_var.key,
        env_var.value,
        env_var.is_secret,
        env_var.is_protected,
        None,
    );
    response.environment = payload.to_environment;
    Ok(Json(response))
}

/// List all environment variables for a deployment (read-only)
pub async fn list_deployment_env_vars(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((project_id_or_name, deployment_id)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<EnvVarsResponse>, ServerError> {
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

    // Get deployment by deployment_id within the project
    let deployment =
        crate::db::deployments::find_by_deployment_id(&state.db_pool, &deployment_id, project.id)
            .await
            .internal_err("Failed to get deployment")?
            .ok_or_else(|| ServerError::not_found("Deployment not found"))?;

    // Check if we should include unprotected values
    let include_unprotected = params
        .get("include_unprotected_values")
        .map(|v| v == "true")
        .unwrap_or(false);

    // Get all deployment environment variables
    let db_env_vars = db_env_vars::list_deployment_env_vars(&state.db_pool, deployment.id)
        .await
        .internal_err("Failed to list deployment environment variables")?;

    // Convert to API response
    let mut env_vars = Vec::new();
    for var in db_env_vars {
        let value = if include_unprotected && var.is_secret && !var.is_protected {
            // Decrypt unprotected secret
            match &state.encryption_provider {
                Some(provider) => provider
                    .decrypt(&var.value)
                    .await
                    .internal_err("Failed to decrypt secret")?,
                None => {
                    return Err(ServerError::internal(
                        "Cannot decrypt secrets: no encryption provider configured",
                    ))
                }
            }
        } else {
            var.value.clone()
        };

        env_vars.push(
            if var.is_secret && (!include_unprotected || var.is_protected) {
                // Mask protected secrets
                EnvVarResponse::from_db_model(
                    var.key,
                    var.value,
                    var.is_secret,
                    var.is_protected,
                    Some(var.source),
                )
            } else {
                // Return plaintext or decrypted value
                EnvVarResponse {
                    key: var.key,
                    value,
                    is_secret: var.is_secret,
                    is_protected: var.is_protected,
                    environment: None,
                    source: Some(var.source),
                }
            },
        );
    }

    Ok(Json(EnvVarsResponse { env_vars }))
}

/// Get the decrypted value of a specific retrievable secret
pub async fn get_project_env_var_value(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((project_id_or_name, key)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<EnvVarValueResponse>, ServerError> {
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
    validate_env_var_key(&key)?;

    // Resolve environment from query parameter
    let environment_id = resolve_environment_id(&state.db_pool, project.id, &params).await?;

    let env_var =
        db_env_vars::get_project_env_var(&state.db_pool, project.id, &key, environment_id)
            .await
            .internal_err("Failed to get environment variable")?
            .ok_or_else(|| {
                ServerError::not_found(format!("Environment variable '{}' not found", key))
            })?;

    // Validate: must be an unprotected secret
    if !env_var.is_secret || env_var.is_protected {
        return Err(ServerError::bad_request(format!(
            "Environment variable '{}' is a protected secret and cannot be retrieved. \
             Update it with --protected=false to allow retrieval.",
            key
        )));
    }

    // Decrypt the value
    let decrypted_value = match &state.encryption_provider {
        Some(provider) => provider
            .decrypt(&env_var.value)
            .await
            .internal_err("Failed to decrypt secret")?,
        None => {
            return Err(ServerError::internal(
                "Cannot decrypt secrets: no encryption provider configured",
            ))
        }
    };

    tracing::info!(
        "Retrieved decrypted value for secret '{}' in project '{}' by user '{}'",
        key,
        project.name,
        user.email
    );

    Ok(Json(EnvVarValueResponse {
        value: decrypted_value,
    }))
}

/// Get the decrypted value of a specific retrievable deployment secret
pub async fn get_deployment_env_var_value(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((project_id_or_name, deployment_id, key)): Path<(String, String, String)>,
) -> Result<Json<EnvVarValueResponse>, ServerError> {
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

    // Get deployment by deployment_id within the project
    let deployment =
        crate::db::deployments::find_by_deployment_id(&state.db_pool, &deployment_id, project.id)
            .await
            .internal_err("Failed to get deployment")?
            .ok_or_else(|| ServerError::not_found("Deployment not found"))?;

    // Get the specific environment variable
    let env_var = db_env_vars::get_deployment_env_var(&state.db_pool, deployment.id, &key)
        .await
        .internal_err("Failed to get environment variable")?
        .ok_or_else(|| {
            ServerError::not_found(format!("Environment variable '{}' not found", key))
        })?;

    // Validate: must be an unprotected secret
    if !env_var.is_secret || env_var.is_protected {
        return Err(ServerError::bad_request(format!(
            "Environment variable '{}' is a protected secret and cannot be retrieved. \
             Update it with --protected=false to allow retrieval.",
            key
        )));
    }

    // Decrypt the value
    let decrypted_value = match &state.encryption_provider {
        Some(provider) => provider
            .decrypt(&env_var.value)
            .await
            .internal_err("Failed to decrypt secret")?,
        None => {
            return Err(ServerError::internal(
                "Cannot decrypt secrets: no encryption provider configured",
            ))
        }
    };

    tracing::info!(
        "Retrieved decrypted value for secret '{}' in deployment '{}' by user '{}'",
        key,
        deployment.deployment_id,
        user.email
    );

    Ok(Json(EnvVarValueResponse {
        value: decrypted_value,
    }))
}

/// Preview the full set of environment variables a deployment would receive.
///
/// Returns:
/// - User-set environment variables
/// - System vars: PORT plus those from [`deployment_models::rise_system_env_vars`]
/// - Extension-injected vars
///
/// Protected vars are masked. This enables `rise run` to inject the same env vars as a real deployment.
pub async fn preview_deployment_env_vars(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(project_id_or_name): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<EnvVarsResponse>, ServerError> {
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

    let deployment_group = params
        .get("deployment_group")
        .cloned()
        .unwrap_or_else(|| "default".to_string());

    // Collect all env vars into a map (later entries override earlier ones)
    let mut env_map: HashMap<String, EnvVarResponse> = HashMap::new();

    // 1. Load user-set project vars (resolve environment from query param)
    let preview_env_id = resolve_environment_id(&state.db_pool, project.id, &params).await?;
    let preview_env_name = params.get("environment").cloned();
    let env_source = preview_env_name
        .as_ref()
        .map(|name| format!("env:{}", name));

    let db_vars = db_env_vars::list_project_env_vars(&state.db_pool, project.id, preview_env_id)
        .await
        .internal_err("Failed to list environment variables")?;

    for var in db_vars {
        // Determine source: global if no environment_id, env:<name> if scoped
        let source = Some(if var.environment_id.is_some() {
            env_source.clone().unwrap_or_else(|| "global".to_string())
        } else {
            "global".to_string()
        });

        if var.is_secret && !var.is_protected {
            // Unprotected secret — decrypt for preview
            let decrypted = match &state.encryption_provider {
                Some(provider) => provider
                    .decrypt(&var.value)
                    .await
                    .internal_err("Failed to decrypt secret")?,
                None => {
                    return Err(ServerError::internal(
                        "Cannot decrypt secrets: no encryption provider configured",
                    ))
                }
            };
            env_map.insert(
                var.key.clone(),
                EnvVarResponse {
                    key: var.key,
                    value: decrypted,
                    is_secret: true,
                    is_protected: false,
                    environment: None,
                    source,
                },
            );
        } else if var.is_secret {
            // Protected secret — mask
            env_map.insert(
                var.key.clone(),
                EnvVarResponse {
                    key: var.key,
                    value: "••••••••".to_string(),
                    is_secret: true,
                    is_protected: true,
                    environment: None,
                    source,
                },
            );
        } else {
            // Plain var
            env_map.insert(
                var.key.clone(),
                EnvVarResponse {
                    key: var.key.clone(),
                    value: var.value,
                    is_secret: false,
                    is_protected: false,
                    environment: None,
                    source,
                },
            );
        }
    }

    // 2. Add system vars
    if !env_map.contains_key("PORT") {
        env_map.insert(
            "PORT".to_string(),
            EnvVarResponse {
                key: "PORT".to_string(),
                value: "8080".to_string(),
                is_secret: false,
                is_protected: false,
                environment: None,
                source: Some("system".to_string()),
            },
        );
    }

    // System vars from rise_system_env_vars() — requires deployment URLs from the backend.
    // When URLs are unavailable (e.g. no deployment controller configured), fall back to
    // inserting only the URL-independent vars (RISE_ISSUER, RISE_DEPLOYMENT_GROUP*).
    match state
        .deployment_backend
        .get_project_urls(&project, &deployment_group)
        .await
    {
        Ok(urls) => {
            for (key, value) in deployment_models::rise_system_env_vars(
                &state.public_url,
                &deployment_group,
                &urls,
                params.get("environment").map(|s| s.as_str()),
            ) {
                env_map.insert(
                    key.clone(),
                    EnvVarResponse {
                        key,
                        value,
                        is_secret: false,
                        is_protected: false,
                        environment: None,
                        source: Some("system".to_string()),
                    },
                );
            }
        }
        Err(e) => {
            tracing::debug!(
                "Could not compute project URLs for preview (no deployment controller?): {:?}",
                e
            );
            // Insert URL-independent system vars only
            for (key, value) in [
                ("RISE_ISSUER", state.public_url.clone()),
                ("RISE_DEPLOYMENT_GROUP", deployment_group.clone()),
                (
                    "RISE_DEPLOYMENT_GROUP_NORMALIZED",
                    deployment_models::normalize_deployment_group(&deployment_group),
                ),
            ] {
                env_map.insert(
                    key.to_string(),
                    EnvVarResponse {
                        key: key.to_string(),
                        value,
                        is_secret: false,
                        is_protected: false,
                        environment: None,
                        source: Some("system".to_string()),
                    },
                );
            }
        }
    }

    // 3. Collect extension env vars
    for (_, extension) in state.extension_registry.iter() {
        match extension
            .preview_env_vars(project.id, &deployment_group)
            .await
        {
            Ok(vars) => {
                for var in vars {
                    let response = match var.value {
                        InjectedEnvVarValue::Plain(v) => EnvVarResponse {
                            key: var.key.clone(),
                            value: v,
                            is_secret: false,
                            is_protected: false,
                            environment: None,
                            source: Some("extension".to_string()),
                        },
                        InjectedEnvVarValue::Secret { decrypted, .. } => EnvVarResponse {
                            key: var.key.clone(),
                            value: decrypted,
                            is_secret: true,
                            is_protected: false,
                            environment: None,
                            source: Some("extension".to_string()),
                        },
                        InjectedEnvVarValue::Protected { .. } => EnvVarResponse {
                            key: var.key.clone(),
                            value: "••••••••".to_string(),
                            is_secret: true,
                            is_protected: true,
                            environment: None,
                            source: Some("extension".to_string()),
                        },
                    };
                    // Extension vars override user vars for the same key
                    env_map.insert(var.key, response);
                }
            }
            Err(e) => {
                tracing::warn!(
                    "Extension '{}' failed to provide preview env vars: {:?}",
                    extension.extension_type(),
                    e
                );
            }
        }
    }

    // Convert to sorted vec
    let mut env_vars: Vec<EnvVarResponse> = env_map.into_values().collect();
    env_vars.sort_by(|a, b| a.key.cmp(&b.key));

    Ok(Json(EnvVarsResponse { env_vars }))
}
