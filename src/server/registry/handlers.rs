use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};

use super::models::GetRegistryCredsResponse;
use crate::db::{deployments as db_deployments, projects};
use crate::server::auth::context::AuthContext;
use crate::server::deployment::state_machine;
use crate::server::error::{ServerError, ServerErrorExt};
use crate::server::project::handlers::ensure_project_access_or_admin;
use crate::server::state::AppState;

/// Get registry credentials scoped to a specific in-progress deployment.
///
/// Credentials are only available while the deployment still needs an image push
/// (Pending, Building, or Pushing states). Returns 409 Conflict if the deployment
/// has already progressed past the Pushing state.
pub async fn get_deployment_registry_credentials(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((project_name, deployment_id)): Path<(String, String)>,
) -> Result<Json<GetRegistryCredsResponse>, ServerError> {
    // Find the project by name
    let project = projects::find_by_name(&state.db_pool, &project_name)
        .await
        .internal_err("Failed to find project")
        .map_err(|e| e.with_context("project_name", &project_name))?
        .ok_or_else(|| ServerError::not_found(format!("Project '{}' not found", project_name)))?;

    // Resolve auth for project scope
    let (user, is_sa) = auth
        .resolve_for_project(
            &state.db_pool,
            &project,
            state.controllers_by_issuer.as_ref(),
        )
        .await
        .map_err(|e| {
            if e.status == StatusCode::UNAUTHORIZED || e.status == StatusCode::FORBIDDEN {
                ServerError::not_found(format!("Project '{}' not found", project.name))
            } else {
                e
            }
        })?;

    // Check if user has permission (SA access already validated)
    if !is_sa {
        ensure_project_access_or_admin(&state, &user, &project).await?;
    }

    // Find the deployment
    let deployment =
        db_deployments::find_by_deployment_id(&state.db_pool, &deployment_id, project.id)
            .await
            .internal_err("Failed to find deployment")
            .map_err(|e| {
                e.with_context("project_name", &project_name)
                    .with_context("deployment_id", &deployment_id)
            })?
            .ok_or_else(|| {
                ServerError::not_found(format!(
                    "Deployment '{}' not found for project '{}'",
                    deployment_id, project_name
                ))
            })?;

    // Validate that the deployment still needs an image push
    if !state_machine::needs_image_push(&deployment.status) {
        return Err(ServerError::new(
            StatusCode::CONFLICT,
            format!(
                "Deployment '{}' is in state '{}' and no longer accepts image pushes",
                deployment_id, deployment.status
            ),
        ));
    }

    // Get credentials from the registry provider
    let repository = project.name.clone();

    let credentials = state
        .registry_provider
        .get_credentials(&repository, &deployment_id)
        .await
        .internal_err("Failed to get registry credentials")
        .map_err(|e| {
            e.with_context("project_name", &project_name)
                .with_context("repository", &repository)
        })?;

    Ok(Json(GetRegistryCredsResponse {
        credentials,
        repository,
    }))
}
