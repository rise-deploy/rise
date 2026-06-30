use tracing::error;

use crate::db::deployments as db_deployments;
use crate::db::env_vars as db_env_vars;
use crate::db::models::{Deployment, Project};
use crate::server::error::{ServerError, ServerErrorExt};
use crate::server::extensions::InjectedEnvVarValue;
use crate::server::state::AppState;

pub use crate::deployment_id::generate_deployment_id;

/// Get the image tag for a deployment
///
/// This is the single source of truth for determining which image to use for a deployment.
/// For pre-built images: returns the digest-pinned reference from image_digest field
/// For build-from-source: constructs the full registry tag using registry configuration
/// For rollback deployments: uses the source deployment's deployment_id for the tag
///
/// # Arguments
/// * `state` - AppState containing registry provider configuration
/// * `deployment` - The deployment record
/// * `project` - The project record
///
/// # Returns
/// The fully-qualified image tag to use for docker pull
pub async fn get_deployment_image_tag(
    state: &AppState,
    deployment: &Deployment,
    project: &Project,
) -> String {
    // Pre-built images use the pinned digest
    if let Some(ref digest) = deployment.image_digest {
        return digest.clone();
    }

    if let Some(ref image_path) = deployment.image_path {
        return image_path.clone();
    }

    // For rollback deployments, use the source deployment's deployment_id for the image tag
    // This is because rollbacks don't build new images - they reuse the source deployment's image
    let deployment_id_for_tag =
        if let Some(source_deployment_id) = deployment.rolled_back_from_deployment_id {
            // Fetch the source deployment to get its deployment_id
            match db_deployments::find_by_id(&state.db_pool, source_deployment_id).await {
                Ok(Some(source_deployment)) => source_deployment.deployment_id,
                Ok(None) => {
                    tracing::warn!(
                        "Rollback deployment {} references non-existent source deployment {}",
                        deployment.deployment_id,
                        source_deployment_id
                    );
                    // Fallback to current deployment_id if source not found
                    deployment.deployment_id.clone()
                }
                Err(e) => {
                    tracing::error!(
                        "Failed to fetch source deployment {} for rollback {}: {}",
                        source_deployment_id,
                        deployment.deployment_id,
                        e
                    );
                    // Fallback to current deployment_id on error
                    deployment.deployment_id.clone()
                }
            }
        } else {
            // Regular build-from-source deployment
            deployment.deployment_id.clone()
        };

    // Build-from-source: construct from registry config using the appropriate deployment_id
    let registry_url = state.registry_provider.registry_url();
    format!(
        "{}/{}:{}",
        registry_url.trim_end_matches('/'),
        project.name,
        deployment_id_for_tag
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
