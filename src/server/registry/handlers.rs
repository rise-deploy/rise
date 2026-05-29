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

    // Get credentials from the registry provider.
    //
    // For multi-container deployments the credentials need to cover every
    // container's tag — provider-side scoping (JFrog) is per-tag, so a
    // single-tag mint would only let the CLI push one of the N images.
    // The persisted container list (folded onto the deployment row) drives the
    // full tag set.
    let repository = project.name.clone();
    let push_tags = derive_push_tags(&deployment)?;
    let push_tag_refs: Vec<&str> = push_tags.iter().map(String::as_str).collect();

    let credentials = state
        .registry_provider
        .get_credentials(&repository, &push_tag_refs)
        .await
        .internal_err("Failed to get registry credentials")
        .map_err(|e| {
            e.with_context("project_name", &project_name)
                .with_context("repository", &repository)
        })?;

    // The target architecture is advertised separately via the platform
    // capabilities endpoint (`GET /api/v1/platform/capabilities`), which the
    // CLI consults to choose its build platform.
    Ok(Json(GetRegistryCredsResponse {
        credentials,
        repository,
    }))
}

/// Resolve the full set of image tags a deployment's CLI push needs to write.
///
/// Single-container deployments (legacy / `containers IS NULL`) push exactly
/// one image tagged with the deployment ID. Multi-container deployments push
/// one image per container, all sharing the project repository and tagged
/// `<deployment_id>-<container_name>`. The returned slice is what
/// `RegistryProvider::get_credentials` needs in order to scope the minted
/// credential to every push — critical for tag-scoped providers like JFrog.
///
/// Note: this derives a tag for *every* container, including pre-built ones the
/// CLI never pushes. That is an accepted, minor over-grant: the extra tags only
/// widen write access within the project's own repository, and the scope is
/// already bounded by the client-supplied container names — a tag-scoped mint
/// inherently trusts the client on which `<deployment_id>-<name>` tags it asks
/// for. Filtering pre-built containers out here would not change that trust.
fn derive_push_tags(
    deployment: &crate::db::models::Deployment,
) -> Result<Vec<String>, ServerError> {
    // Container side-data is folded onto the deployment row. A NULL `containers`
    // column is a legacy single-container deployment: one image tagged with the
    // deployment ID. A non-NULL but undecodable column is corrupt — fail with a
    // 500 rather than silently minting a credential scoped to the wrong tags.
    let container_specs: Vec<crate::server::deployment::models::ContainerSpec> =
        match deployment.containers.as_ref() {
            Some(v) => crate::server::deployment::models::decode_side_data(v).map_err(|e| {
                ServerError::internal(format!(
                    "Deployment {} ({}) has a non-NULL `containers` column that could not be \
                     decoded into Vec<ContainerSpec>: {:?}",
                    deployment.id, deployment.deployment_id, e
                ))
            })?,
            None => Vec::new(),
        };

    if container_specs.is_empty() {
        return Ok(vec![deployment.deployment_id.clone()]);
    }

    Ok(container_specs
        .iter()
        .map(|c| format!("{}-{}", deployment.deployment_id, c.name))
        .collect())
}
