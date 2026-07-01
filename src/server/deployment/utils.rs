use anyhow::Result;
use sqlx::PgPool;
use tracing::error;

use crate::db::deployments as db_deployments;
use crate::db::env_vars as db_env_vars;
use crate::db::models::{Deployment, Project};
use crate::server::error::{ServerError, ServerErrorExt};
use crate::server::extensions::InjectedEnvVarValue;
use crate::server::registry::{ImageTagType, RegistryProvider};
use crate::server::state::AppState;

/// One-shot backfill of `deployments.image_path` for build-from-source rows
/// that predate the column. The value written matches what the pre-column
/// fallback in `resolve_image` would have reconstructed at pull time, so
/// pods keep pulling the same image after this runs — the point is only
/// to make the reference explicit in the DB so `resolve_image` can trust
/// `image_path` instead of falling back on the current `registry_url()`.
///
/// Runs from `AppState::new` after the registry provider is initialized and
/// migrations have applied. Pre-built rows (which pin their own image via
/// `image_digest`) are skipped.
pub async fn backfill_missing_image_paths(
    pool: &PgPool,
    registry_provider: &dyn RegistryProvider,
) -> Result<usize> {
    let rows = db_deployments::list_missing_image_paths(pool).await?;
    if rows.is_empty() {
        return Ok(0);
    }
    tracing::info!("Backfilling image_path for {} deployments", rows.len());
    for row in &rows {
        let image_path = registry_provider.get_image_tag(
            &row.project_name,
            &row.deployment_id,
            ImageTagType::Internal,
        );
        db_deployments::set_image_path(pool, row.id, &image_path).await?;
    }
    Ok(rows.len())
}

/// Resolve the image reference for a deployment. Single source of truth for
/// both the HTTP-response case and the controller reconciler.
///
/// Priority: `image_digest` (pre-built) → `image_path` (Internal form set at
/// deploy creation) → reconstruct via `registry_provider.get_image_tag`
/// (legacy fallback for rows persisted before `image_path` existed).
pub fn resolve_image(
    project: &Project,
    deployment: &Deployment,
    source_deployment_id: Option<&str>,
    registry_provider: &dyn RegistryProvider,
) -> String {
    if let Some(ref digest) = deployment.image_digest {
        return digest.clone();
    }
    if let Some(ref image_path) = deployment.image_path {
        return image_path.clone();
    }
    let id = source_deployment_id.unwrap_or(&deployment.deployment_id);
    registry_provider.get_image_tag(&project.name, id, ImageTagType::Internal)
}

/// Async wrapper that resolves a rollback's source deployment_id (needs a DB
/// lookup) and then delegates to `resolve_image`.
pub async fn get_deployment_image_tag(
    state: &AppState,
    deployment: &Deployment,
    project: &Project,
) -> String {
    let source_id = if let Some(source_deployment_id) = deployment.rolled_back_from_deployment_id {
        match db_deployments::find_by_id(&state.db_pool, source_deployment_id).await {
            Ok(Some(src)) => Some(src.deployment_id),
            Ok(None) => {
                tracing::warn!(
                    "Rollback deployment {} references non-existent source deployment {}",
                    deployment.deployment_id,
                    source_deployment_id
                );
                None
            }
            Err(e) => {
                tracing::error!(
                    "Failed to fetch source deployment {} for rollback {}: {}",
                    source_deployment_id,
                    deployment.deployment_id,
                    e
                );
                None
            }
        }
    } else {
        None
    };
    resolve_image(
        project,
        deployment,
        source_id.as_deref(),
        &*state.registry_provider,
    )
}

/// Create a deployment and invoke extension hooks
///
/// This is the single code path for creating deployments. It:
/// 1. Creates the deployment record in the database
/// 2. Invokes before_deployment hooks for all registered extensions
/// 3. Marks the deployment as failed if any extension hook fails
///
/// # Arguments
/// * `state` - AppState containing database pool and extension registry
/// * `params` - Parameters for creating the deployment
/// * `project` - The project this deployment belongs to
///
/// # Returns
/// The created deployment on success, or a ServerError
pub async fn create_deployment_with_hooks(
    state: &AppState,
    params: db_deployments::CreateDeploymentParams<'_>,
    project: &Project,
) -> Result<Deployment, ServerError> {
    // Extract deployment_group before moving params (needed for extension hooks)
    let deployment_group = params.deployment_group.to_string();

    // Create the deployment record
    let deployment = db_deployments::create(&state.db_pool, params)
        .await
        .internal_err("Failed to create deployment")?;

    // Call before_deployment hooks for all registered extensions
    for (_, extension) in state.extension_registry.iter() {
        let vars = match extension
            .before_deployment(project.id, &deployment_group)
            .await
        {
            Ok(vars) => vars,
            Err(e) => {
                let error_msg = format!(
                    "Extension type '{}' failed: {}",
                    extension.extension_type(),
                    e
                );
                if let Err(mark_err) =
                    db_deployments::mark_failed(&state.db_pool, deployment.id, &error_msg).await
                {
                    error!(
                        "Failed to mark deployment as failed after extension error: {:?}",
                        mark_err
                    );
                }

                return Err(ServerError::internal_anyhow(
                    e,
                    format!(
                        "Extension type '{}' before_deployment hook failed",
                        extension.extension_type()
                    ),
                ));
            }
        };

        // Write returned env vars to deployment_env_vars
        for var in vars {
            let (value, is_secret, is_protected) = match var.value {
                InjectedEnvVarValue::Plain(v) => (v, false, false),
                InjectedEnvVarValue::Secret { encrypted, .. } => (encrypted, true, false),
                InjectedEnvVarValue::Protected { encrypted, .. } => (encrypted, true, true),
            };

            if let Err(e) = db_env_vars::upsert_deployment_env_var(
                &state.db_pool,
                deployment.id,
                &var.key,
                &value,
                is_secret,
                is_protected,
                "extension",
            )
            .await
            {
                let error_msg = format!("Failed to write env var '{}'", var.key);
                if let Err(mark_err) =
                    db_deployments::mark_failed(&state.db_pool, deployment.id, &error_msg).await
                {
                    error!(
                        "Failed to mark deployment as failed after env var write error: {:?}",
                        mark_err
                    );
                }

                return Err(ServerError::internal_anyhow(e, error_msg));
            }
        }
    }

    Ok(deployment)
}
