use super::fuzzy::find_similar_projects;
use super::models::{
    AccessClassInfo, CreateProjectRequest, CreateProjectResponse, DeploymentDefaultsInfo,
    GetProjectParams, ListAccessClassesResponse, OwnerInfo, PlatformConstraintsInfo,
    Project as ApiProject, ProjectOwner, ProjectStatus, TeamInfo, UpdateProjectRequest,
    UpdateProjectResponse, UserInfo,
};
use crate::db::models::User;
use crate::db::{projects, teams as db_teams, users as db_users};
use crate::server::auth::context::AuthContext;
use crate::server::error::{ServerError, ServerErrorExt};
use crate::server::state::AppState;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use uuid::Uuid;

/// Validate that a URL uses only http or https schemes.
/// Returns the trimmed URL on success, or an error message if the URL is invalid or uses a disallowed scheme.
pub fn validate_http_url(url: &str) -> Result<String, String> {
    let trimmed = url.trim();
    let parsed = url::Url::parse(trimmed).map_err(|e| format!("Invalid URL: {e}"))?;
    match parsed.scheme() {
        "http" | "https" => Ok(trimmed.to_string()),
        scheme => Err(format!(
            "URL scheme '{}' is not allowed; only http and https are permitted",
            scheme
        )),
    }
}

/// List available access classes for the deployment controller
pub async fn list_access_classes(
    State(state): State<AppState>,
    auth: AuthContext,
) -> Result<Json<ListAccessClassesResponse>, ServerError> {
    let _user = auth.user()?;
    let access_classes = state
        .access_classes
        .iter()
        .map(|(id, class)| AccessClassInfo {
            id: id.clone(),
            display_name: class.display_name.clone(),
            description: class.description.clone(),
        })
        .collect();

    Ok(Json(ListAccessClassesResponse { access_classes }))
}

/// Resolve user identifier (UUID or email) to user ID
async fn resolve_user_identifier(
    pool: &sqlx::PgPool,
    identifier: &str,
) -> Result<uuid::Uuid, ServerError> {
    use crate::server::error::ServerErrorExt;

    if let Ok(uuid) = uuid::Uuid::parse_str(identifier) {
        // Valid UUID - verify user exists
        db_users::find_by_id(pool, uuid)
            .await
            .internal_err("Failed to lookup user")?
            .ok_or_else(|| ServerError::not_found(format!("User not found: {}", identifier)))
            .map(|u| u.id)
    } else {
        // Treat as email - look up user
        db_users::find_by_email(pool, identifier)
            .await
            .internal_err("Failed to lookup user")?
            .ok_or_else(|| ServerError::not_found(format!("User not found: {}", identifier)))
            .map(|u| u.id)
    }
}

/// Resolve team identifier (UUID or name) to team ID
async fn resolve_team_identifier(
    pool: &sqlx::PgPool,
    identifier: &str,
) -> Result<uuid::Uuid, ServerError> {
    use crate::server::error::ServerErrorExt;

    if let Ok(uuid) = uuid::Uuid::parse_str(identifier) {
        // Valid UUID - verify team exists
        db_teams::find_by_id(pool, uuid)
            .await
            .internal_err("Failed to lookup team")?
            .ok_or_else(|| ServerError::not_found(format!("Team not found: {}", identifier)))
            .map(|t| t.id)
    } else {
        // Treat as team name - look up team
        db_teams::find_by_name(pool, identifier)
            .await
            .internal_err("Failed to lookup team")?
            .ok_or_else(|| ServerError::not_found(format!("Team not found: {}", identifier)))
            .map(|t| t.id)
    }
}

pub async fn create_project(
    State(state): State<AppState>,
    auth: AuthContext,
    Json(payload): Json<CreateProjectRequest>,
) -> Result<Json<CreateProjectResponse>, ServerError> {
    let user = auth.user()?;
    // Validate access_class against configured access classes
    let is_valid_access_class = state.access_classes.contains_key(&payload.access_class);

    if !is_valid_access_class {
        let available = state
            .access_classes
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");

        return Err(ServerError::bad_request(format!(
            "Invalid access class '{}'. Available: {}",
            payload.access_class, available
        )));
    }

    // Validate and normalize source_url if provided
    let source_url = match payload.source_url {
        Some(ref url) => Some(
            validate_http_url(url)
                .map_err(|e| ServerError::bad_request(format!("source_url: {e}")))?,
        ),
        None => None,
    };

    // Validate owner - exactly one of owner_user or owner_team must be set
    let (owner_user_id, owner_team_id) = match &payload.owner {
        ProjectOwner::User(user_id) => {
            let uuid =
                Uuid::parse_str(user_id).server_err(StatusCode::BAD_REQUEST, "Invalid user ID")?;
            (Some(uuid), None)
        }
        ProjectOwner::Team(team_id) => {
            let uuid =
                Uuid::parse_str(team_id).server_err(StatusCode::BAD_REQUEST, "Invalid team ID")?;

            // Verify user is a member of the team
            let is_member = db_teams::is_member(&state.db_pool, uuid, user.id)
                .await
                .internal_err("Failed to check team membership")
                .map_err(|e| {
                    e.with_context("team_id", uuid.to_string())
                        .with_context("user_id", user.id.to_string())
                })?;

            if !is_member {
                return Err(ServerError::forbidden("You are not a member of this team"));
            }

            (None, Some(uuid))
        }
    };

    tracing::info!(
        "Creating project '{}' for user {}",
        payload.name,
        user.email
    );

    use crate::server::error::ServerErrorExt;

    let mut tx = state
        .db_pool
        .begin()
        .await
        .internal_err("Failed to start transaction")?;

    let project = projects::create(
        &mut *tx,
        &payload.name,
        crate::db::models::ProjectStatus::Stopped,
        payload.access_class,
        owner_user_id,
        owner_team_id,
        source_url.as_deref(),
    )
    .await
    .map_err(|e| {
        if e.to_string().contains("duplicate key") || e.to_string().contains("unique constraint") {
            ServerError::new(
                StatusCode::CONFLICT,
                format!("Project '{}' already exists", &payload.name),
            )
        } else {
            ServerError::internal_anyhow(e, "Failed to create project")
                .with_context("project_name", &payload.name)
                .with_context("user_email", &user.email)
        }
    })?;

    // Bootstrap default "production" environment for new project
    crate::db::environments::create_default_for_project(&mut *tx, project.id)
        .await
        .map_err(|e| {
            ServerError::internal_anyhow(e, "Failed to create default environment for project")
        })?;

    // Add app users if provided
    for user_identifier in &payload.app_users {
        let user_id = resolve_user_identifier(&state.db_pool, user_identifier).await?;
        crate::db::project_app_users::add_user(&mut *tx, project.id, user_id)
            .await
            .internal_err("Failed to add app user")?;
    }

    // Add app teams if provided
    for team_identifier in &payload.app_teams {
        let team_id = resolve_team_identifier(&state.db_pool, team_identifier).await?;
        crate::db::project_app_users::add_team(&mut *tx, project.id, team_id)
            .await
            .internal_err("Failed to add app team")?;
    }

    tx.commit()
        .await
        .internal_err("Failed to commit transaction")?;

    // Create RiseProject CRD for Metacontroller (best-effort)
    #[cfg(feature = "backend")]
    if let Some(ref kube_client) = state.kube_client {
        if let Err(e) =
            crate::server::deployment::crd::ensure_rise_project(kube_client, &project.name).await
        {
            tracing::warn!(
                project = %project.name,
                "Failed to create RiseProject CRD: {:?}", e
            );
        }
    }

    let owner_info = resolve_owner_info(&state, &project)
        .await
        .map_err(|e| ServerError::internal(format!("Failed to resolve owner info: {}", e)))?;

    Ok(Json(CreateProjectResponse {
        project: convert_project(project, owner_info, &state),
    }))
}

pub async fn list_projects(
    State(state): State<AppState>,
    auth: AuthContext,
) -> Result<Json<Vec<ApiProject>>, ServerError> {
    let user = auth.user()?;
    // Admins can see all projects, others only see projects they have access to
    let projects = if state.is_admin(&user.email) {
        projects::list(&state.db_pool, None)
            .await
            .internal_err("Failed to list projects")?
    } else {
        projects::list_accessible_by_user(&state.db_pool, user.id)
            .await
            .internal_err("Failed to list projects")
            .map_err(|e| e.with_context("user_id", user.id.to_string()))?
    };

    // Batch fetch active deployment info for efficiency
    let project_ids: Vec<uuid::Uuid> = projects.iter().map(|p| p.id).collect();
    let active_deployment_info =
        projects::get_active_deployment_info_batch(&state.db_pool, &project_ids)
            .await
            .internal_err("Failed to get active deployment info")?;

    // Batch fetch active deployments to calculate URLs
    let deployment_ids: Vec<Uuid> = active_deployment_info
        .values()
        .filter_map(|info| info.as_ref().map(|i| i.id))
        .collect();

    let deployments_map = if !deployment_ids.is_empty() {
        crate::db::deployments::get_deployments_batch(&state.db_pool, &deployment_ids)
            .await
            .internal_err("Failed to get deployments")?
    } else {
        std::collections::HashMap::new()
    };

    // Batch fetch owner information (user emails and team names)
    let user_ids: Vec<Uuid> = projects.iter().filter_map(|p| p.owner_user_id).collect();
    let team_ids: Vec<Uuid> = projects.iter().filter_map(|p| p.owner_team_id).collect();

    let user_emails = if !user_ids.is_empty() {
        db_users::get_emails_batch(&state.db_pool, &user_ids)
            .await
            .internal_err("Failed to get user emails")?
    } else {
        std::collections::HashMap::new()
    };

    let team_names = if !team_ids.is_empty() {
        db_teams::get_names_batch(&state.db_pool, &team_ids)
            .await
            .internal_err("Failed to get team names")?
    } else {
        std::collections::HashMap::new()
    };

    // Calculate URLs for all projects
    let mut api_projects = Vec::new();
    for project in projects {
        let (active_deployment_status, default_url, primary_url, custom_domain_urls) =
            if let Some(Some(info)) = active_deployment_info.get(&project.id) {
                if let Some(deployment) = deployments_map.get(&info.id) {
                    match state
                        .deployment_backend
                        .get_deployment_urls(deployment, &project)
                        .await
                    {
                        Ok(urls) => (
                            Some(info.status.to_string()),
                            Some(urls.default_url),
                            Some(urls.primary_url),
                            urls.custom_domain_urls,
                        ),
                        Err(e) => {
                            return Err(ServerError::internal_anyhow(
                                e,
                                "Failed to calculate deployment URLs",
                            )
                            .with_context("project_name", &project.name));
                        }
                    }
                } else {
                    (Some(info.status.to_string()), None, None, vec![])
                }
            } else {
                (None, None, None, vec![])
            };

        let owner = if let Some(user_id) = project.owner_user_id {
            user_emails.get(&user_id).map(|email| {
                OwnerInfo::User(UserInfo {
                    id: user_id.to_string(),
                    email: email.clone(),
                })
            })
        } else if let Some(team_id) = project.owner_team_id {
            team_names.get(&team_id).map(|name| {
                OwnerInfo::Team(TeamInfo {
                    id: team_id.to_string(),
                    name: name.clone(),
                })
            })
        } else {
            None
        };

        api_projects.push(ApiProject {
            id: project.id.to_string(),
            created: project.created_at.to_rfc3339(),
            updated: project.updated_at.to_rfc3339(),
            name: project.name,
            status: ProjectStatus::from(project.status),
            access_class: project.access_class,
            owner,
            active_deployment_status,
            default_url,
            primary_url,
            custom_domain_urls,
            deployment_groups: None, // Not populated in list view for performance
            finalizers: vec![],      // Not populated in list view for performance
            app_users: vec![],       // Not populated in list view for performance
            app_teams: vec![],       // Not populated in list view for performance
            source_url: project.source_url,
            resolved_source_url: None, // Not populated in list view for performance
            deployment_defaults: None, // Not populated in list view
            platform_constraints: None, // Not populated in list view
        });
    }

    Ok(Json(api_projects))
}

pub async fn get_project(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id_or_name): Path<String>,
    Query(params): Query<GetProjectParams>,
) -> Result<Json<serde_json::Value>, ServerError> {
    let user = auth.user()?;
    // Resolve project by ID or name
    let project = resolve_project(&state, &id_or_name, params.by_id).await?;

    // Check read permission
    let can_read = check_read_permission(&state, &project, user)
        .await
        .map_err(|e| ServerError::internal(format!("Failed to check permissions: {}", e)))?;

    if !can_read {
        // Use 404 to hide project existence from unauthorized users
        return Err(ServerError::not_found(format!(
            "Project '{}' not found",
            id_or_name
        )));
    }

    // Calculate deployment URLs if there's an active deployment
    let (default_url, primary_url, custom_domain_urls) =
        match crate::db::deployments::get_active_deployments_for_project(&state.db_pool, project.id)
            .await
        {
            Ok(active_deployments) => {
                // Find the active deployment in the default group
                if let Some(deployment) = active_deployments.iter().find(|d| {
                    d.deployment_group
                        == crate::server::deployment::models::DEFAULT_DEPLOYMENT_GROUP
                }) {
                    match state
                        .deployment_backend
                        .get_deployment_urls(deployment, &project)
                        .await
                    {
                        Ok(urls) => (
                            Some(urls.default_url),
                            Some(urls.primary_url),
                            urls.custom_domain_urls,
                        ),
                        Err(e) => {
                            return Err(ServerError::internal_anyhow(
                                e,
                                "Failed to calculate URLs",
                            ));
                        }
                    }
                } else {
                    (None, None, vec![])
                }
            }
            Err(e) => {
                return Err(ServerError::internal_anyhow(
                    e,
                    "Failed to get active deployments",
                ));
            }
        };

    // Resolve owner info
    let owner_info = resolve_owner_info(&state, &project)
        .await
        .map_err(|e| ServerError::internal(format!("Failed to resolve owner info: {}", e)))?;

    let mut api_project = convert_project(project.clone(), owner_info, &state);
    api_project.default_url = default_url;
    api_project.primary_url = primary_url;
    api_project.custom_domain_urls = custom_domain_urls;

    // When no source URL is explicitly configured, resolve one from deployment
    // metadata (active primary deployment, else most recent deployment).
    if api_project.source_url.is_none() {
        api_project.resolved_source_url =
            crate::db::deployments::resolve_git_repository_url(&state.db_pool, project.id)
                .await
                .internal_err("Failed to resolve project source URL")?;
    }

    // Get active deployment groups
    let deployment_groups =
        crate::db::deployments::get_active_deployment_groups(&state.db_pool, project.id)
            .await
            .internal_err("Failed to get deployment groups")?;
    api_project.deployment_groups = if deployment_groups.is_empty() {
        None
    } else {
        Some(deployment_groups)
    };

    // Load app users and teams
    let (app_users, app_teams) = load_app_users_for_project(&state, project.id)
        .await
        .map_err(|e| ServerError::internal(format!("Failed to load app users: {}", e)))?;
    api_project.app_users = app_users;
    api_project.app_teams = app_teams;

    Ok(Json(serde_json::to_value(api_project).unwrap()))
}

pub async fn update_project(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id_or_name): Path<String>,
    Query(params): Query<GetProjectParams>,
    Json(payload): Json<UpdateProjectRequest>,
) -> Result<Json<UpdateProjectResponse>, ServerError> {
    let user = auth.user()?;
    // Resolve project by ID or name
    let project = resolve_project(&state, &id_or_name, params.by_id).await?;

    // Check write permission
    let can_write = check_write_permission(&state, &project, user)
        .await
        .map_err(|e| ServerError::internal(format!("Failed to check permissions: {}", e)))?;

    if !can_write {
        return Err(ServerError::forbidden(
            "You do not have permission to update this project",
        ));
    }

    // Service accounts cannot update projects
    if auth.is_service_account() {
        return Err(ServerError::forbidden(
            "Service accounts cannot modify projects",
        ));
    }

    // Update project fields
    let mut updated_project = project;

    // Update owner if provided
    if let Some(owner) = payload.owner {
        let (owner_user_id, owner_team_id) = match owner {
            ProjectOwner::User(user_identifier) => {
                // Try to resolve as email first, then as UUID
                let user = if let Ok(uuid) = Uuid::parse_str(&user_identifier) {
                    // Valid UUID - look up by ID
                    db_users::find_by_id(&state.db_pool, uuid)
                        .await
                        .internal_err("Failed to verify user")?
                } else {
                    // Not a UUID - treat as email
                    db_users::find_by_email(&state.db_pool, &user_identifier)
                        .await
                        .internal_err("Failed to verify user")?
                };

                let user = user.ok_or_else(|| {
                    ServerError::not_found(format!("User '{}' not found", user_identifier))
                })?;

                (Some(user.id), None)
            }
            ProjectOwner::Team(team_identifier) => {
                // Try to resolve as name first, then as UUID
                let team = if let Ok(uuid) = Uuid::parse_str(&team_identifier) {
                    // Valid UUID - look up by ID
                    db_teams::find_by_id(&state.db_pool, uuid)
                        .await
                        .internal_err("Failed to verify team")?
                } else {
                    // Not a UUID - treat as team name
                    db_teams::find_by_name(&state.db_pool, &team_identifier)
                        .await
                        .internal_err("Failed to verify team")?
                };

                let team = team.ok_or_else(|| {
                    ServerError::not_found(format!("Team '{}' not found", team_identifier))
                })?;

                // Verify the requesting user is a member of the team they're transferring to
                let is_member = db_teams::is_member(&state.db_pool, team.id, user.id)
                    .await
                    .internal_err("Failed to check team membership")?;

                if !is_member {
                    return Err(ServerError::forbidden(format!(
                        "You must be a member of team '{}' to transfer projects to it",
                        team.name
                    )));
                }

                (None, Some(team.id))
            }
        };

        updated_project = projects::update_owner(
            &state.db_pool,
            updated_project.id,
            owner_user_id,
            owner_team_id,
        )
        .await
        .internal_err("Failed to update project owner")?;
    }

    // Update access_class if provided
    if let Some(access_class) = payload.access_class {
        // Validate against configured access classes
        let is_valid = state.access_classes.contains_key(&access_class);

        if !is_valid {
            let available = state
                .access_classes
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");

            return Err(ServerError::bad_request(format!(
                "Invalid access class '{}'. Available: {}",
                access_class, available
            )));
        }

        // Only `access_class` currently affects ingress configuration via the
        // RiseProject CRD (see resource_builder.rs). When `visibility` becomes
        // ingress-enforced (per CLAUDE.md "Ingress Authentication"), extend
        // the same resync trigger to that field too.
        let access_class_changed = updated_project.access_class != access_class;
        updated_project =
            projects::update_access_class(&state.db_pool, updated_project.id, access_class)
                .await
                .internal_err("Failed to update project access class")?;

        // Fire immediately after the DB write so a later validation error in
        // this same request can't leave the DB updated and the CRD stale.
        if access_class_changed {
            if let Some(ref kube_client) = state.kube_client {
                if let Err(e) = crate::server::deployment::crd::trigger_resync(
                    kube_client,
                    &updated_project.name,
                )
                .await
                {
                    tracing::warn!(
                        project = %updated_project.name,
                        "Failed to trigger CRD resync after access class update: {:?}", e
                    );
                }
            }
        }
    }

    // Update app users if provided
    if let Some(app_users) = payload.app_users {
        let mut tx = state
            .db_pool
            .begin()
            .await
            .internal_err("Failed to start transaction")?;

        // Remove all existing app users
        let existing_users = crate::db::project_app_users::list_users(&mut *tx, updated_project.id)
            .await
            .internal_err("Failed to list existing app users")?;

        for user_id in existing_users {
            crate::db::project_app_users::remove_user(&mut *tx, updated_project.id, user_id)
                .await
                .internal_err("Failed to remove app user")?;
        }

        // Add new app users
        for user_identifier in &app_users {
            let user_id = resolve_user_identifier(&state.db_pool, user_identifier).await?;

            crate::db::project_app_users::add_user(&mut *tx, updated_project.id, user_id)
                .await
                .internal_err("Failed to add app user")?;
        }

        tx.commit()
            .await
            .internal_err("Failed to commit transaction")?;
    }

    // Update app teams if provided
    if let Some(app_teams) = payload.app_teams {
        let mut tx = state
            .db_pool
            .begin()
            .await
            .internal_err("Failed to start transaction")?;

        // Remove all existing app teams
        let existing_teams = crate::db::project_app_users::list_teams(&mut *tx, updated_project.id)
            .await
            .internal_err("Failed to list existing app teams")?;

        for team_id in existing_teams {
            crate::db::project_app_users::remove_team(&mut *tx, updated_project.id, team_id)
                .await
                .internal_err("Failed to remove app team")?;
        }

        // Add new app teams
        for team_identifier in &app_teams {
            let team_id = resolve_team_identifier(&state.db_pool, team_identifier).await?;

            crate::db::project_app_users::add_team(&mut *tx, updated_project.id, team_id)
                .await
                .internal_err("Failed to add app team")?;
        }

        tx.commit()
            .await
            .internal_err("Failed to commit transaction")?;
    }

    // Update status if provided
    if let Some(status) = payload.status {
        updated_project = projects::update_status(
            &state.db_pool,
            updated_project.id,
            crate::db::models::ProjectStatus::from(status),
        )
        .await
        .internal_err("Failed to update project status")?;
    }

    // Update source_url if provided (Some(None) clears, Some(Some(url)) sets)
    if let Some(ref source_url) = payload.source_url {
        let normalized = match source_url {
            Some(url) => Some(
                validate_http_url(url)
                    .map_err(|e| ServerError::bad_request(format!("source_url: {e}")))?,
            ),
            None => None,
        };
        updated_project =
            projects::update_source_url(&state.db_pool, updated_project.id, normalized)
                .await
                .internal_err("Failed to update project source URL")?;
    }

    let owner_info = resolve_owner_info(&state, &updated_project)
        .await
        .map_err(|e| ServerError::internal(format!("Failed to resolve owner info: {}", e)))?;

    Ok(Json(UpdateProjectResponse {
        project: convert_project(updated_project, owner_info, &state),
    }))
}

pub async fn delete_project(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id_or_name): Path<String>,
    Query(params): Query<GetProjectParams>,
) -> Result<StatusCode, ServerError> {
    let user = auth.user()?;
    // Resolve project by ID or name
    let project = resolve_project(&state, &id_or_name, params.by_id).await?;

    // Check write permission
    let can_write = check_write_permission(&state, &project, user)
        .await
        .map_err(|e| ServerError::internal(format!("Failed to check permissions: {}", e)))?;

    if !can_write {
        return Err(ServerError::forbidden(
            "You do not have permission to delete this project",
        ));
    }

    // Service accounts cannot delete projects
    if auth.is_service_account() {
        return Err(ServerError::forbidden(
            "Service accounts cannot modify projects",
        ));
    }

    // Check if already deleting
    if project.status == crate::db::models::ProjectStatus::Deleting {
        return Ok(StatusCode::ACCEPTED);
    }

    // Mark project as deleting
    projects::mark_deleting(&state.db_pool, project.id)
        .await
        .internal_err("Failed to mark project for deletion")?;

    // Delete RiseProject CRD — Metacontroller will call the finalize webhook
    #[cfg(feature = "backend")]
    if let Some(ref kube_client) = state.kube_client {
        if let Err(e) =
            crate::server::deployment::crd::delete_rise_project(kube_client, &project.name).await
        {
            tracing::warn!(
                project = %project.name,
                "Failed to delete RiseProject CRD: {:?}", e
            );
        }
    }

    tracing::info!("Project {} marked for deletion", project.name);

    // Return 202 Accepted - deletion is asynchronous
    Ok(StatusCode::ACCEPTED)
}

/// Query project by ID
async fn query_project_by_id(
    state: &AppState,
    project_id: &str,
) -> Result<crate::db::models::Project, String> {
    let uuid = Uuid::parse_str(project_id).map_err(|e| format!("Invalid project ID: {}", e))?;

    projects::find_by_id(&state.db_pool, uuid)
        .await
        .map_err(|e| format!("Project not found: {}", e))?
        .ok_or_else(|| "Project not found".to_string())
}

/// Query project by name
async fn query_project_by_name(
    state: &AppState,
    project_name: &str,
) -> Result<crate::db::models::Project, String> {
    tracing::info!("Querying project by name: {}", project_name);

    projects::find_by_name(&state.db_pool, project_name)
        .await
        .map_err(|e| format!("Failed to query project by name: {}", e))?
        .ok_or_else(|| format!("Project '{}' not found", project_name))
}

/// Resolve owner information for a project
async fn resolve_owner_info(
    state: &AppState,
    project: &crate::db::models::Project,
) -> Result<Option<OwnerInfo>, String> {
    if let Some(user_id) = project.owner_user_id {
        let user = db_users::find_by_id(&state.db_pool, user_id)
            .await
            .map_err(|e| format!("Failed to fetch user: {}", e))?
            .ok_or_else(|| "Owner user not found".to_string())?;

        Ok(Some(OwnerInfo::User(UserInfo {
            id: user.id.to_string(),
            email: user.email,
        })))
    } else if let Some(team_id) = project.owner_team_id {
        let team = db_teams::find_by_id(&state.db_pool, team_id)
            .await
            .map_err(|e| format!("Failed to fetch team: {}", e))?
            .ok_or_else(|| "Owner team not found".to_string())?;

        Ok(Some(OwnerInfo::Team(TeamInfo {
            id: team.id.to_string(),
            name: team.name,
        })))
    } else {
        Ok(None)
    }
}

/// Resolve project by ID or name with fuzzy matching support
async fn resolve_project(
    state: &AppState,
    id_or_name: &str,
    by_id: bool,
) -> Result<crate::db::models::Project, ServerError> {
    tracing::info!("Resolving project '{}', by_id={}", id_or_name, by_id);

    let project = if by_id {
        // Explicit ID lookup
        tracing::info!("Using explicit ID lookup");
        query_project_by_id(state, id_or_name)
            .await
            .map_err(ServerError::not_found)?
    } else {
        // Try name first, fallback to ID
        tracing::info!("Trying name lookup first, will fallback to ID");
        match query_project_by_name(state, id_or_name).await {
            Ok(project) => project,
            Err(e) => {
                tracing::info!("Name lookup failed: {}, trying ID fallback", e);
                query_project_by_id(state, id_or_name).await.map_err(|_e| {
                    tracing::info!("Both lookups failed, generating fuzzy suggestions");
                    // Both failed - provide fuzzy suggestions
                    let all_projects = tokio::task::block_in_place(|| {
                        tokio::runtime::Handle::current()
                            .block_on(projects::list(&state.db_pool, None))
                    });

                    let suggestions = match all_projects {
                        Ok(all_projects) => {
                            let api_projects: Vec<ApiProject> = all_projects
                                .into_iter()
                                .map(|p| convert_project(p, None, state))
                                .collect();
                            let similar = find_similar_projects(id_or_name, &api_projects, 0.85);
                            if similar.is_empty() {
                                None
                            } else {
                                Some(similar)
                            }
                        }
                        Err(_) => None,
                    };

                    ServerError::not_found(format!("Project '{}' not found", id_or_name))
                        .with_suggestions(suggestions)
                })?
            }
        }
    };

    Ok(project)
}

/// Load app users and teams for a project
async fn load_app_users_for_project(
    state: &AppState,
    project_id: uuid::Uuid,
) -> Result<(Vec<UserInfo>, Vec<TeamInfo>), String> {
    // Get app user IDs
    let user_ids = crate::db::project_app_users::list_users(&state.db_pool, project_id)
        .await
        .map_err(|e| format!("Failed to list app users: {}", e))?;

    // Get app team IDs
    let team_ids = crate::db::project_app_users::list_teams(&state.db_pool, project_id)
        .await
        .map_err(|e| format!("Failed to list app teams: {}", e))?;

    // Batch fetch user details (if any)
    let users = if !user_ids.is_empty() {
        let users_map = db_users::get_users_batch(&state.db_pool, &user_ids)
            .await
            .map_err(|e| format!("Failed to batch fetch users: {}", e))?;

        user_ids
            .into_iter()
            .filter_map(|id| {
                users_map.get(&id).map(|u| UserInfo {
                    id: u.id.to_string(),
                    email: u.email.clone(),
                })
            })
            .collect()
    } else {
        Vec::new()
    };

    // Batch fetch team details (if any)
    let teams = if !team_ids.is_empty() {
        let teams_map = db_teams::get_teams_batch(&state.db_pool, &team_ids)
            .await
            .map_err(|e| format!("Failed to batch fetch teams: {}", e))?;

        team_ids
            .into_iter()
            .filter_map(|id| {
                teams_map.get(&id).map(|t| TeamInfo {
                    id: t.id.to_string(),
                    name: t.name.clone(),
                })
            })
            .collect()
    } else {
        Vec::new()
    };

    Ok((users, teams))
}

/// Convert database Project model to API Project model
fn convert_project(
    project: crate::db::models::Project,
    owner: Option<OwnerInfo>,
    state: &AppState,
) -> ApiProject {
    #[cfg(feature = "backend")]
    let (deployment_defaults, platform_constraints) = {
        let defaults = state
            .deployment_defaults
            .as_ref()
            .map(|d| DeploymentDefaultsInfo {
                replicas: d.replicas,
                cpu: d.cpu.clone(),
                memory: d.memory.clone(),
            });
        let constraints = state
            .deployment_constraints
            .as_ref()
            .map(|c| PlatformConstraintsInfo {
                min_replicas: c.min_replicas,
                max_replicas: c.max_replicas,
                min_cpu: c.min_cpu.clone(),
                max_cpu: c.max_cpu.clone(),
                min_memory: c.min_memory.clone(),
                max_memory: c.max_memory.clone(),
            });
        (defaults, constraints)
    };
    #[cfg(not(feature = "backend"))]
    let (deployment_defaults, platform_constraints) = {
        let _ = state;
        (
            None::<DeploymentDefaultsInfo>,
            None::<PlatformConstraintsInfo>,
        )
    };

    ApiProject {
        id: project.id.to_string(),
        created: project.created_at.to_rfc3339(),
        updated: project.updated_at.to_rfc3339(),
        name: project.name,
        status: ProjectStatus::from(project.status),
        access_class: project.access_class,
        owner,
        active_deployment_status: None, // Will be populated by caller if needed
        default_url: None,              // Will be populated by caller
        primary_url: None,              // Will be populated by caller
        custom_domain_urls: vec![],     // Will be populated by caller
        deployment_groups: None,        // Will be populated by caller if needed
        finalizers: project.finalizers.clone(),
        app_users: vec![], // Will be populated by caller if needed
        app_teams: vec![], // Will be populated by caller if needed
        source_url: project.source_url,
        resolved_source_url: None, // Will be populated by caller if source_url is unset
        deployment_defaults,
        platform_constraints,
    }
}

/// Check if user has access to a project, returning an error if not (admin bypass)
///
/// Admins always have access. Non-admins must pass the project ownership/team membership check.
/// Returns `Ok(())` if access is granted, or `Err(ServerError::forbidden(...))` if not.
pub async fn ensure_project_access_or_admin(
    state: &AppState,
    user: &User,
    project: &crate::db::models::Project,
) -> Result<(), ServerError> {
    if state.is_admin(&user.email) {
        return Ok(());
    }

    let can_access = projects::user_can_access(&state.db_pool, project.id, user.id)
        .await
        .internal_err("Failed to check project access")?;

    if !can_access {
        return Err(ServerError::forbidden(
            "You do not have access to this project",
        ));
    }

    Ok(())
}

/// Check if user can read a project (owner, team member, or admin)
pub async fn check_read_permission(
    state: &AppState,
    project: &crate::db::models::Project,
    user: &User,
) -> Result<bool, String> {
    // Admins have full access
    if state.is_admin(&user.email) {
        return Ok(true);
    }

    // Check ownership or team membership
    projects::user_can_access(&state.db_pool, project.id, user.id)
        .await
        .map_err(|e| format!("Failed to check access: {}", e))
}

/// Check if user can write to a project (owner, team member, or admin)
pub async fn check_write_permission(
    state: &AppState,
    project: &crate::db::models::Project,
    user: &User,
) -> Result<bool, String> {
    // Admins have full access
    if state.is_admin(&user.email) {
        return Ok(true);
    }

    // Write access requires ownership (user or team member)
    projects::user_can_access(&state.db_pool, project.id, user.id)
        .await
        .map_err(|e| format!("Failed to check access: {}", e))
}
