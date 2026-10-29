use super::models::{
    EnvVarResponse, EnvVarValueResponse, EnvVarsResponse, MoveEnvVarRequest, SetEnvVarRequest,
};
use crate::db::{env_vars as db_env_vars, environments as db_environments, projects};
use crate::server::auth::context::AuthContext;
use crate::server::auth::session_scope::{MaybeSessionScope, SessionScope};
use crate::server::deployment::models as deployment_models;
use crate::server::encryption::EncryptionContext;
use crate::server::error::{ServerError, ServerErrorExt};
use crate::server::extensions::InjectedEnvVarValue;
use crate::server::project::handlers::ensure_project_access_or_admin;
use crate::server::state::AppState;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use rise_backend_auth::session_scope::{Kind, Operation, Subresource, Verb};
use std::collections::HashMap;

/// Validate an environment variable key from a URL path segment.
///
/// Keys must match `[-._a-zA-Z0-9]+` (the same pattern Kubernetes enforces for
/// Secret data keys).  Leading/trailing whitespace is rejected rather than
/// silently normalised — if the key contains whitespace the caller likely made
/// a mistake and should fix the request.
///
/// The `RISE_` prefix is reserved for Rise-injected pod env vars (e.g.
/// `RISE_APP_URL`, `RISE_DEPLOYMENT_GROUP`, `RISE_CONTAINER_HOST__<NAME>`).
/// Those are set as explicit pod `env` and would silently shadow a same-named
/// secret, so user-supplied keys in that namespace are rejected outright.
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
    if deployment_models::is_reserved_env_var_key(key) {
        return Err(ServerError::bad_request(format!(
            "Invalid environment variable key {:?}: the '{}' prefix is reserved for Rise-injected variables",
            key,
            deployment_models::RESERVED_ENV_VAR_PREFIX,
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

    let environment_id = resolve_environment_id(&state.db_pool, project.id, &params).await?;

    // Encrypt the value if it's a secret
    let value_to_store = if payload.is_secret {
        let provider = state
            .encryption_provider
            .as_ref()
            .expect("Encryption provider checked above");

        provider
            .encrypt(
                &payload.value,
                &EncryptionContext::project_env(project.id, environment_id, &key),
            )
            .await
            .internal_err("Failed to encrypt secret")?
    } else {
        payload.value.clone()
    };

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
    MaybeSessionScope(scope): MaybeSessionScope,
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

    let visibility = VarVisibility {
        scope: scope.as_ref(),
        project: &project.name,
    };
    visibility.require_any()?;

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
        let environment = var
            .environment_id
            .and_then(|id| env_name_map.get(&id).cloned());
        if !visibility.lists(environment.as_deref()) {
            continue;
        }
        let include_unprotected = include_unprotected && visibility.reveals(environment.as_deref());
        let value = if include_unprotected && var.is_secret && !var.is_protected {
            // Decrypt unprotected secret
            match &state.encryption_provider {
                Some(provider) => provider
                    .decrypt(
                        &var.value,
                        &EncryptionContext::project_env(
                            var.project_id,
                            var.environment_id,
                            &var.key,
                        ),
                    )
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

/// What a scoped session may see of a project's variables (ADR-0006 §3): a
/// variable bound to an environment needs a grant there, a project-wide one a
/// project grant, and a secret's value additionally `secrets`.
struct VarVisibility<'a> {
    scope: Option<&'a SessionScope>,
    project: &'a str,
}

impl VarVisibility<'_> {
    const LIST: Operation = Operation::new(Verb::List, Kind::EnvironmentVariable);
    const READ_VALUE: Operation =
        Operation::sub(Verb::Get, Kind::EnvironmentVariable, Subresource::Value);

    /// Refuse a session that can list no variable in the project at all.
    fn require_any(&self) -> Result<(), ServerError> {
        match self.scope {
            Some(scope) => scope.require_reach(Self::LIST, self.project),
            None => Ok(()),
        }
    }

    fn lists(&self, environment: Option<&str>) -> bool {
        self.scope
            .is_none_or(|scope| scope.allows(Self::LIST, self.project, environment))
    }

    fn reveals(&self, environment: Option<&str>) -> bool {
        self.scope
            .is_none_or(|scope| scope.allows(Self::READ_VALUE, self.project, environment))
    }
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

    let source = existing_source.expect("source variable checked above");
    let original_value = source.value.clone();
    let moved_value = if source.is_secret {
        let provider = state
            .encryption_provider
            .as_ref()
            .ok_or_else(|| ServerError::internal("Encryption provider not configured"))?;
        let plaintext = provider
            .decrypt(
                &source.value,
                &EncryptionContext::project_env(project.id, from_env_id, &key),
            )
            .await
            .internal_err("Failed to decrypt variable for move")?;
        provider
            .encrypt(
                &plaintext,
                &EncryptionContext::project_env(project.id, to_env_id, &key),
            )
            .await
            .internal_err("Failed to encrypt moved variable")?
    } else {
        source.value
    };

    let env_var = db_env_vars::update_env_var_environment(
        &state.db_pool,
        project.id,
        &key,
        from_env_id,
        to_env_id,
        &moved_value,
        &original_value,
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
                    .decrypt(
                        &var.value,
                        &EncryptionContext::deployment_env(var.deployment_id, &var.key),
                    )
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
             Update it with --secret to allow retrieval.",
            key
        )));
    }

    // Decrypt the value
    let decrypted_value = match &state.encryption_provider {
        Some(provider) => provider
            .decrypt(
                &env_var.value,
                &EncryptionContext::project_env(
                    env_var.project_id,
                    env_var.environment_id,
                    &env_var.key,
                ),
            )
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
             Update it with --secret to allow retrieval.",
            key
        )));
    }

    // Decrypt the value
    let decrypted_value = match &state.encryption_provider {
        Some(provider) => provider
            .decrypt(
                &env_var.value,
                &EncryptionContext::deployment_env(env_var.deployment_id, &env_var.key),
            )
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
    MaybeSessionScope(scope): MaybeSessionScope,
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

    let visibility = VarVisibility {
        scope: scope.as_ref(),
        project: &project.name,
    };
    visibility.require_any()?;

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
        let environment = var.environment_id.and(preview_env_name.as_deref());
        if !visibility.lists(environment) {
            continue;
        }

        if var.is_secret && !var.is_protected && visibility.reveals(environment) {
            // Unprotected secret — decrypt for preview
            let decrypted = match &state.encryption_provider {
                Some(provider) => provider
                    .decrypt(
                        &var.value,
                        &EncryptionContext::project_env(
                            var.project_id,
                            var.environment_id,
                            &var.key,
                        ),
                    )
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
            // Protected secret, or one this session may not read — mask
            env_map.insert(
                var.key.clone(),
                EnvVarResponse {
                    key: var.key,
                    value: "••••••••".to_string(),
                    is_secret: true,
                    is_protected: var.is_protected,
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

#[cfg(test)]
mod visibility_tests {
    use super::VarVisibility;
    use crate::server::auth::session_scope::tests::scope;
    use serde_json::json;

    #[test]
    fn an_environment_grant_sees_only_its_own_variables() {
        let staging = scope(json!({
            "kind": "restricted",
            "grants": [{"project": "app", "environment": "staging", "preset": "deploy"}],
        }));
        let visibility = VarVisibility {
            scope: Some(&staging),
            project: "app",
        };
        assert!(visibility.require_any().is_ok());
        assert!(visibility.lists(Some("staging")));
        assert!(!visibility.lists(Some("production")));
        // Project-wide variables apply to every environment.
        assert!(!visibility.lists(None));
        // `deploy` doesn't include `secrets`.
        assert!(!visibility.reveals(Some("staging")));
    }

    #[test]
    fn secrets_reveal_values_only_where_granted() {
        let staging = scope(json!({
            "kind": "restricted",
            "grants": [{"project": "app", "environment": "staging", "permissions": ["view", "secrets"]}],
        }));
        let visibility = VarVisibility {
            scope: Some(&staging),
            project: "app",
        };
        assert!(visibility.reveals(Some("staging")));
        assert!(!visibility.reveals(None));

        let project = scope(json!({
            "kind": "restricted",
            "grants": [{"project": "app", "preset": "read"}],
        }));
        let visibility = VarVisibility {
            scope: Some(&project),
            project: "app",
        };
        assert!(visibility.lists(None) && visibility.lists(Some("staging")));
        assert!(!visibility.reveals(None));
    }

    #[test]
    fn an_unrestricted_session_sees_everything() {
        let visibility = VarVisibility {
            scope: None,
            project: "app",
        };
        assert!(visibility.lists(None) && visibility.reveals(Some("staging")));
    }
}

#[cfg(test)]
mod tests {
    use super::validate_env_var_key;

    #[test]
    fn accepts_valid_keys() {
        for key in ["FOO", "MY_VAR", "foo.bar", "a-b_c.1", "rise_foo"] {
            assert!(
                validate_env_var_key(key).is_ok(),
                "expected {key:?} to be accepted"
            );
        }
    }

    #[test]
    fn rejects_reserved_rise_prefix() {
        for key in [
            "RISE_FOO",
            "RISE_",
            "RISE_APP_URL",
            "RISE_CONTAINER_HOST__API",
        ] {
            let err =
                validate_env_var_key(key).expect_err(&format!("expected {key:?} to be rejected"));
            assert!(
                err.message.contains("reserved"),
                "unexpected error for {key:?}: {}",
                err.message
            );
        }
    }

    #[test]
    fn rejects_invalid_charset_and_whitespace() {
        assert!(validate_env_var_key("").is_err());
        assert!(validate_env_var_key(" FOO").is_err());
        assert!(validate_env_var_key("FOO ").is_err());
        assert!(validate_env_var_key("foo bar").is_err());
        assert!(validate_env_var_key("foo$bar").is_err());
    }
}

/// A variable on one side of a comparison. `value` is the plaintext, or
/// `None` when this caller may not compare it (or it could not be decrypted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComparedVar {
    pub value: Option<String>,
    pub is_secret: bool,
    pub is_protected: bool,
    /// `global`, `env:<name>`, `toml` or `cli`.
    pub source: String,
}

/// Compare two sides key by key. Values are copied into the result only for
/// variables plain on both sides.
pub(crate) fn diff_env_vars(
    before: &std::collections::BTreeMap<String, ComparedVar>,
    after: &std::collections::BTreeMap<String, ComparedVar>,
) -> Vec<super::models::EnvVarDiffEntry> {
    use super::models::{EnvVarChange, EnvVarDiffEntry};
    let shown = |v: &ComparedVar| if v.is_secret { None } else { v.value.clone() };
    let mut out = Vec::new();
    for (key, now) in after {
        let entry = match before.get(key) {
            None => EnvVarDiffEntry {
                key: key.clone(),
                change: EnvVarChange::Added,
                is_secret: now.is_secret,
                is_protected: now.is_protected,
                from: None,
                to: shown(now),
                from_source: None,
                to_source: Some(now.source.clone()),
            },
            Some(old) => {
                let change = match (&old.value, &now.value) {
                    (Some(a), Some(b)) if a == b => EnvVarChange::Unchanged,
                    (Some(_), Some(_)) => EnvVarChange::Changed,
                    // Not comparable, but it now comes from somewhere else.
                    _ if old.source != now.source => EnvVarChange::Changed,
                    _ => EnvVarChange::Unknown,
                };
                let both_plain = !old.is_secret && !now.is_secret;
                EnvVarDiffEntry {
                    key: key.clone(),
                    change,
                    is_secret: now.is_secret || old.is_secret,
                    is_protected: now.is_protected || old.is_protected,
                    from: if both_plain { old.value.clone() } else { None },
                    to: if both_plain { now.value.clone() } else { None },
                    from_source: Some(old.source.clone()),
                    to_source: Some(now.source.clone()),
                }
            }
        };
        out.push(entry);
    }
    for (key, old) in before {
        if !after.contains_key(key) {
            out.push(EnvVarDiffEntry {
                key: key.clone(),
                change: EnvVarChange::Removed,
                is_secret: old.is_secret,
                is_protected: old.is_protected,
                from: shown(old),
                to: None,
                from_source: Some(old.source.clone()),
                to_source: None,
            });
        }
    }
    out
}

/// A variable as stored, before deciding whether to decrypt it.
struct RawVar {
    value: String,
    context: EncryptionContext,
    is_secret: bool,
    is_protected: bool,
    source: String,
    /// The environment the variable is scoped to, for session-scope checks.
    scope: Option<String>,
}

/// Compare a deployment's variable snapshot with what a deployment created
/// from it into an environment (`?environment=`, default: the deployment's
/// own) would get now: the project's current Global and environment
/// variables, with the source's rise.toml and CLI variables on top.
///
/// Rise system and extension variables are not compared; they are set by
/// Rise for every deployment. Secrets are decrypted here to compare them, and
/// only whether they changed is returned. Two rules keep this from revealing
/// a secret: a secret is compared only when the caller may read its value on
/// both sides, and a protected secret only when both sides come from the same
/// place, so a guess stored under another scope cannot be tested against it.
pub async fn diff_deployment_env_vars(
    State(state): State<AppState>,
    auth: AuthContext,
    MaybeSessionScope(scope): MaybeSessionScope,
    Path((project_id_or_name, deployment_id)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<super::models::EnvVarDiffResponse>, ServerError> {
    use std::collections::BTreeMap;

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

    let visibility = VarVisibility {
        scope: scope.as_ref(),
        project: &project.name,
    };
    visibility.require_any()?;

    let deployment =
        crate::db::deployments::find_by_deployment_id(&state.db_pool, &deployment_id, project.id)
            .await
            .internal_err("Failed to get deployment")?
            .ok_or_else(|| ServerError::not_found("Deployment not found"))?;
    let deployment_env = db_environments::find_by_id(&state.db_pool, deployment.environment_id)
        .await
        .internal_err("Failed to get environment")?
        .map(|e| e.name);

    // The environment compared against: the one asked for, else the
    // deployment's own.
    let (target_id, target_env) =
        match resolve_environment_id(&state.db_pool, project.id, &params).await? {
            Some(id) => (Some(id), params.get("environment").cloned()),
            None => (Some(deployment.environment_id), deployment_env.clone()),
        };

    // Before: the snapshot's project and configuration variables.
    let snapshot = db_env_vars::list_deployment_env_vars(&state.db_pool, deployment.id)
        .await
        .internal_err("Failed to list deployment environment variables")?;
    let mut before: BTreeMap<String, RawVar> = BTreeMap::new();
    let mut config: Vec<(String, RawVar)> = Vec::new();
    for var in snapshot {
        let scope_env = match var.source.as_str() {
            "global" => None,
            "toml" | "cli" => deployment_env.clone(),
            source => match source.strip_prefix("env:") {
                Some(env) => Some(env.to_string()),
                None => continue, // system and extension variables
            },
        };
        if !visibility.lists(scope_env.as_deref()) {
            continue;
        }
        let raw = RawVar {
            context: EncryptionContext::deployment_env(var.deployment_id, &var.key),
            value: var.value,
            is_secret: var.is_secret,
            is_protected: var.is_protected,
            source: var.source.clone(),
            scope: scope_env,
        };
        if var.source == "toml" || var.source == "cli" {
            config.push((
                var.key.clone(),
                RawVar {
                    context: raw.context.clone(),
                    value: raw.value.clone(),
                    is_secret: raw.is_secret,
                    is_protected: raw.is_protected,
                    source: raw.source.clone(),
                    scope: target_env.clone(),
                },
            ));
        }
        before.insert(var.key, raw);
    }

    // After: Global, the target environment's own values over it, then the
    // source's rise.toml and CLI variables, which a deployment created from
    // it inherits.
    let current = db_env_vars::list_project_env_vars(&state.db_pool, project.id, target_id)
        .await
        .internal_err("Failed to list environment variables")?;
    let (scoped, global): (Vec<_>, Vec<_>) = current
        .into_iter()
        .filter(|v| v.environment_id.is_none() || v.environment_id == target_id)
        .partition(|v| v.environment_id.is_some());
    let mut after: BTreeMap<String, RawVar> = BTreeMap::new();
    for var in global.into_iter().chain(scoped) {
        let context = EncryptionContext::project_env(var.project_id, var.environment_id, &var.key);
        let scope_env = var.environment_id.and(target_env.clone());
        if !visibility.lists(scope_env.as_deref()) {
            continue;
        }
        after.insert(
            var.key,
            RawVar {
                context,
                value: var.value,
                is_secret: var.is_secret,
                is_protected: var.is_protected,
                source: match &scope_env {
                    Some(env) => format!("env:{env}"),
                    None => "global".to_string(),
                },
                scope: scope_env,
            },
        );
    }
    for (key, raw) in config {
        if visibility.lists(raw.scope.as_deref()) {
            after.insert(key, raw);
        }
    }

    // Decide which secrets may be compared, and decrypt only those, at once.
    let comparable = |key: &str| -> bool {
        let (Some(a), Some(b)) = (before.get(key), after.get(key)) else {
            return false;
        };
        if !a.is_secret && !b.is_secret {
            return true;
        }
        let readable = |v: &RawVar| visibility.reveals(v.scope.as_deref());
        let same_place = !(a.is_protected || b.is_protected) || a.source == b.source;
        readable(a) && readable(b) && same_place
    };
    let decrypt = |raw: &RawVar| {
        let value = raw.value.clone();
        let is_secret = raw.is_secret;
        let context = raw.context.clone();
        let state = &state;
        async move {
            if !is_secret {
                return Some(value);
            }
            let provider = state.encryption_provider.as_ref()?;
            match provider.decrypt(&value, &context).await {
                Ok(plain) => Some(plain),
                Err(e) => {
                    tracing::warn!("Failed to decrypt a variable for comparison: {:?}", e);
                    None
                }
            }
        }
    };
    let keys: Vec<String> = before.keys().filter(|k| comparable(k)).cloned().collect();
    let decrypted = futures::future::join_all(keys.iter().map(|k| {
        let (a, b) = (&before[k], &after[k]);
        let (da, db) = (decrypt(a), decrypt(b));
        async move { futures::join!(da, db) }
    }))
    .await;
    let values: HashMap<String, (Option<String>, Option<String>)> =
        keys.into_iter().zip(decrypted).collect();

    let side = |map: BTreeMap<String, RawVar>, first: bool| -> BTreeMap<String, ComparedVar> {
        map.into_iter()
            .map(|(key, raw)| {
                let value = match values.get(&key) {
                    Some((a, b)) => {
                        if first {
                            a.clone()
                        } else {
                            b.clone()
                        }
                    }
                    None if !raw.is_secret => Some(raw.value.clone()),
                    None => None,
                };
                let var = ComparedVar {
                    value,
                    is_secret: raw.is_secret,
                    is_protected: raw.is_protected,
                    source: raw.source,
                };
                (key, var)
            })
            .collect()
    };
    let before = side(before, true);
    let after = side(after, false);

    Ok(Json(super::models::EnvVarDiffResponse {
        environment: target_env,
        changes: diff_env_vars(&before, &after),
    }))
}

#[cfg(test)]
mod diff_tests {
    use super::{diff_env_vars, ComparedVar};
    use crate::server::env_vars::models::EnvVarChange;
    use std::collections::BTreeMap;

    fn var(value: Option<&str>, is_secret: bool, is_protected: bool, source: &str) -> ComparedVar {
        ComparedVar {
            value: value.map(str::to_string),
            is_secret,
            is_protected,
            source: source.into(),
        }
    }

    fn map(vars: &[(&str, ComparedVar)]) -> BTreeMap<String, ComparedVar> {
        vars.iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    fn change(
        changes: &[crate::server::env_vars::models::EnvVarDiffEntry],
        key: &str,
    ) -> EnvVarChange {
        changes.iter().find(|c| c.key == key).unwrap().change
    }

    #[test]
    fn compared_secrets_report_change_without_revealing_values() {
        let before = map(&[
            ("DB_PASSWORD", var(Some("old"), true, true, "global")),
            ("API_KEY", var(Some("same"), true, true, "global")),
        ]);
        let after = map(&[
            ("DB_PASSWORD", var(Some("new"), true, true, "global")),
            ("API_KEY", var(Some("same"), true, true, "global")),
        ]);
        let changes = diff_env_vars(&before, &after);
        assert_eq!(change(&changes, "DB_PASSWORD"), EnvVarChange::Changed);
        assert_eq!(change(&changes, "API_KEY"), EnvVarChange::Unchanged);
        assert!(changes.iter().all(|c| c.from.is_none() && c.to.is_none()));
    }

    #[test]
    fn uncomparable_values_are_unknown_unless_their_source_moved() {
        let before = map(&[
            ("SAME_PLACE", var(None, true, true, "global")),
            ("MOVED", var(None, true, true, "global")),
        ]);
        let after = map(&[
            ("SAME_PLACE", var(None, true, true, "global")),
            ("MOVED", var(None, true, true, "env:production")),
        ]);
        let changes = diff_env_vars(&before, &after);
        assert_eq!(change(&changes, "SAME_PLACE"), EnvVarChange::Unknown);
        assert_eq!(change(&changes, "MOVED"), EnvVarChange::Changed);
        let moved = changes.iter().find(|c| c.key == "MOVED").unwrap();
        assert_eq!(moved.from_source.as_deref(), Some("global"));
        assert_eq!(moved.to_source.as_deref(), Some("env:production"));
    }

    #[test]
    fn plain_values_are_shown_and_additions_and_removals_reported() {
        let before = map(&[
            ("LOG_LEVEL", var(Some("info"), false, false, "global")),
            ("OLD", var(Some("x"), false, false, "toml")),
        ]);
        let after = map(&[
            ("LOG_LEVEL", var(Some("warn"), false, false, "global")),
            ("NEW", var(Some("y"), false, false, "env:staging")),
        ]);
        let changes = diff_env_vars(&before, &after);
        let level = changes.iter().find(|c| c.key == "LOG_LEVEL").unwrap();
        assert_eq!(level.change, EnvVarChange::Changed);
        assert_eq!(
            (level.from.as_deref(), level.to.as_deref()),
            (Some("info"), Some("warn"))
        );
        assert_eq!(change(&changes, "NEW"), EnvVarChange::Added);
        assert_eq!(change(&changes, "OLD"), EnvVarChange::Removed);
    }

    #[test]
    fn a_value_that_turned_secret_is_hidden_on_both_sides() {
        let before = map(&[("TOKEN", var(Some("plain-before"), false, false, "global"))]);
        let after = map(&[("TOKEN", var(Some("secret-now"), true, false, "global"))]);
        let changes = diff_env_vars(&before, &after);
        assert_eq!(changes[0].change, EnvVarChange::Changed);
        assert!(changes[0].from.is_none() && changes[0].to.is_none());
    }
}
