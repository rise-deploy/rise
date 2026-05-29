use anyhow::Context;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::sse::{Event, KeepAlive, Sse},
    Json,
};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use regex::Regex;
use std::sync::LazyLock;
use tracing::{debug, error, info, warn};

use super::models::{self, *};
use super::state_machine;
use super::utils::{create_deployment_with_hooks, generate_deployment_id};
use crate::db::models::DeploymentStatus as DbDeploymentStatus;
use crate::db::{deployments as db_deployments, projects, service_accounts, users};
use crate::server::auth::context::AuthContext;
use crate::server::error::{ServerError, ServerErrorExt};
use crate::server::registry::ImageTagType;
use crate::server::state::AppState;

/// Validate group name format: must be 'default' or match [a-z0-9][a-z0-9/-]*[a-z0-9]
/// with additional constraints:
/// - No consecutive hyphens (`--`) to avoid collisions with normalized names
///   (e.g. `mr/123` normalizes to `mr--123`, so `mr--123` as a raw group name is disallowed)
/// - Normalized result must be <= 63 chars (Kubernetes label value limit)
fn is_valid_group_name(name: &str) -> bool {
    if name == models::DEFAULT_DEPLOYMENT_GROUP {
        return true;
    }

    if name.len() > 100 {
        return false;
    }

    // Disallow consecutive hyphens to prevent collisions with normalized names
    if name.contains("--") {
        return false;
    }

    let valid_pattern = Regex::new(r"^[a-z0-9][a-z0-9/-]*[a-z0-9]$")
        .unwrap()
        .is_match(name);

    if !valid_pattern {
        return false;
    }

    // Ensure the normalized result fits within Kubernetes label value limit (63 chars)
    let normalized = models::normalize_deployment_group(name);
    if normalized.len() > 63 {
        return false;
    }

    true
}

/// Parse expiration duration string (e.g., "7d", "2h", "30m") to DateTime
fn parse_expiration(expires_in: &str) -> Result<DateTime<Utc>, String> {
    let s = expires_in.trim();
    let (num_str, unit) = if let Some(num_str) = s.strip_suffix('d') {
        (num_str, "d")
    } else if let Some(num_str) = s.strip_suffix('h') {
        (num_str, "h")
    } else if let Some(num_str) = s.strip_suffix('m') {
        (num_str, "m")
    } else {
        return Err("Duration must end with d, h, or m".to_string());
    };

    let num: i64 = num_str
        .parse()
        .map_err(|_| "Invalid number in duration".to_string())?;

    if num <= 0 {
        return Err("Duration must be positive".to_string());
    }

    let duration = match unit {
        "d" => chrono::Duration::days(num),
        "h" => chrono::Duration::hours(num),
        "m" => chrono::Duration::minutes(num),
        _ => return Err("Invalid duration unit".to_string()),
    };

    Ok(Utc::now() + duration)
}

/// Resolve image tag to digest by contacting OCI registry directly
///
/// This function uses the OCI Distribution API to fetch the image manifest
/// (without pulling the entire image) and returns the digest-pinned reference.
/// Docker Hub shorthand (e.g. `nginx:latest`) is handled by the OCI client internally.
///
/// # Arguments
/// * `oci_client` - OCI client for registry interaction
/// * `registry_provider` - Registry provider for credentials
/// * `image_ref` - Image reference (e.g., "registry:5000/repo/app:tag" or "nginx:latest")
/// * `project_name` - Project name for scoped pull credentials and cross-project validation
///
/// # Returns
/// Fully-qualified digest reference (e.g., "docker.io/library/nginx@sha256:abc123...")
///
/// # Errors
/// Returns error if image doesn't exist, requires authentication, registry is unreachable,
/// or the image belongs to a different Rise project.
async fn resolve_image_digest(
    oci_client: &crate::server::oci::OciClient,
    registry_provider: &std::sync::Arc<dyn crate::server::registry::RegistryProvider>,
    image_ref: &str,
    project_name: &str,
) -> anyhow::Result<String> {
    // Cross-project image validation: if the image is on the Rise registry,
    // verify it belongs to this project (defense-in-depth).
    // Use registry_url() (which includes namespace/repo_key) to correctly match
    // Rise-managed images rather than just the hostname.
    let registry_url = registry_provider.registry_url();
    let prefix = format!("{}/", registry_url.trim_end_matches('/'));
    let is_rise_image = if let Some(image_path) = image_ref.strip_prefix(&prefix) {
        // After stripping the registry URL prefix, the first path segment is the project name.
        let image_project = image_path.split([':', '/', '@']).next().unwrap_or("");
        if image_project != project_name {
            anyhow::bail!(
                "Image belongs to a different Rise project '{}' — \
                 deployments can only use images from their own project '{}'",
                image_project,
                project_name
            );
        }
        true
    } else {
        false
    };

    // Only fetch scoped pull credentials for Rise-managed images.
    // External images use anonymous auth.
    let mut credentials = crate::server::oci::RegistryCredentialsMap::new();

    if is_rise_image {
        let registry_host = registry_provider.registry_host();
        match registry_provider
            .get_k8s_pull_credentials(project_name)
            .await
        {
            Ok(creds) if !creds.username.is_empty() => {
                debug!(
                    "Adding project-scoped credentials for registry host: {}",
                    registry_host
                );
                credentials.insert(registry_host.to_string(), (creds.username, creds.password));
            }
            Ok(_) => {
                debug!("Registry provider returned empty credentials, using anonymous auth");
            }
            Err(e) => {
                error!(
                    "Failed to get pull credentials from registry provider: {:?}",
                    e
                );
                // Continue with anonymous auth
            }
        }
    }

    let digest_ref = oci_client
        .resolve_image_digest(image_ref, &credentials)
        .await
        .context(format!("Failed to resolve image '{}'", image_ref))?;

    info!("Resolved '{}' to digest '{}'", image_ref, digest_ref);
    Ok(digest_ref)
}

/// Convert API DeploymentStatus to DB DeploymentStatus
fn convert_status_to_db(status: DeploymentStatus) -> DbDeploymentStatus {
    match status {
        DeploymentStatus::Pending => DbDeploymentStatus::Pending,
        DeploymentStatus::Building => DbDeploymentStatus::Building,
        DeploymentStatus::Pushing => DbDeploymentStatus::Pushing,
        DeploymentStatus::Pushed => DbDeploymentStatus::Pushed,
        DeploymentStatus::Deploying => DbDeploymentStatus::Deploying,
        DeploymentStatus::Healthy => DbDeploymentStatus::Healthy,
        DeploymentStatus::Unhealthy => DbDeploymentStatus::Unhealthy,
        DeploymentStatus::Cancelling => DbDeploymentStatus::Cancelling,
        DeploymentStatus::Cancelled => DbDeploymentStatus::Cancelled,
        DeploymentStatus::Terminating => DbDeploymentStatus::Terminating,
        DeploymentStatus::Stopped => DbDeploymentStatus::Stopped,
        DeploymentStatus::Superseded => DbDeploymentStatus::Superseded,
        DeploymentStatus::Failed => DbDeploymentStatus::Failed,
        DeploymentStatus::Expired => DbDeploymentStatus::Expired,
    }
}

/// Insert Rise-provided environment variables into a deployment.
///
/// Inserts the system env vars generated by [`models::rise_system_env_vars`]:
/// RISE_ISSUER, RISE_APP_URL, RISE_APP_URLS, RISE_DEPLOYMENT_GROUP, RISE_DEPLOYMENT_GROUP_NORMALIZED.
///
/// These environment variables are visible in the Rise UI and allow deployed applications
/// to validate Rise-issued JWTs (via /.well-known/openid-configuration), call Rise APIs,
/// and know their own URLs and deployment context.
async fn insert_rise_env_vars(
    state: &AppState,
    deployment: &crate::db::models::Deployment,
    project: &crate::db::models::Project,
) -> Result<(), ServerError> {
    let deployment_urls = state
        .deployment_backend
        .get_deployment_urls(deployment, project)
        .await
        .internal_err("Failed to get deployment URLs")?;

    // Resolve environment name for RISE_ENVIRONMENT
    let environment_name = if let Some(env_id) = deployment.environment_id {
        crate::db::environments::find_by_id(&state.db_pool, env_id)
            .await
            .ok()
            .flatten()
            .map(|e| e.name)
    } else {
        None
    };

    let vars = models::rise_system_env_vars(
        &state.public_url,
        &deployment.deployment_group,
        &deployment_urls,
        environment_name.as_deref(),
    );

    for (key, value) in &vars {
        crate::db::env_vars::upsert_deployment_env_var(
            &state.db_pool,
            deployment.id,
            key,
            value,
            false, // Not a secret
            false, // is_protected
            "system",
        )
        .await
        .internal_err(format!("Failed to insert {}", key))?;
    }

    info!(
        "Inserted Rise environment variables for deployment {}",
        deployment.id
    );
    Ok(())
}

/// Filter env overrides by target environment.
///
/// - `for_environment: None` → always included (global override)
/// - `for_environment: Some(name)` → included only if name matches `resolved_environment`
///
/// Logs a warning for each override that was dropped because its `for_environment`
/// didn't match the resolved environment.
fn filter_env_overrides_by_environment<'a>(
    overrides: &'a [models::EnvOverride],
    resolved_environment: Option<&str>,
) -> Vec<&'a models::EnvOverride> {
    let mut result = Vec::new();
    for o in overrides {
        match &o.for_environment {
            None => result.push(o),
            Some(env_name) => {
                if resolved_environment == Some(env_name.as_str()) {
                    result.push(o);
                } else {
                    warn!(
                        "Env override '{}' skipped: for_environment '{}' does not match resolved environment {:?}",
                        o.key, env_name, resolved_environment
                    );
                }
            }
        }
    }
    result
}

/// Apply env var overrides from the deployment request.
///
/// Encrypts secret values and upserts each override into the deployment's env vars.
/// Called after copying project/source env vars and before upserting PORT.
///
/// Only client-allowed source values (`toml`, `cli`) are accepted. Unknown or
/// server-managed source values are replaced with `cli`.
/// Outcome of `resolve_multi_container_images`.
///
/// Holds everything the caller needs to: mint registry credentials with the
/// right scope, persist the deployment's container side-data, and return the
/// per-container image map to the CLI.
#[cfg(feature = "backend")]
struct ResolvedContainerPayload {
    /// Containers with `image` filled in for every entry. Server-derived
    /// entries hold an internal-facing reference (what the cluster pulls);
    /// pre-built entries are pinned to an immutable `repo@sha256:…` digest so
    /// a mutable tag can't drift across pod restarts.
    resolved_specs: Vec<crate::server::deployment::models::ContainerSpec>,
    /// Tag suffixes for which the server is going to derive image tags and
    /// the CLI is expected to push (i.e. containers whose request entry had
    /// `image == None`). Used to scope the credential mint — JFrog needs
    /// every tag listed; ECR/GitLab scope by repository and ignore them.
    push_tags: Vec<String>,
    /// Map of container_name → fully-qualified CLIENT-facing image tag the
    /// CLI should `docker build -t` and `docker push` to. Pre-built
    /// containers map to their resolved immutable digest (exactly what the
    /// cluster runs) so the CLI logs the real image and can skip the push.
    container_images: std::collections::BTreeMap<String, String>,
}

/// Normalize the container list in a `CreateDeploymentRequest`.
///
/// For each container with no `image` set, derive both an internal-facing
/// tag (persisted on the row, used by the K8s controller to pull) and a
/// client-facing tag (returned to the CLI so it knows what to push).
/// Containers that came in with an explicit `image` are pre-built: their
/// reference is resolved to an immutable `repo@sha256:…` digest (mirroring the
/// single-container `--image` path) and persisted pinned, so a mutable tag
/// like `redis:7-alpine` can't drift across pod restarts under
/// `imagePullPolicy: Always`. Pre-built containers are excluded from
/// `push_tags` (no push is expected for them).
///
/// Returns an error (surfaced as a `400`) if any pre-built image can't be
/// resolved — the whole deployment is rejected rather than shipping with an
/// unresolvable image, matching the single-container behaviour.
#[cfg(feature = "backend")]
async fn resolve_multi_container_images(
    state: &AppState,
    project_name: &str,
    deployment_id: &str,
    specs: &[crate::server::deployment::models::ContainerSpec],
) -> Result<ResolvedContainerPayload, ServerError> {
    use crate::server::registry::ImageTagType;

    let mut resolved_specs = Vec::with_capacity(specs.len());
    let mut push_tags = Vec::new();
    let mut container_images = std::collections::BTreeMap::new();

    for spec in specs {
        let mut resolved = spec.clone();
        let tag_suffix = format!("{}-{}", deployment_id, spec.name);

        let client_facing_tag = match spec.image.as_deref() {
            Some(image) if !image.is_empty() => {
                // Pre-built — CLI doesn't push, server doesn't derive. Pin the
                // reference to an immutable digest so `imagePullPolicy: Always`
                // can't pull a different image on a later pod restart (matches
                // the single-container `--image` flow). The reconciler uses
                // `spec.image` verbatim as the pod image, so persisting the
                // pinned ref is all that's required.
                let pinned = resolve_image_digest(
                    &state.oci_client,
                    &state.registry_provider,
                    image,
                    project_name,
                )
                .await
                .map_err(|e| {
                    ServerError::bad_request(format!(
                        "Failed to resolve image '{}' for container '{}': {}",
                        image, spec.name, e
                    ))
                })?;
                resolved.image = Some(pinned.clone());
                pinned
            }
            _ => {
                // Server-derived. Persist the internal tag (controller-facing,
                // possibly cluster-local), return the client tag to the CLI.
                let internal = state.registry_provider.get_image_tag(
                    project_name,
                    &tag_suffix,
                    ImageTagType::Internal,
                );
                resolved.image = Some(internal);
                push_tags.push(tag_suffix.clone());
                state.registry_provider.get_image_tag(
                    project_name,
                    &tag_suffix,
                    ImageTagType::ClientFacing,
                )
            }
        };

        container_images.insert(spec.name.clone(), client_facing_tag);
        resolved_specs.push(resolved);
    }

    Ok(ResolvedContainerPayload {
        resolved_specs,
        push_tags,
        container_images,
    })
}

/// Serialise the resolved multi-container side-data so it can be passed into
/// the deployments INSERT atomically (avoids the legacy "row exists but
/// container JSON failed to persist" hazard).
///
/// Backfills `replicas`/`cpu`/`memory` from the deployment-level effective
/// values when a container leaves them unset, so the reconciler can read the
/// JSON directly without re-deriving anything.
#[cfg(feature = "backend")]
fn build_container_side_data_json(
    resolved_specs: &[crate::server::deployment::models::ContainerSpec],
    routes: &[crate::server::deployment::models::RouteSpec],
    effective_replicas: u32,
    effective_cpu: &str,
    effective_memory: &str,
) -> Result<(serde_json::Value, Option<serde_json::Value>), ServerError> {
    let mut backfilled = resolved_specs.to_vec();
    for spec in &mut backfilled {
        if spec.replicas.is_none() {
            spec.replicas = Some(effective_replicas);
        }
        if spec.cpu.is_none() {
            spec.cpu = Some(effective_cpu.to_string());
        }
        if spec.memory.is_none() {
            spec.memory = Some(effective_memory.to_string());
        }
    }
    // Wrap both columns in the versioned side-data envelope so the persisted
    // shape can evolve (see `models::CONTAINER_SIDE_DATA_VERSION`).
    let containers_json =
        models::encode_side_data(&backfilled).internal_err("Failed to serialise containers")?;
    let routes_json = if routes.is_empty() {
        None
    } else {
        Some(models::encode_side_data(routes).internal_err("Failed to serialise routes")?)
    };
    Ok((containers_json, routes_json))
}

/// Inherit the source deployment's container side-data verbatim. Used by the
/// rollback path when the redeploy request itself doesn't carry a `containers`
/// block — without this, a redeploy of a multi-container source would silently
/// fall back to the single-container path and the reconciler would drop every
/// per-container K8s resource. The side-data is folded onto the source
/// [`Deployment`] row, so this is a plain clone with no extra query.
#[cfg(feature = "backend")]
fn inherit_container_side_data_from_source(
    source_deployment: &crate::db::models::Deployment,
) -> (Option<serde_json::Value>, Option<serde_json::Value>) {
    (
        source_deployment.containers.clone(),
        source_deployment.routes.clone(),
    )
}

async fn apply_env_overrides(
    state: &AppState,
    deployment_id: uuid::Uuid,
    overrides: &[models::EnvOverride],
    resolved_environment: Option<&str>,
) -> Result<(), ServerError> {
    use crate::db::models::EnvVarSource;

    if overrides.is_empty() {
        return Ok(());
    }

    let filtered = filter_env_overrides_by_environment(overrides, resolved_environment);

    if filtered.is_empty() {
        return Ok(());
    }

    info!(
        "Applying {} env override(s) to deployment {} ({} total, environment: {:?})",
        filtered.len(),
        deployment_id,
        overrides.len(),
        resolved_environment,
    );

    for env_override in filtered {
        let is_protected = validate_env_override(env_override)?;

        // Validate source: only client-allowed values (toml, cli) are accepted.
        // Default to "cli" if missing or invalid.
        let source = env_override
            .source
            .as_deref()
            .and_then(EnvVarSource::parse)
            .filter(|s| s.is_client_allowed())
            .unwrap_or(EnvVarSource::Cli);

        // Encrypt if secret
        let value_to_store = if env_override.is_secret {
            let provider = state.encryption_provider.as_ref().ok_or_else(|| {
                ServerError::bad_request(
                    "Cannot store secret variables: no encryption provider configured",
                )
            })?;
            provider
                .encrypt(&env_override.value)
                .await
                .internal_err(format!("Failed to encrypt secret '{}'", env_override.key))?
        } else {
            env_override.value.clone()
        };

        let source_str = source.as_str();
        crate::db::env_vars::upsert_deployment_env_var(
            &state.db_pool,
            deployment_id,
            &env_override.key,
            &value_to_store,
            env_override.is_secret,
            is_protected,
            &source_str,
        )
        .await
        .internal_err(format!("Failed to set env override '{}'", env_override.key))?;
    }

    Ok(())
}

fn validate_env_override_key(key: &str) -> bool {
    !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn normalize_env_override_is_protected(env_override: &models::EnvOverride) -> bool {
    env_override.is_protected.unwrap_or(env_override.is_secret)
}

fn validate_env_override(env_override: &models::EnvOverride) -> Result<bool, ServerError> {
    if !validate_env_override_key(&env_override.key) {
        return Err(ServerError::bad_request(format!(
            "Invalid env var key '{}' (must be alphanumeric with underscores)",
            env_override.key
        )));
    }

    if env_override.key == "PORT" {
        return Err(ServerError::bad_request(
            "PORT cannot be set via env overrides. Use http_port/--http-port instead.",
        ));
    }

    let is_protected = normalize_env_override_is_protected(env_override);
    if is_protected && !env_override.is_secret {
        return Err(ServerError::bad_request(format!(
            "Env override '{}' cannot be protected unless it is also secret.",
            env_override.key
        )));
    }

    Ok(is_protected)
}

fn validate_env_overrides(overrides: &[models::EnvOverride]) -> Result<(), ServerError> {
    for env_override in overrides {
        validate_env_override(env_override)?;
    }

    Ok(())
}

/// Convert DB DeploymentStatus to API DeploymentStatus
fn convert_status_from_db(status: DbDeploymentStatus) -> DeploymentStatus {
    match status {
        DbDeploymentStatus::Pending => DeploymentStatus::Pending,
        DbDeploymentStatus::Building => DeploymentStatus::Building,
        DbDeploymentStatus::Pushing => DeploymentStatus::Pushing,
        DbDeploymentStatus::Pushed => DeploymentStatus::Pushed,
        DbDeploymentStatus::Deploying => DeploymentStatus::Deploying,
        DbDeploymentStatus::Healthy => DeploymentStatus::Healthy,
        DbDeploymentStatus::Unhealthy => DeploymentStatus::Unhealthy,
        DbDeploymentStatus::Cancelling => DeploymentStatus::Cancelling,
        DbDeploymentStatus::Cancelled => DeploymentStatus::Cancelled,
        DbDeploymentStatus::Terminating => DeploymentStatus::Terminating,
        DbDeploymentStatus::Stopped => DeploymentStatus::Stopped,
        DbDeploymentStatus::Superseded => DeploymentStatus::Superseded,
        DbDeploymentStatus::Failed => DeploymentStatus::Failed,
        DbDeploymentStatus::Expired => DeploymentStatus::Expired,
    }
}

/// Fetch the creator email for a deployment
async fn get_creator_email(pool: &sqlx::PgPool, created_by_id: uuid::Uuid) -> String {
    match users::find_by_id(pool, created_by_id).await {
        Ok(Some(user)) => user.email,
        _ => "unknown".to_string(),
    }
}

/// Convert DB Deployment to API Deployment
///
/// Dynamically calculates the image tag when not stored in the database,
/// using the registry provider's configuration to construct the full image reference.
async fn convert_deployment(
    state: &AppState,
    deployment: crate::db::models::Deployment,
    project: &crate::db::models::Project,
    created_by_email: String,
    primary_url: Option<String>,
    custom_domain_urls: Vec<String>,
    all_urls: Vec<String>,
) -> Deployment {
    // Multi-container side-data, folded onto the row. `None` is a legacy
    // single-container deployment — the frontend reads the flat replicas/cpu/
    // memory fields in that case. A present-but-unparseable column is corrupt:
    // log it (the reconciler will have marked the deployment Failed, surfacing
    // the problem) but render best-effort rather than 500-ing a whole list
    // because one row is bad.
    let containers = match deployment.containers.as_ref() {
        Some(v) => {
            match models::decode_side_data::<crate::server::deployment::models::ContainerSpec>(v) {
                Ok(specs) => Some(specs),
                Err(e) => {
                    error!(
                        deployment_id = %deployment.deployment_id,
                        "non-NULL `containers` column could not be decoded into \
                         Vec<ContainerSpec>; rendering without container side-data: {:?}", e
                    );
                    None
                }
            }
        }
        None => None,
    };
    let is_multi_container = containers.as_ref().is_some_and(|c| !c.is_empty());

    // Backfill the deployment-level image for display.
    // - Pre-built single-container: `deployment.image` is already set.
    // - Build-from-source single-container: synthesise the internal registry tag.
    // - Multi-container: there is no single deployment-level image — the
    //   per-container images live in `containers`. Leave it unset rather than
    //   synthesising a `<registry>/<project>:<deployment_id>` tag that is never
    //   pushed (which would surface a bogus image in the CLI/UI).
    let image = if deployment.image.is_some() {
        deployment.image.clone()
    } else if is_multi_container {
        None
    } else {
        Some(super::utils::get_deployment_image_tag(state, &deployment, project).await)
    };
    let can_rollback = state_machine::can_create_from(&deployment);

    // Resolve environment name and color
    let (environment, environment_color) = if let Some(env_id) = deployment.environment_id {
        let env = crate::db::environments::find_by_id(&state.db_pool, env_id)
            .await
            .ok()
            .flatten();
        (env.as_ref().map(|e| e.name.clone()), env.map(|e| e.color))
    } else {
        (None, None)
    };

    Deployment {
        id: deployment.id.to_string(),
        deployment_id: deployment.deployment_id,
        project: deployment.project_id.to_string(),
        created_by: deployment.created_by_id.to_string(),
        created_by_email,
        status: convert_status_from_db(deployment.status),
        deployment_group: deployment.deployment_group,
        environment,
        environment_color,
        expires_at: deployment.expires_at.map(|dt| dt.to_rfc3339()),
        error_message: deployment.error_message,
        completed_at: deployment.completed_at.map(|dt| dt.to_rfc3339()),
        build_logs: deployment.build_logs,
        controller_metadata: deployment.controller_metadata,
        primary_url,
        custom_domain_urls,
        all_urls,
        image,
        image_digest: deployment.image_digest,
        http_port: deployment.http_port as u16,
        is_active: deployment.is_active,
        can_rollback,
        replicas: deployment.replicas as u32,
        cpu: deployment.cpu,
        memory: deployment.memory,
        containers,
        job_url: deployment.job_url,
        pull_request_url: deployment.pull_request_url,
        git_repository_url: deployment.git_repository_url,
        created: deployment.created_at.to_rfc3339(),
        updated: deployment.updated_at.to_rfc3339(),
    }
}

/// Resolve the deployment target (group + environment) from request parameters.
///
/// Rules:
/// - If both specified: use both, but validate that the group is not the primary
///   deployment group of a *different* environment.
/// - If environment specified, no group: use environment's primary deployment group.
/// - If group specified, no environment: look up environment whose primary group matches;
///   then auto-resolve (0 envs → none, 1 env → use it, 2+ → error).
/// - If neither specified: if the project has exactly one environment, use it.
///   If zero, use the default group with no environment. If more than one, error.
async fn resolve_deployment_target(
    pool: &sqlx::PgPool,
    project_id: uuid::Uuid,
    requested_environment: Option<&str>,
    requested_group: Option<&str>,
) -> Result<(String, Option<crate::db::models::Environment>), ServerError> {
    use crate::db::environments;

    match (requested_environment, requested_group) {
        // Both specified
        (Some(env_name), Some(group)) => {
            let env = environments::find_by_name(pool, project_id, env_name)
                .await
                .internal_err("Failed to look up environment")?
                .ok_or_else(|| {
                    ServerError::not_found(format!("Environment '{}' not found", env_name))
                })?;
            // Validate: if the group is the primary group of another environment, reject
            if let Some(primary_env) = environments::find_by_primary_group(pool, project_id, group)
                .await
                .internal_err("Failed to look up environment by group")?
            {
                if primary_env.name != env_name {
                    return Err(ServerError::bad_request(format!(
                        "Group '{}' is the primary deployment group of environment '{}', and cannot be used with environment '{}'.",
                        group, primary_env.name, env_name
                    )));
                }
            }
            Ok((group.to_string(), Some(env)))
        }
        // Environment only, no group
        (Some(env_name), None) => {
            let env = environments::find_by_name(pool, project_id, env_name)
                .await
                .internal_err("Failed to look up environment")?
                .ok_or_else(|| {
                    ServerError::not_found(format!("Environment '{}' not found", env_name))
                })?;
            let group = env.primary_deployment_group.clone().ok_or_else(|| {
                ServerError::bad_request(format!(
                    "Environment '{}' has no primary deployment group. Specify a deployment group explicitly.",
                    env_name
                ))
            })?;
            Ok((group, Some(env)))
        }
        // Group only, no environment
        (None, Some(group)) => {
            // Check if this group is the primary group of an environment
            let env = environments::find_by_primary_group(pool, project_id, group)
                .await
                .internal_err("Failed to look up environment by group")?;
            if env.is_some() {
                Ok((group.to_string(), env))
            } else {
                // No primary group match. Auto-resolve from available environments.
                let all_envs = environments::list_for_project(pool, project_id)
                    .await
                    .internal_err("Failed to list environments")?;
                match all_envs.len() {
                    0 => Ok((group.to_string(), None)),
                    1 => Ok((
                        group.to_string(),
                        Some(all_envs.into_iter().next().unwrap()),
                    )),
                    _ => Err(ServerError::bad_request(
                        "Multiple environments configured. Specify an environment to select one.",
                    )),
                }
            }
        }
        // Neither specified: auto-resolve from available environments
        (None, None) => {
            let all_envs = environments::list_for_project(pool, project_id)
                .await
                .internal_err("Failed to list environments")?;
            match all_envs.len() {
                0 => Ok((models::DEFAULT_DEPLOYMENT_GROUP.to_string(), None)),
                1 => {
                    let env = all_envs.into_iter().next().unwrap();
                    let group = env
                        .primary_deployment_group
                        .clone()
                        .unwrap_or_else(|| models::DEFAULT_DEPLOYMENT_GROUP.to_string());
                    Ok((group, Some(env)))
                }
                _ => Err(ServerError::bad_request(
                    "Multiple environments configured. Specify an environment to select one.",
                )),
            }
        }
    }
}

/// Resolve the effective HTTP port for a deployment.
///
/// Priority:
/// 1. Explicit http_port from request (if provided)
/// 2. PORT env var from project (if set and valid)
/// 3. Default: 8080
async fn resolve_effective_http_port(
    state: &AppState,
    project_id: uuid::Uuid,
    explicit_port: Option<u16>,
) -> Result<u16, ServerError> {
    // 1. Explicit port takes precedence
    if let Some(port) = explicit_port {
        return Ok(port);
    }

    // 2. Check project's PORT env var
    let project_env_vars =
        crate::db::env_vars::list_project_env_vars(&state.db_pool, project_id, None)
            .await
            .internal_err("Failed to list project environment variables")?;

    if let Some(port_var) = project_env_vars.iter().find(|v| v.key == "PORT") {
        if let Ok(port) = port_var.value.parse::<u16>() {
            if port > 0 {
                debug!("Using PORT {} from project environment variable", port);
                return Ok(port);
            }
        }
        // Invalid PORT value - warn but fall through to default
        debug!(
            "Project PORT env var '{}' is not a valid port number, using default",
            port_var.value
        );
    }

    // 3. Default to 8080
    debug!("No explicit port or PORT env var, defaulting to 8080");
    Ok(8080)
}

/// Validate deployment resources against effective constraints.
///
/// Constraints are resolved from: environment-specific overrides > platform defaults.
/// When `is_redeploy` is true, error messages include a hint about using CLI flags to override
/// inherited resource values.
static IDENTITY_AUDIENCE_NAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9._-]+$").unwrap());

/// Validate `[identity].audiences` from a deploy request.
///
/// Keys become in-pod token filenames, so they must be safe path segments.
/// Audience values must be non-empty.
fn validate_identity_audiences(
    audiences: &std::collections::BTreeMap<String, String>,
) -> Result<(), ServerError> {
    for (name, audience) in audiences {
        if name.is_empty()
            || name == "."
            || name == ".."
            || !IDENTITY_AUDIENCE_NAME_RE.is_match(name)
        {
            return Err(ServerError::bad_request(format!(
                "Invalid identity audience name '{}'; use only letters, numbers, '.', '_' or '-'",
                name
            )));
        }
        if audience.trim().is_empty() {
            return Err(ServerError::bad_request(format!(
                "Identity audience '{}' has an empty value",
                name
            )));
        }
    }
    Ok(())
}

/// Project the resource-check inputs for `validate_resource_constraints`.
///
/// For single-container requests (`containers` is `None`) returns the single
/// effective values verbatim. For multi-container, sums replicas across
/// containers (the platform/environment replica cap applies to the whole
/// deployment) and collects per-container `(cpu, memory)` so range checks
/// still run against each container individually.
///
/// Per-container fields that are unset inherit the deployment-wide
/// `effective_*` defaults, matching how the reconciler resolves them.
#[cfg(feature = "backend")]
fn resolve_resource_check_inputs<'a>(
    containers: Option<&'a [crate::server::deployment::models::ContainerSpec]>,
    effective_replicas: u32,
    effective_cpu: &'a str,
    effective_memory: &'a str,
) -> (u32, Vec<(&'a str, &'a str)>) {
    match containers {
        Some(specs) if !specs.is_empty() => {
            let total_replicas: u32 = specs
                .iter()
                .map(|s| s.replicas.unwrap_or(effective_replicas))
                .sum();
            let per_container: Vec<(&str, &str)> = specs
                .iter()
                .map(|s| {
                    let cpu = s.cpu.as_deref().unwrap_or(effective_cpu);
                    let mem = s.memory.as_deref().unwrap_or(effective_memory);
                    (cpu, mem)
                })
                .collect();
            (total_replicas, per_container)
        }
        _ => (effective_replicas, vec![(effective_cpu, effective_memory)]),
    }
}

/// Validate that the K8s resource names a deployment will generate stay within
/// Kubernetes limits. The per-container Service name `<group>-<container>` must
/// be a DNS-1035 label (≤ 63 chars) — the binding constraint, since the
/// deployment group and container name share that budget. The per-container
/// Deployment name `<project>-<deployment_id>-<container>` must be a DNS-1123
/// subdomain (≤ 253 chars). Metacontroller surfaces no apply error for an
/// over-limit name back to the deployment, so we reject at request time.
#[cfg(feature = "backend")]
fn validate_container_resource_names(
    project_name: &str,
    deployment_group: &str,
    deployment_id: &str,
    container_names: &[&str],
) -> Result<(), ServerError> {
    use crate::server::deployment::resource_builder::ResourceBuilder;
    let group = ResourceBuilder::escaped_group_name(deployment_group);
    for name in container_names {
        let service_name = format!("{group}-{name}");
        if service_name.len() > 63 {
            return Err(ServerError::bad_request(format!(
                "Container '{name}' would produce Service name '{service_name}' ({} chars), \
                 over Kubernetes' 63-character limit. Shorten the deployment group or the container name.",
                service_name.len()
            )));
        }
        let deployment_name = format!("{project_name}-{deployment_id}-{name}");
        if deployment_name.len() > 253 {
            return Err(ServerError::bad_request(format!(
                "Container '{name}' would produce Deployment name '{deployment_name}' ({} chars), \
                 over Kubernetes' 253-character limit. Shorten the project or container name.",
                deployment_name.len()
            )));
        }
    }
    Ok(())
}

#[cfg(feature = "backend")]
fn validate_resource_constraints(
    state: &AppState,
    resolved_environment: &Option<crate::db::models::Environment>,
    // Total replicas across the deployment. For single-container this is the
    // container's `replicas`; for multi-container it MUST be the sum across
    // all containers — the limit applies to the deployment as a whole.
    total_replicas: u32,
    // (cpu, memory) for each container. CPU and memory ranges are enforced
    // per-container so a worker can't dodge limits by joining a deployment
    // alongside a tiny frontend. For single-container, pass one entry.
    per_container_resources: &[(&str, &str)],
    is_redeploy: bool,
) -> Result<(), ServerError> {
    let platform_constraints = state
        .deployment_constraints
        .as_ref()
        .cloned()
        .unwrap_or_default();

    let eff_min_replicas = resolved_environment
        .as_ref()
        .and_then(|e| e.min_replicas)
        .map(|v| v as u32)
        .unwrap_or(platform_constraints.min_replicas);
    let eff_max_replicas = resolved_environment
        .as_ref()
        .and_then(|e| e.max_replicas)
        .map(|v| v as u32)
        .unwrap_or(platform_constraints.max_replicas);
    let eff_min_cpu = resolved_environment
        .as_ref()
        .and_then(|e| e.min_cpu.as_deref())
        .unwrap_or(&platform_constraints.min_cpu);
    let eff_max_cpu = resolved_environment
        .as_ref()
        .and_then(|e| e.max_cpu.as_deref())
        .unwrap_or(&platform_constraints.max_cpu);
    let eff_min_memory = resolved_environment
        .as_ref()
        .and_then(|e| e.min_memory.as_deref())
        .unwrap_or(&platform_constraints.min_memory);
    let eff_max_memory = resolved_environment
        .as_ref()
        .and_then(|e| e.max_memory.as_deref())
        .unwrap_or(&platform_constraints.max_memory);

    let redeploy_hint = if is_redeploy {
        ". Use --replicas, --cpu, or --memory flags with `rise deploy` to override inherited values"
    } else {
        ""
    };

    if total_replicas < eff_min_replicas || total_replicas > eff_max_replicas {
        let summary = if per_container_resources.len() > 1 {
            format!(
                "Requested {} replicas total across {} containers but allowed range is [{}, {}]",
                total_replicas,
                per_container_resources.len(),
                eff_min_replicas,
                eff_max_replicas
            )
        } else {
            format!(
                "Requested {} replicas but allowed range is [{}, {}]",
                total_replicas, eff_min_replicas, eff_max_replicas
            )
        };
        return Err(ServerError::bad_request(format!(
            "{}{}",
            summary, redeploy_hint
        )));
    }

    for (cpu, memory) in per_container_resources {
        super::quantity::validate_cpu_range(cpu, eff_min_cpu, eff_max_cpu).map_err(|e| {
            ServerError::bad_request(format!("CPU constraint violation: {}{}", e, redeploy_hint))
        })?;

        super::quantity::validate_memory_range(memory, eff_min_memory, eff_max_memory).map_err(
            |e| {
                ServerError::bad_request(format!(
                    "Memory constraint violation: {}{}",
                    e, redeploy_hint
                ))
            },
        )?;
    }

    Ok(())
}

/// POST /deployments - Create a new deployment
pub async fn create_deployment(
    State(state): State<AppState>,
    auth: AuthContext,
    Json(payload): Json<CreateDeploymentRequest>,
) -> Result<Json<CreateDeploymentResponse>, ServerError> {
    info!("Creating deployment for project '{}'", payload.project);

    // Validate deployment group name if explicitly provided
    if let Some(ref group) = payload.group {
        if !is_valid_group_name(group) {
            return Err(ServerError::bad_request(format!(
                "Invalid group name '{}'. Must be 'default' or match pattern [a-z0-9][a-z0-9/-]*[a-z0-9] (no consecutive hyphens, normalized length max 63 chars)",
                group
            )));
        }
    }

    // Validate http_port if provided (should be 1-65535)
    if let Some(port) = payload.http_port {
        if port == 0 {
            return Err(ServerError::bad_request(
                "HTTP port must be between 1 and 65535",
            ));
        }
    }

    // Validate and normalize URL fields if provided
    let job_url = match payload.job_url {
        Some(ref url) => Some(
            crate::server::project::handlers::validate_http_url(url)
                .map_err(|e| ServerError::bad_request(format!("job_url: {e}")))?,
        ),
        None => None,
    };
    let pull_request_url = match payload.pull_request_url {
        Some(ref url) => Some(
            crate::server::project::handlers::validate_http_url(url)
                .map_err(|e| ServerError::bad_request(format!("pull_request_url: {e}")))?,
        ),
        None => None,
    };
    let git_repository_url = match payload.git_repository_url {
        Some(ref url) => Some(
            crate::server::project::handlers::validate_http_url(url)
                .map_err(|e| ServerError::bad_request(format!("git_repository_url: {e}")))?,
        ),
        None => None,
    };

    validate_env_overrides(&payload.env_overrides)?;

    // Wire-level validation for multi-container deployments (fails fast before
    // image resolution / registry calls / DB writes). Single-container requests
    // (no `containers`) pass through unchanged.
    models::validate_containers_and_routes(payload.containers.as_deref(), &payload.routes)?;

    // Per-container env overrides go through the same key/`PORT`/protected
    // validation as top-level overrides (`validate_containers_and_routes` only
    // enforces the is_secret restriction). An invalid key would otherwise reach
    // the pod spec and wedge the reconcile.
    if let Some(containers) = payload.containers.as_deref() {
        for spec in containers {
            for over in &spec.env_overrides {
                validate_env_override(over)?;
            }
        }
    }

    // Parse expiration duration if provided
    let expires_at = if let Some(ref expires_in) = payload.expires_in {
        Some(parse_expiration(expires_in).map_err(|e| {
            ServerError::bad_request(format!(
                "Invalid expiration duration '{}': {}",
                expires_in, e
            ))
        })?)
    } else {
        None
    };

    // Query project by name
    let project = projects::find_by_name(&state.db_pool, &payload.project)
        .await
        .internal_err("Failed to query project")?
        .ok_or_else(|| {
            ServerError::not_found(format!("Project '{}' not found", payload.project))
        })?;

    // Prevent deployments on projects in deletion lifecycle
    // Projects in Deleting or Terminated status should not accept new deployments
    if matches!(
        project.status,
        crate::db::models::ProjectStatus::Deleting | crate::db::models::ProjectStatus::Terminated
    ) {
        return Err(ServerError::conflict(format!(
            "Cannot create deployment for project in {:?} state",
            project.status
        )));
    }

    // Resolve deployment target (group + environment) from request parameters
    let (resolved_group, resolved_environment) = resolve_deployment_target(
        &state.db_pool,
        project.id,
        payload.environment.as_deref(),
        payload.group.as_deref(),
    )
    .await?;

    // Resolve auth for project scope (validates SA claims if external token)
    // Only mask auth failures (401/403) as 404 to prevent project existence leakage;
    // preserve 409 (SA collision) and 5xx (misconfiguration) for diagnosability.
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

    // Check deployment permissions (SA access already validated above)
    if !is_sa {
        crate::server::project::handlers::ensure_project_access_or_admin(&state, &user, &project)
            .await
            .map_err(|_| {
                ServerError::not_found(format!("Project '{}' not found", payload.project))
            })?;
    }

    // Enforce service account environment restrictions
    if is_sa {
        let sa = service_accounts::find_active_by_user_id(&state.db_pool, user.id)
            .await
            .internal_err("Failed to look up service account")?;
        if let Some(ref allowed_env_ids) = sa.and_then(|sa| sa.allowed_environment_ids) {
            let target_env_id = resolved_environment.as_ref().map(|e| e.id);
            match target_env_id {
                Some(env_id) if !allowed_env_ids.contains(&env_id) => {
                    return Err(ServerError::forbidden(
                        "This service account is not allowed to deploy to the requested environment",
                    ));
                }
                None => {
                    // SA has environment restrictions but no environment was specified;
                    // block the deployment since we can't verify the target is allowed.
                    return Err(ServerError::forbidden(
                        "This service account requires an explicit environment target",
                    ));
                }
                _ => {} // target environment is in the allowed list
            }
        }
    }

    // Generate deployment ID
    let deployment_id = generate_deployment_id();
    debug!("Generated deployment ID: {}", deployment_id);

    // Reject deployments whose generated K8s resource names would exceed K8s
    // limits. The deployment group and container name share the 63-char
    // Service-name budget, and the group is only known now — so this can't be
    // validated at rise.toml parse time. Single-container uses the implicit `app`.
    #[cfg(feature = "backend")]
    {
        let container_names: Vec<&str> = match payload.containers.as_deref() {
            Some(specs) => specs.iter().map(|s| s.name.as_str()).collect(),
            None => vec![crate::rise_toml::DEFAULT_CONTAINER_NAME],
        };
        validate_container_resource_names(
            &payload.project,
            &resolved_group,
            &deployment_id,
            &container_names,
        )?;
    }

    // Resolve effective http_port:
    // 1. Explicit http_port from request (if provided)
    // 2. Source deployment's http_port (if --from is used, handled below)
    // 3. PORT env var from project (if set and valid)
    // 4. Default: 8080
    let effective_http_port =
        resolve_effective_http_port(&state, project.id, payload.http_port).await?;
    info!(
        "Using http_port {} for deployment {}",
        effective_http_port, deployment_id
    );

    // Resolve multi-container images before minting credentials so the mint
    // covers every tag the CLI is going to push, and so pre-built container
    // images are pinned to an immutable digest. Returns None for legacy
    // single-container requests.
    let multi_container_resolved = match payload.containers.as_ref() {
        Some(specs) => Some(
            resolve_multi_container_images(&state, &payload.project, &deployment_id, specs).await?,
        ),
        None => None,
    };

    // Resolve effective deployment resources (replicas, cpu, memory)
    // Priority: request payload > platform defaults
    // Validation against constraints happens after rollback inheritance (below)
    let (mut effective_replicas, mut effective_cpu, mut effective_memory) = {
        #[cfg(feature = "backend")]
        {
            let defaults = state
                .deployment_defaults
                .as_ref()
                .cloned()
                .unwrap_or_default();

            // Resolve: request > platform defaults
            let replicas = payload.replicas.unwrap_or(defaults.replicas);
            let cpu = payload.cpu.clone().unwrap_or_else(|| defaults.cpu.clone());
            let memory = payload
                .memory
                .clone()
                .unwrap_or_else(|| defaults.memory.clone());

            (replicas, cpu, memory)
        }

        #[cfg(not(feature = "backend"))]
        {
            let replicas = payload.replicas.unwrap_or(1);
            let cpu = payload.cpu.clone().unwrap_or_else(|| "500m".to_string());
            let memory = payload
                .memory
                .clone()
                .unwrap_or_else(|| "256Mi".to_string());
            (replicas, cpu, memory)
        }
    };

    // Resolve effective workload-identity audiences from the request payload.
    // On redeploy without an explicit value, inherited from the source deployment below.
    let mut effective_identity_audiences: serde_json::Value =
        if let Some(ref audiences) = payload.identity_audiences {
            validate_identity_audiences(audiences)?;
            serde_json::to_value(audiences)
                .expect("BTreeMap<String, String> serialization is infallible")
        } else {
            serde_json::json!({})
        };

    // Handle deployment creation from an existing deployment (redeploy/rollback)
    if let Some(ref from_deployment_id) = payload.from_deployment {
        info!(
            "Creating deployment from existing deployment '{}'",
            from_deployment_id
        );

        // Find the source deployment
        let source_deployment = db_deployments::find_by_project_and_deployment_id(
            &state.db_pool,
            project.id,
            from_deployment_id,
        )
        .await
        .internal_err("Failed to find source deployment")?
        .ok_or_else(|| {
            ServerError::not_found(format!(
                "Source deployment '{}' not found for project '{}'",
                from_deployment_id, payload.project
            ))
        })?;

        // Verify the source deployment already has a reusable image.
        if !state_machine::can_create_from(&source_deployment) {
            return Err(ServerError::bad_request(format!(
                "Cannot create deployment from '{}' because its image is not available yet (status '{}').",
                from_deployment_id, source_deployment.status
            )));
        }

        // For chained redeployments, follow the chain to find the original source
        // This ensures we use the correct image tag from the original deployment that built the image
        let original_source_id =
            if let Some(chained_source) = source_deployment.rolled_back_from_deployment_id {
                // Source is itself a redeploy - use its source instead
                debug!(
                    "Source deployment {} is a redeploy, following chain to original source {}",
                    from_deployment_id, chained_source
                );
                chained_source
            } else {
                // Source is the original - use it directly
                source_deployment.id
            };

        // Determine http_port for the new deployment:
        // - If explicit http_port was provided in request, use it (already in effective_http_port)
        // - If no explicit port, inherit from source deployment
        let final_http_port = if payload.http_port.is_some() {
            effective_http_port
        } else {
            source_deployment.http_port as u16
        };

        // Inherit resources from source deployment unless explicitly overridden
        if payload.replicas.is_none() {
            effective_replicas = source_deployment.replicas as u32;
        }
        if payload.cpu.is_none() {
            effective_cpu = source_deployment.cpu.clone();
        }
        if payload.memory.is_none() {
            effective_memory = source_deployment.memory.clone();
        }
        if payload.identity_audiences.is_none() {
            effective_identity_audiences = source_deployment.identity_audiences.clone();
        }

        // Validate resources against constraints (after rollback inheritance).
        //
        // When the redeploy request carries no `[containers]` block we must
        // still re-validate against the SOURCE's persisted per-container specs:
        // those replicas/cpu/memory can sum higher than the deployment-level
        // `effective_replicas`, so a cap that tightened since the source was
        // created would otherwise be silently bypassed on a bare redeploy.
        #[cfg(feature = "backend")]
        let inherited_source_specs: Option<
            Vec<crate::server::deployment::models::ContainerSpec>,
        > = if payload.containers.is_none() {
            match source_deployment.containers.as_ref() {
                Some(v) => Some(models::decode_side_data(v).map_err(|e| {
                    ServerError::internal(format!(
                        "Source deployment {} ({}) has a non-NULL `containers` column that could \
                         not be decoded into Vec<ContainerSpec>: {:?}",
                        source_deployment.id, source_deployment.deployment_id, e
                    ))
                })?),
                None => None,
            }
        } else {
            None
        };

        #[cfg(feature = "backend")]
        {
            // Prefer the request's containers; otherwise the source's inherited
            // specs (so the across-container sum is re-checked); else single.
            let check_specs = payload
                .containers
                .as_deref()
                .or(inherited_source_specs.as_deref());
            let (total_replicas, per_container) = resolve_resource_check_inputs(
                check_specs,
                effective_replicas,
                &effective_cpu,
                &effective_memory,
            );
            validate_resource_constraints(
                &state,
                &resolved_environment,
                total_replicas,
                &per_container,
                payload.from_deployment.is_some(),
            )?;
        }

        // Compute container side-data BEFORE the INSERT so it lands atomically
        // with the deployments row. If the redeploy request carries its own
        // `containers` block, use that; otherwise inherit the source's JSON
        // verbatim so a redeploy of a multi-container source stays
        // multi-container (the legacy fallback would drop every per-container
        // K8s resource).
        let (containers_json, routes_json): (Option<serde_json::Value>, Option<serde_json::Value>) =
            if let Some(ref r) = multi_container_resolved {
                let (c, ro) = build_container_side_data_json(
                    &r.resolved_specs,
                    &payload.routes,
                    effective_replicas,
                    &effective_cpu,
                    &effective_memory,
                )?;
                (Some(c), ro)
            } else {
                inherit_container_side_data_from_source(&source_deployment)
            };

        // Create new deployment with Pushed status and invoke extension hooks
        // Copy image and image_digest from source - the helper function will determine the tag
        // For pre-built images: image_digest is copied, helper returns it
        // For build-from-source: rolled_back_from_deployment_id is used to find the original source deployment's image
        let new_deployment = create_deployment_with_hooks(
            &state,
            db_deployments::CreateDeploymentParams {
                deployment_id: &deployment_id,
                project_id: project.id,
                created_by_id: user.id,
                status: DbDeploymentStatus::Pushed, // Start in Pushed state so controller picks it up
                image: source_deployment.image.as_deref(), // Copy image from source if present
                image_digest: source_deployment.image_digest.as_deref(), // Copy digest from source if present
                rolled_back_from_deployment_id: Some(original_source_id), // Track original source for image tag calculation
                deployment_group: &resolved_group, // Use requested group (may be different from source)
                environment_id: resolved_environment.as_ref().map(|e| e.id),
                expires_at,                        // expires_at
                http_port: final_http_port as i32, // Use determined http_port
                is_active: false,                  // Deployments start as inactive
                job_url: job_url.as_deref(),
                pull_request_url: pull_request_url.as_deref(),
                git_repository_url: git_repository_url.as_deref(),
                replicas: effective_replicas as i32,
                cpu: &effective_cpu,
                memory: &effective_memory,
                identity_audiences: effective_identity_audiences.clone(),
                containers: containers_json.as_ref(),
                routes: routes_json.as_ref(),
            },
            &project,
        )
        .await?;

        // Handle environment variables based on use_source_env_vars flag
        if payload.use_source_env_vars {
            // Copy environment variables from source deployment
            info!(
                "Copying environment variables from source deployment '{}'",
                from_deployment_id
            );
            crate::db::env_vars::copy_deployment_env_vars_to_deployment(
                &state.db_pool,
                source_deployment.id,
                new_deployment.id,
            )
            .await
            .internal_err("Failed to copy environment variables")?;
        } else {
            // Copy current project environment variables to deployment
            info!("Using current project environment variables");
            crate::db::env_vars::copy_project_env_vars_to_deployment(
                &state.db_pool,
                project.id,
                new_deployment.id,
                new_deployment.environment_id,
                resolved_environment.as_ref().map(|e| e.name.as_str()),
            )
            .await
            .internal_err("Failed to copy environment variables")?;
        }

        // Apply env overrides from the request
        apply_env_overrides(
            &state,
            new_deployment.id,
            &payload.env_overrides,
            resolved_environment.as_ref().map(|e| e.name.as_str()),
        )
        .await?;

        // Upsert PORT env var with the final http_port value
        crate::db::env_vars::upsert_deployment_env_var(
            &state.db_pool,
            new_deployment.id,
            "PORT",
            &final_http_port.to_string(),
            false, // not a secret
            false, // is_protected
            "system",
        )
        .await
        .internal_err("Failed to insert PORT env var")?;

        // Insert Rise-provided environment variables
        insert_rise_env_vars(&state, &new_deployment, &project).await?;

        // Use helper to determine image tag (for logging/response only)
        let image_tag = crate::server::deployment::utils::get_deployment_image_tag(
            &state,
            &new_deployment,
            &project,
        )
        .await;

        info!(
            "Created deployment {} from {} (image: {}, env vars: {})",
            deployment_id,
            from_deployment_id,
            image_tag,
            if payload.use_source_env_vars {
                "from source"
            } else {
                "from project"
            }
        );

        // Trigger Metacontroller resync so the webhook picks up the new Pushed deployment
        if let Some(ref kube_client) = state.kube_client {
            if let Err(e) =
                crate::server::deployment::crd::trigger_resync(kube_client, &project.name).await
            {
                tracing::warn!(
                    project = %project.name,
                    "Failed to trigger CRD resync after rollback deployment: {:?}", e
                );
            }
        }

        // Return response with image tag and empty credentials (no push needed)
        return Ok(Json(CreateDeploymentResponse {
            deployment_id,
            image_tag,
            credentials: crate::server::registry::models::RegistryCredentials {
                registry_url: String::new(),
                username: String::new(),
                password: String::new(),
                expires_in: None,
                auth_method: Default::default(),
            },
            container_images: multi_container_resolved
                .as_ref()
                .map(|r| r.container_images.clone()),
        }));
    }

    // Validate resources against constraints (normal deployment path)
    #[cfg(feature = "backend")]
    {
        let (total_replicas, per_container) = resolve_resource_check_inputs(
            payload.containers.as_deref(),
            effective_replicas,
            &effective_cpu,
            &effective_memory,
        );
        validate_resource_constraints(
            &state,
            &resolved_environment,
            total_replicas,
            &per_container,
            false,
        )?;
    }

    // Compute container side-data once (non-rollback paths) so it can be
    // inserted atomically with the deployments row. `None` for legacy
    // single-container requests.
    let (containers_json, routes_json): (Option<serde_json::Value>, Option<serde_json::Value>) =
        if let Some(ref r) = multi_container_resolved {
            let (c, ro) = build_container_side_data_json(
                &r.resolved_specs,
                &payload.routes,
                effective_replicas,
                &effective_cpu,
                &effective_memory,
            )?;
            (Some(c), ro)
        } else {
            (None, None)
        };

    // Branch based on whether user provided a pre-built image
    if let Some(ref user_image) = payload.image {
        if payload.push_image {
            // Path 1a: Push-image deployment — CLI will pull and push to Rise registry
            // Skip digest resolution: the image will be pushed to the internal registry by tag,
            // and the controller uses get_image_tag() (the no-digest path) to pull from there.
            info!("Creating push-image deployment with image: {}", user_image);

            let image_tag = state.registry_provider.get_image_tag(
                &payload.project,
                &deployment_id,
                ImageTagType::ClientFacing,
            );
            // push-image is single-container only; the multi-container CLI
            // always sends `image: None` at the top level and uses the
            // build-from-source branch below.
            let credentials = state
                .registry_provider
                .get_credentials(&payload.project, &[deployment_id.as_str()])
                .await
                .internal_err("Failed to get credentials")?;

            let deployment = create_deployment_with_hooks(
                &state,
                db_deployments::CreateDeploymentParams {
                    deployment_id: &deployment_id,
                    project_id: project.id,
                    created_by_id: user.id,
                    status: DbDeploymentStatus::Pending,
                    image: Some(user_image), // Store original user input for display
                    image_digest: None, // No digest — controller will use internal registry tag
                    rolled_back_from_deployment_id: None,
                    deployment_group: &resolved_group,
                    environment_id: resolved_environment.as_ref().map(|e| e.id),
                    expires_at,
                    http_port: effective_http_port as i32,
                    is_active: false,
                    job_url: job_url.as_deref(),
                    pull_request_url: pull_request_url.as_deref(),
                    git_repository_url: git_repository_url.as_deref(),
                    replicas: effective_replicas as i32,
                    cpu: &effective_cpu,
                    memory: &effective_memory,
                    identity_audiences: effective_identity_audiences.clone(),
                    containers: containers_json.as_ref(),
                    routes: routes_json.as_ref(),
                },
                &project,
            )
            .await?;

            info!(
                "Created push-image deployment {} for project {}",
                deployment_id, payload.project
            );

            // Copy project environment variables to deployment
            crate::db::env_vars::copy_project_env_vars_to_deployment(
                &state.db_pool,
                project.id,
                deployment.id,
                deployment.environment_id,
                resolved_environment.as_ref().map(|e| e.name.as_str()),
            )
            .await
            .internal_err("Failed to copy environment variables")?;

            // Apply env overrides from the request
            apply_env_overrides(
                &state,
                deployment.id,
                &payload.env_overrides,
                resolved_environment.as_ref().map(|e| e.name.as_str()),
            )
            .await?;

            crate::db::env_vars::upsert_deployment_env_var(
                &state.db_pool,
                deployment.id,
                "PORT",
                &effective_http_port.to_string(),
                false,
                false,
                "system",
            )
            .await
            .internal_err("Failed to insert PORT env var")?;

            insert_rise_env_vars(&state, &deployment, &project).await?;

            return Ok(Json(CreateDeploymentResponse {
                deployment_id,
                image_tag,
                credentials,
                container_images: multi_container_resolved
                    .as_ref()
                    .map(|r| r.container_images.clone()),
            }));
        }

        // Path 1b: Direct pre-built image deployment (no push)
        info!("Creating deployment with pre-built image: {}", user_image);

        // Resolve image to digest (the OCI client handles Docker Hub shorthand
        // normalization like nginx → docker.io/library/nginx internally)
        info!("Resolving image '{}' to digest...", user_image);
        let image_digest = resolve_image_digest(
            &state.oci_client,
            &state.registry_provider,
            user_image,
            &project.name,
        )
        .await
        .map_err(|e| {
            ServerError::bad_request(format!("Failed to resolve image '{}': {}", user_image, e))
        })?;

        info!("Successfully resolved image to digest: {}", image_digest);

        // Create deployment record with image fields set and invoke extension hooks
        let deployment = create_deployment_with_hooks(
            &state,
            db_deployments::CreateDeploymentParams {
                deployment_id: &deployment_id,
                project_id: project.id,
                created_by_id: user.id,
                status: DbDeploymentStatus::Pushed,
                image: Some(user_image),
                image_digest: Some(&image_digest),
                rolled_back_from_deployment_id: None,
                deployment_group: &resolved_group,
                environment_id: resolved_environment.as_ref().map(|e| e.id),
                expires_at,
                http_port: effective_http_port as i32,
                is_active: false,
                job_url: job_url.as_deref(),
                pull_request_url: pull_request_url.as_deref(),
                git_repository_url: git_repository_url.as_deref(),
                replicas: effective_replicas as i32,
                cpu: &effective_cpu,
                memory: &effective_memory,
                identity_audiences: effective_identity_audiences.clone(),
                containers: containers_json.as_ref(),
                routes: routes_json.as_ref(),
            },
            &project,
        )
        .await?;

        info!(
            "Created pre-built image deployment {} for project {}",
            deployment_id, payload.project
        );

        // Copy project environment variables to deployment
        crate::db::env_vars::copy_project_env_vars_to_deployment(
            &state.db_pool,
            project.id,
            deployment.id,
            deployment.environment_id,
            resolved_environment.as_ref().map(|e| e.name.as_str()),
        )
        .await
        .internal_err("Failed to copy environment variables")?;

        // Apply env overrides from the request
        apply_env_overrides(
            &state,
            deployment.id,
            &payload.env_overrides,
            resolved_environment.as_ref().map(|e| e.name.as_str()),
        )
        .await?;

        // Upsert PORT env var with the resolved effective value
        // This overwrites any user-set PORT with the resolved value (which may be the same)
        crate::db::env_vars::upsert_deployment_env_var(
            &state.db_pool,
            deployment.id,
            "PORT",
            &effective_http_port.to_string(),
            false, // not a secret
            false, // is_protected
            "system",
        )
        .await
        .internal_err("Failed to insert PORT env var")?;

        // Insert Rise-provided environment variables
        insert_rise_env_vars(&state, &deployment, &project).await?;

        // Trigger Metacontroller resync so the webhook picks up the new Pushed deployment
        if let Some(ref kube_client) = state.kube_client {
            if let Err(e) =
                crate::server::deployment::crd::trigger_resync(kube_client, &project.name).await
            {
                tracing::warn!(
                    project = %project.name,
                    "Failed to trigger CRD resync after pre-built image deployment: {:?}", e
                );
            }
        }

        // Return response with digest as image_tag and empty credentials
        Ok(Json(CreateDeploymentResponse {
            deployment_id,
            image_tag: image_digest,
            credentials: crate::server::registry::models::RegistryCredentials {
                registry_url: String::new(),
                username: String::new(),
                password: String::new(),
                expires_in: None,
                auth_method: Default::default(),
            },
            container_images: multi_container_resolved
                .as_ref()
                .map(|r| r.container_images.clone()),
        }))
    } else {
        // Path 2: Build from source (current behavior)
        // Get full image tag from provider for CLI client (uses client_registry_url if configured)
        let image_tag = state.registry_provider.get_image_tag(
            &payload.project,
            &deployment_id,
            ImageTagType::ClientFacing,
        );

        // Get registry credentials scoped to every tag that's going to be
        // pushed. For single-container that's just the deployment ID; for
        // multi-container it's the per-container tag list returned by
        // `resolve_multi_container_images`, so a single mint covers every
        // `docker push` the CLI will perform (critical for JFrog, which
        // scopes by tag).
        let credentials = {
            let owned_tags: Vec<String> = multi_container_resolved
                .as_ref()
                .filter(|r| !r.push_tags.is_empty())
                .map(|r| r.push_tags.clone())
                .unwrap_or_else(|| vec![deployment_id.clone()]);
            let tag_refs: Vec<&str> = owned_tags.iter().map(String::as_str).collect();
            state
                .registry_provider
                .get_credentials(&payload.project, &tag_refs)
                .await
                .internal_err("Failed to get credentials")?
        };

        debug!("Image tag: {}", image_tag);

        // Create deployment record in database and invoke extension hooks
        let deployment = create_deployment_with_hooks(
            &state,
            db_deployments::CreateDeploymentParams {
                deployment_id: &deployment_id,
                project_id: project.id,
                created_by_id: user.id,
                status: DbDeploymentStatus::Pending,
                image: None,        // image - NULL for build-from-source
                image_digest: None, // image_digest - NULL for build-from-source
                rolled_back_from_deployment_id: None, // Not a rollback
                deployment_group: &resolved_group, // deployment_group
                environment_id: resolved_environment.as_ref().map(|e| e.id),
                expires_at,                            // expires_at
                http_port: effective_http_port as i32, // http_port
                is_active: false,                      // Deployments start as inactive
                job_url: job_url.as_deref(),
                pull_request_url: pull_request_url.as_deref(),
                git_repository_url: git_repository_url.as_deref(),
                replicas: effective_replicas as i32,
                cpu: &effective_cpu,
                memory: &effective_memory,
                identity_audiences: effective_identity_audiences.clone(),
                containers: containers_json.as_ref(),
                routes: routes_json.as_ref(),
            },
            &project,
        )
        .await?;

        info!(
            "Created build-from-source deployment {} for project {}",
            deployment_id, payload.project
        );

        // Copy project environment variables to deployment
        crate::db::env_vars::copy_project_env_vars_to_deployment(
            &state.db_pool,
            project.id,
            deployment.id,
            deployment.environment_id,
            resolved_environment.as_ref().map(|e| e.name.as_str()),
        )
        .await
        .internal_err("Failed to copy environment variables")?;

        // Apply env overrides from the request
        apply_env_overrides(
            &state,
            deployment.id,
            &payload.env_overrides,
            resolved_environment.as_ref().map(|e| e.name.as_str()),
        )
        .await?;

        // Upsert PORT env var with the resolved effective value
        // This overwrites any user-set PORT with the resolved value (which may be the same)
        crate::db::env_vars::upsert_deployment_env_var(
            &state.db_pool,
            deployment.id,
            "PORT",
            &effective_http_port.to_string(),
            false, // not a secret
            false, // is_protected
            "system",
        )
        .await
        .internal_err("Failed to insert PORT env var")?;

        // Insert Rise-provided environment variables
        insert_rise_env_vars(&state, &deployment, &project).await?;

        // Return response
        Ok(Json(CreateDeploymentResponse {
            deployment_id,
            image_tag,
            credentials,
            container_images: multi_container_resolved
                .as_ref()
                .map(|r| r.container_images.clone()),
        }))
    }
}

/// Shared logic for updating a deployment's status.
///
/// Given a resolved deployment and project, validates permissions and applies the status update.
/// When `is_sa` is true, permission checks are skipped (already validated by `resolve_for_project`).
async fn perform_status_update(
    state: &AppState,
    user: &crate::db::models::User,
    is_sa: bool,
    deployment: crate::db::models::Deployment,
    project: &crate::db::models::Project,
    deployment_id: &str,
    payload: UpdateDeploymentStatusRequest,
) -> Result<Json<Deployment>, ServerError> {
    // Check if user has permission (owns the project)
    // SA access was already validated by resolve_for_project
    if !is_sa {
        crate::server::project::handlers::ensure_project_access_or_admin(state, user, project)
            .await
            .map_err(|_| {
                ServerError::not_found(format!("Deployment '{}' not found", deployment_id))
            })?;
    }

    // Update status in database
    let status_copy = payload.status.clone();
    let updated_deployment = match payload.status {
        DeploymentStatus::Failed => {
            let error_msg = payload.error_message.as_deref().unwrap_or("Unknown error");
            let deployment = db_deployments::mark_failed(&state.db_pool, deployment.id, error_msg)
                .await
                .internal_err("Failed to update deployment")?;

            // Update project status to Failed
            projects::update_calculated_status(&state.db_pool, project.id)
                .await
                .internal_err("Failed to update project status")?;

            deployment
        }
        _ => {
            // update_status will validate the state transition
            let db_status = convert_status_to_db(payload.status);
            let deployment =
                db_deployments::update_status(&state.db_pool, deployment.id, db_status)
                    .await
                    .map_err(|e| {
                        // State transition validation errors are returned as anyhow errors
                        // Return BAD_REQUEST for validation errors, INTERNAL_SERVER_ERROR otherwise
                        let error_msg = e.to_string();
                        if error_msg.contains("Invalid deployment state transition") {
                            ServerError::bad_request(error_msg)
                        } else {
                            ServerError::internal_anyhow(e, "Failed to update deployment")
                        }
                    })?;

            // Update project status (e.g., to Deploying)
            projects::update_calculated_status(&state.db_pool, project.id)
                .await
                .internal_err("Failed to update project status")?;

            deployment
        }
    };

    info!(
        "Updated deployment {} to status {:?}",
        deployment_id, status_copy
    );

    // Trigger Metacontroller resync so the webhook picks up the status change
    if let Some(ref kube_client) = state.kube_client {
        if let Err(e) =
            crate::server::deployment::crd::trigger_resync(kube_client, &project.name).await
        {
            tracing::warn!(
                project = %project.name,
                "Failed to trigger CRD resync: {:?}", e
            );
        }
    }

    // Only calculate URLs for non-terminal deployments that could receive traffic
    let (primary_url, custom_domain_urls, all_urls) =
        if state_machine::is_terminal(&updated_deployment.status) {
            (None, vec![], vec![])
        } else {
            match state
                .deployment_backend
                .get_deployment_urls(&updated_deployment, project)
                .await
            {
                Ok(urls) => (
                    Some(urls.primary_url),
                    urls.custom_domain_urls,
                    urls.all_urls,
                ),
                Err(e) => {
                    error!(
                        "Failed to calculate URLs for deployment {}: {}",
                        deployment_id, e
                    );
                    (None, vec![], vec![])
                }
            }
        };

    let created_by_email =
        get_creator_email(&state.db_pool, updated_deployment.created_by_id).await;
    Ok(Json(
        convert_deployment(
            state,
            updated_deployment,
            project,
            created_by_email,
            primary_url,
            custom_domain_urls,
            all_urls,
        )
        .await,
    ))
}

/// PATCH /projects/{project_name}/deployments/{deployment_id}/status - Update deployment status (project-scoped)
pub async fn update_deployment_status_by_project(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((project_name, deployment_id)): Path<(String, String)>,
    Json(payload): Json<UpdateDeploymentStatusRequest>,
) -> Result<Json<Deployment>, ServerError> {
    info!(
        "Updating deployment {} status to {:?} for project {}",
        deployment_id, payload.status, project_name
    );

    // Find project by name
    let project = projects::find_by_name(&state.db_pool, &project_name)
        .await
        .internal_err("Failed to find project")?
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

    // Find deployment by deployment_id + project_id
    let deployment =
        db_deployments::find_by_deployment_id(&state.db_pool, &deployment_id, project.id)
            .await
            .internal_err("Failed to find deployment")?
            .ok_or_else(|| {
                ServerError::not_found(format!(
                    "Deployment '{}' not found for project '{}'",
                    deployment_id, project_name
                ))
            })?;

    perform_status_update(
        &state,
        &user,
        is_sa,
        deployment,
        &project,
        &deployment_id,
        payload,
    )
    .await
}

/// PATCH /deployments/{deployment_id}/status - Update deployment status (deprecated, unscoped)
///
/// This endpoint is deprecated. Use `PATCH /projects/{project_name}/deployments/{deployment_id}/status` instead.
/// Kept for backward compatibility with older CLI versions.
///
/// Returns 409 Conflict when multiple projects have deployments with the same deployment_id,
/// since we cannot determine which one the caller intended.
pub async fn update_deployment_status(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(deployment_id): Path<String>,
    Json(payload): Json<UpdateDeploymentStatusRequest>,
) -> Result<Json<Deployment>, ServerError> {
    warn!(
        "Deprecated endpoint called: PATCH /deployments/{}/status — use PATCH /projects/{{project_name}}/deployments/{}/status instead",
        deployment_id, deployment_id
    );

    // Single query to find matching deployments across all projects (LIMIT 2 to detect collisions)
    let matching_deployments =
        db_deployments::find_by_deployment_id_unscoped(&state.db_pool, &deployment_id, 2)
            .await
            .internal_err(format!("Failed to query deployment '{}'", deployment_id))?;

    if matching_deployments.len() > 1 {
        let project_ids: Vec<String> = matching_deployments
            .iter()
            .map(|d| d.project_id.to_string())
            .collect();
        warn!(
            "Deployment ID collision on deprecated endpoint: deployment_id={} matches {} projects: {:?}. \
             Refusing to proceed. Clients should migrate to the project-scoped endpoint.",
            deployment_id,
            matching_deployments.len(),
            project_ids,
        );
        return Err(ServerError::conflict(format!(
            "Ambiguous deployment_id '{}': matches multiple projects. \
             Use PATCH /api/v1/projects/{{project_name}}/deployments/{}/status instead.",
            deployment_id, deployment_id
        )));
    }

    let deployment = matching_deployments.into_iter().next().ok_or_else(|| {
        ServerError::not_found(format!("Deployment '{}' not found", deployment_id))
    })?;

    let project = projects::find_by_id(&state.db_pool, deployment.project_id)
        .await
        .internal_err("Failed to find project")?
        .ok_or_else(|| {
            ServerError::not_found(format!(
                "Project for deployment '{}' not found",
                deployment_id
            ))
        })?;

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

    perform_status_update(
        &state,
        &user,
        is_sa,
        deployment,
        &project,
        &deployment_id,
        payload,
    )
    .await
}

/// Query parameters for listing deployments
#[derive(Debug, serde::Deserialize)]
pub struct ListDeploymentsQuery {
    #[serde(rename = "group")]
    pub deployment_group: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// List deployments for a project
pub async fn list_deployments(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(project_name): Path<String>,
    Query(query): Query<ListDeploymentsQuery>,
) -> Result<Json<Vec<Deployment>>, ServerError> {
    debug!(
        "Listing deployments for project: {} (group: {:?})",
        project_name, query.deployment_group
    );

    // Find the project by name
    let project = projects::find_by_name(&state.db_pool, &project_name)
        .await
        .internal_err("Failed to find project")?
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

    // Check if user has permission to view deployments (SA access already validated)
    if !is_sa {
        crate::server::project::handlers::ensure_project_access_or_admin(&state, &user, &project)
            .await
            .map_err(|_| ServerError::not_found(format!("Project '{}' not found", project_name)))?;
    }

    // Get deployments from database (optionally filtered by group, with pagination)
    let db_deployments = db_deployments::list_for_project_and_group(
        &state.db_pool,
        project.id,
        query.deployment_group.as_deref(),
        query.limit,
        query.offset,
    )
    .await
    .internal_err("Failed to list deployments")?;

    // Convert to API models (fetch creator emails and calculate URLs)
    let mut deployments = Vec::with_capacity(db_deployments.len());
    for db_deployment in db_deployments {
        let created_by_email = get_creator_email(&state.db_pool, db_deployment.created_by_id).await;

        // Only calculate URLs for non-terminal deployments that could receive traffic
        let (primary_url, custom_domain_urls, all_urls) =
            if state_machine::is_terminal(&db_deployment.status) {
                // Terminal deployments (Failed, Stopped, Cancelled, Superseded, Expired) cannot receive traffic
                (None, vec![], vec![])
            } else {
                // Calculate deployment URLs dynamically for active deployments
                match state
                    .deployment_backend
                    .get_deployment_urls(&db_deployment, &project)
                    .await
                {
                    Ok(urls) => (
                        Some(urls.primary_url),
                        urls.custom_domain_urls,
                        urls.all_urls,
                    ),
                    Err(e) => {
                        error!(
                            "Failed to calculate URLs for deployment {}: {}",
                            db_deployment.deployment_id, e
                        );
                        (None, vec![], vec![])
                    }
                }
            };

        deployments.push(
            convert_deployment(
                &state,
                db_deployment,
                &project,
                created_by_email,
                primary_url,
                custom_domain_urls,
                all_urls,
            )
            .await,
        );
    }

    Ok(Json(deployments))
}

/// Query parameters for stopping deployments
#[derive(Debug, serde::Deserialize)]
pub struct StopDeploymentsQuery {
    pub group: String,
}

/// Response for stopping deployments
#[derive(Debug, serde::Serialize)]
pub struct StopDeploymentsResponse {
    pub stopped_count: usize,
    pub deployment_ids: Vec<String>,
}

/// POST /projects/{project_name}/deployments/stop - Stop all deployments in a group
pub async fn stop_deployments_by_group(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(project_name): Path<String>,
    Query(query): Query<StopDeploymentsQuery>,
) -> Result<Json<StopDeploymentsResponse>, ServerError> {
    info!(
        "Stopping all deployments in group '{}' for project '{}'",
        query.group, project_name
    );

    // Validate group name
    if !is_valid_group_name(&query.group) {
        return Err(ServerError::bad_request(format!(
            "Invalid group name '{}'. Must be 'default' or match pattern [a-z0-9][a-z0-9/-]*[a-z0-9] (no consecutive hyphens, normalized length max 63 chars)",
            query.group
        )));
    }

    // Find the project by name
    let project = projects::find_by_name(&state.db_pool, &project_name)
        .await
        .internal_err("Failed to find project")?
        .ok_or_else(|| ServerError::not_found(format!("Project '{}' not found", project_name)))?;

    // Resolve auth for project scope
    let (_user, is_sa) = auth
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

    // Check if user has permission to stop deployments (SA access already validated)
    if !is_sa {
        crate::server::project::handlers::ensure_project_access_or_admin(&state, &_user, &project)
            .await
            .map_err(|_| ServerError::not_found(format!("Project '{}' not found", project_name)))?;
    }

    // Find all non-terminal deployments in this group
    let deployments = db_deployments::find_non_terminal_for_project_and_group(
        &state.db_pool,
        project.id,
        &query.group,
    )
    .await
    .internal_err("Failed to find deployments")?;

    let mut stopped_ids = Vec::new();

    // Mark each deployment using appropriate state transition
    for deployment in deployments {
        let result = if state_machine::is_cancellable(&deployment.status) {
            db_deployments::mark_cancelling(&state.db_pool, deployment.id)
                .await
                .map(|_| "Cancelling")
        } else {
            db_deployments::mark_terminating(
                &state.db_pool,
                deployment.id,
                crate::db::models::TerminationReason::UserStopped,
            )
            .await
            .map(|_| "Terminating")
        };
        match result {
            Ok(new_status) => {
                info!(
                    "Marked deployment {} as {}",
                    deployment.deployment_id, new_status
                );
                stopped_ids.push(deployment.deployment_id);
            }
            Err(e) => {
                error!(
                    "Failed to stop deployment {}: {}",
                    deployment.deployment_id, e
                );
            }
        }
    }

    // Update project status
    projects::update_calculated_status(&state.db_pool, project.id)
        .await
        .internal_err("Failed to update project status")?;

    // Trigger Metacontroller resync
    if !stopped_ids.is_empty() {
        if let Some(ref kube_client) = state.kube_client {
            if let Err(e) =
                crate::server::deployment::crd::trigger_resync(kube_client, &project.name).await
            {
                tracing::warn!(
                    project = %project.name,
                    "Failed to trigger CRD resync: {:?}", e
                );
            }
        }
    }

    info!(
        "Stopped {} deployments in group '{}' for project '{}'",
        stopped_ids.len(),
        query.group,
        project_name
    );

    Ok(Json(StopDeploymentsResponse {
        stopped_count: stopped_ids.len(),
        deployment_ids: stopped_ids,
    }))
}

/// POST /projects/{project_name}/deployments/{deployment_id}/stop - Stop a specific deployment
pub async fn stop_deployment(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((project_name, deployment_id)): Path<(String, String)>,
) -> Result<Json<Deployment>, ServerError> {
    info!(
        "Stopping deployment '{}' for project '{}'",
        deployment_id, project_name
    );

    // Find the project by name
    let project = projects::find_by_name(&state.db_pool, &project_name)
        .await
        .internal_err("Failed to find project")?
        .ok_or_else(|| ServerError::not_found(format!("Project '{}' not found", project_name)))?;

    // Resolve auth for project scope
    let (_user, is_sa) = auth
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

    // Check if user has permission to stop deployments (SA access already validated)
    if !is_sa {
        crate::server::project::handlers::ensure_project_access_or_admin(&state, &_user, &project)
            .await
            .map_err(|_| ServerError::not_found(format!("Project '{}' not found", project_name)))?;
    }

    // Find the specific deployment
    let deployment =
        db_deployments::find_by_deployment_id(&state.db_pool, &deployment_id, project.id)
            .await
            .internal_err("Failed to find deployment")?
            .ok_or_else(|| {
                ServerError::not_found(format!("Deployment '{}' not found", deployment_id))
            })?;

    // Check if deployment is already in a terminal state
    if state_machine::is_terminal(&deployment.status) {
        return Err(ServerError::bad_request(format!(
            "Deployment '{}' is already in terminal state: {}",
            deployment_id, deployment.status
        )));
    }

    // Use the appropriate state transition based on current status:
    // Pre-infrastructure states (Pending, Building, Pushing, Pushed, Deploying) → Cancelling
    // Infrastructure states (Healthy, Unhealthy) → Terminating
    let updated_deployment = if state_machine::is_cancellable(&deployment.status) {
        let d = db_deployments::mark_cancelling(&state.db_pool, deployment.id)
            .await
            .internal_err("Failed to cancel deployment")?;
        info!("Marked deployment {} as Cancelling", deployment_id);
        d
    } else {
        let d = db_deployments::mark_terminating(
            &state.db_pool,
            deployment.id,
            crate::db::models::TerminationReason::UserStopped,
        )
        .await
        .internal_err("Failed to stop deployment")?;
        info!("Marked deployment {} as Terminating", deployment_id);
        d
    };

    // Update project status
    projects::update_calculated_status(&state.db_pool, project.id)
        .await
        .internal_err("Failed to update project status")?;

    // Trigger Metacontroller resync
    if let Some(ref kube_client) = state.kube_client {
        if let Err(e) =
            crate::server::deployment::crd::trigger_resync(kube_client, &project.name).await
        {
            tracing::warn!(
                project = %project.name,
                "Failed to trigger CRD resync: {:?}", e
            );
        }
    }

    // Calculate deployment URLs dynamically
    let (primary_url, custom_domain_urls, all_urls) = match state
        .deployment_backend
        .get_deployment_urls(&updated_deployment, &project)
        .await
    {
        Ok(urls) => (
            Some(urls.primary_url),
            urls.custom_domain_urls,
            urls.all_urls,
        ),
        Err(e) => {
            error!(
                "Failed to calculate URLs for deployment {}: {}",
                deployment_id, e
            );
            (None, vec![], vec![])
        }
    };

    // Get creator email and convert to API model
    let created_by_email =
        get_creator_email(&state.db_pool, updated_deployment.created_by_id).await;
    Ok(Json(
        convert_deployment(
            &state,
            updated_deployment,
            &project,
            created_by_email,
            primary_url,
            custom_domain_urls,
            all_urls,
        )
        .await,
    ))
}

/// GET /projects/{project_name}/deployments/{deployment_id} - Get a specific deployment
pub async fn get_deployment_by_project(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((project_name, deployment_id)): Path<(String, String)>,
) -> Result<Json<Deployment>, ServerError> {
    debug!(
        "Getting deployment {} for project {}",
        deployment_id, project_name
    );

    // Find the project by name
    let project = projects::find_by_name(&state.db_pool, &project_name)
        .await
        .internal_err("Failed to find project")?
        .ok_or_else(|| ServerError::not_found(format!("Project '{}' not found", project_name)))?;

    // Resolve auth for project scope
    let (_user, is_sa) = auth
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

    // Check if user has permission to view deployments (SA access already validated)
    if !is_sa {
        crate::server::project::handlers::ensure_project_access_or_admin(&state, &_user, &project)
            .await
            .map_err(|_| ServerError::not_found(format!("Project '{}' not found", project_name)))?;
    }

    // Find deployment by project_id and deployment_id
    let deployment = db_deployments::find_by_project_and_deployment_id(
        &state.db_pool,
        project.id,
        &deployment_id,
    )
    .await
    .internal_err("Failed to find deployment")?
    .ok_or_else(|| {
        ServerError::not_found(format!(
            "Deployment '{}' not found for project '{}'",
            deployment_id, project_name
        ))
    })?;

    // Only calculate URLs for non-terminal deployments that could receive traffic
    let (primary_url, custom_domain_urls, all_urls) =
        if state_machine::is_terminal(&deployment.status) {
            // Terminal deployments (Failed, Stopped, Cancelled, Superseded, Expired) cannot receive traffic
            (None, vec![], vec![])
        } else {
            // Calculate deployment URLs dynamically for active deployments
            match state
                .deployment_backend
                .get_deployment_urls(&deployment, &project)
                .await
            {
                Ok(urls) => (
                    Some(urls.primary_url),
                    urls.custom_domain_urls,
                    urls.all_urls,
                ),
                Err(e) => {
                    error!(
                        "Failed to calculate URLs for deployment {}: {}",
                        deployment_id, e
                    );
                    (None, vec![], vec![])
                }
            }
        };

    let created_by_email = get_creator_email(&state.db_pool, deployment.created_by_id).await;
    Ok(Json(
        convert_deployment(
            &state,
            deployment,
            &project,
            created_by_email,
            primary_url,
            custom_domain_urls,
            all_urls,
        )
        .await,
    ))
}

/// GET /projects/{project_name}/deployment-groups - List all deployment groups for a project
pub async fn list_deployment_groups(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(project_name): Path<String>,
) -> Result<Json<Vec<String>>, ServerError> {
    debug!("Listing deployment groups for project: {}", project_name);

    // Find the project by name
    let project = projects::find_by_name(&state.db_pool, &project_name)
        .await
        .internal_err("Failed to find project")?
        .ok_or_else(|| ServerError::not_found(format!("Project '{}' not found", project_name)))?;

    // Resolve auth for project scope
    let (_user, is_sa) = auth
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

    // Check if user has permission to view deployment groups (SA access already validated)
    if !is_sa {
        crate::server::project::handlers::ensure_project_access_or_admin(&state, &_user, &project)
            .await
            .map_err(|_| ServerError::not_found(format!("Project '{}' not found", project_name)))?;
    }

    // Get deployment groups from database
    let groups = db_deployments::get_all_deployment_groups(&state.db_pool, project.id)
        .await
        .internal_err("Failed to list deployment groups")?;

    Ok(Json(groups))
}

/// Query parameters for log streaming
#[derive(serde::Deserialize)]
pub struct LogStreamParams {
    /// Follow the logs (stream continuously)
    #[serde(default)]
    pub follow: bool,
    /// Number of lines to show from the end
    pub tail: Option<i64>,
    /// Include timestamps in the output
    #[serde(default)]
    pub timestamps: bool,
    /// Show logs since this many seconds ago
    pub since: Option<i64>,
    /// Start of an explicit time range in RFC3339 format
    pub start: Option<String>,
    /// End of an explicit time range in RFC3339 format. Honored by the Loki
    /// backend. The Kubernetes backend has no `pods/log` end-time filter and
    /// silently ignores this — for past windows on the Kubernetes backend,
    /// use the Loki backend instead.
    pub end: Option<String>,
    /// Restrict to lines whose backend-emitted level matches one of these
    /// values. Repeated query param: `?level=info&level=warn`. Empty list
    /// (param absent) means no filter. Accepted values come from the
    /// backend's `levels` advertised in `/api/v1/logs/capabilities`.
    #[serde(default, rename = "level")]
    pub level: Vec<String>,
    /// Case-insensitive substring filter applied to each line.
    pub search: Option<String>,
    /// For backward pagination: skip this many of the most-recent qualifying
    /// lines before returning. Used by the Kubernetes backend (whose
    /// `pods/log` API has no end-time filter) so the frontend can scroll
    /// further back than the initial tail window. The Loki backend ignores
    /// this and uses its own timestamp-based pagination.
    pub skip_recent: Option<i64>,
}

/// Query parameters for log volume
#[derive(serde::Deserialize)]
pub struct LogVolumeParams {
    /// Start of an explicit time range in RFC3339 format
    pub start: String,
    /// End of an explicit time range in RFC3339 format
    pub end: String,
    /// Bucket step in seconds
    #[serde(default = "default_log_count_step_seconds")]
    pub step_seconds: i64,
    /// Restrict counts to lines whose backend-emitted level matches one of
    /// these values. Repeated query param: `?level=info&level=warn`.
    #[serde(default, rename = "level")]
    pub level: Vec<String>,
    /// Case-insensitive substring filter applied to each line.
    pub search: Option<String>,
}

/// Stream logs from a deployment via Server-Sent Events
///
/// GET /projects/{project_name}/deployments/{deployment_id}/logs
pub async fn stream_deployment_logs(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((project_name, deployment_id)): Path<(String, String)>,
    axum_extra::extract::Query(params): axum_extra::extract::Query<LogStreamParams>,
) -> Result<Sse<impl futures::Stream<Item = Result<Event, anyhow::Error>>>, ServerError> {
    // Fetch project
    let project = projects::find_by_name(&state.db_pool, &project_name)
        .await
        .internal_err("Failed to fetch project")?
        .ok_or_else(|| ServerError::not_found(format!("Project '{}' not found", project_name)))?;

    // Resolve auth for project scope
    let (_user, is_sa) = auth
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

    // Check permission (SA access already validated)
    if !is_sa {
        crate::server::project::handlers::ensure_project_access_or_admin(&state, &_user, &project)
            .await?;
    }

    // Fetch deployment
    let deployment = db_deployments::find_by_project_and_deployment_id(
        &state.db_pool,
        project.id,
        &deployment_id,
    )
    .await
    .internal_err("Failed to fetch deployment")?
    .ok_or_else(|| {
        ServerError::not_found(format!(
            "Deployment '{}' not found for project '{}'",
            deployment_id, project_name
        ))
    })?;

    let start_time = parse_log_time(params.start.as_deref())?;
    let end_time = parse_log_time(params.end.as_deref())?;
    let followable = crate::server::deployment::logs::is_followable_status(&deployment.status);
    if let (Some(start_time), Some(end_time)) = (start_time, end_time) {
        if start_time >= end_time {
            return Err(ServerError::bad_request(
                "Log range start must be before end",
            ));
        }
    }

    // Get log stream from configured runtime log backend.
    // Follow defaults to 1 line so active deployments start at the most
    // recent log entry, while non-follow requests keep the broader backlog.
    let tail = params
        .tail
        .or(Some(if params.follow && followable { 1 } else { 1000 }));
    let follow = params.follow && followable;

    validate_log_levels(&params.level, state.runtime_log_backend.levels())?;
    let levels = params.level.clone();
    let search = params
        .search
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    // Resolve the project's Org-scoped namespace prefix; the backend doesn't
    // have access to `AppState`'s cache, so the caller threads the prefix in
    // via LogQuery (same pattern as the Metacontroller webhook). Only the
    // Kubernetes log backend uses it; Loki ignores it.
    let org_uid =
        crate::server::deployment::webhook::resolve_project_organization_uid(&state, &project)
            .await
            .internal_err("Failed to resolve project organization linkage")?;
    let namespace_prefix = crate::server::deployment::webhook::resolve_project_namespace_prefix(
        &state, &project, org_uid,
    )
    .await
    .internal_err("Failed to resolve project namespace prefix")?;
    let log_stream = state
        .runtime_log_backend
        .stream_logs(
            &deployment,
            &project,
            crate::server::deployment::logs::LogQuery {
                follow,
                tail_lines: tail,
                timestamps: params.timestamps,
                since_seconds: params.since,
                start_time,
                end_time,
                levels,
                search,
                skip_recent: params.skip_recent.filter(|n| *n > 0),
                namespace_prefix: Some(namespace_prefix),
            },
        )
        .await
        .map_err(|e| {
            let error_msg = format!("{:#}", e);
            if error_msg.contains("not ready yet") || error_msg.contains("waiting to start") {
                ServerError::service_unavailable("Deployment logs are not ready yet.").expected()
            } else if error_msg.contains("exceeds the limit") {
                ServerError::bad_request(
                    "Selected time range is too large for the log backend. Pick a shorter range.",
                )
                .expected()
            } else {
                ServerError::internal_anyhow(e, "Failed to stream logs")
            }
        })?;

    // Convert typed log events to SSE events.
    //
    // The `log` event payload is a JSON object `{"line": ..., "level": ...}`
    // so the frontend chart (counted by `detected_level`) and the live log
    // list classify each line identically — the backend is now the single
    // source of truth for per-line level. See the wire-format contract in
    // PR #333 / src/server/deployment/logs.rs (ClassifiedLevel).
    use futures::stream;
    let sse_stream = log_stream.flat_map(|result| match result {
        Ok(crate::server::deployment::logs::LogEvent::Line { text, level }) => {
            stream::iter(vec![Event::default()
                .event("log")
                .json_data(serde_json::json!({ "line": text, "level": level }))
                .map_err(anyhow::Error::from)])
        }
        Ok(crate::server::deployment::logs::LogEvent::Status(status)) => {
            stream::iter(vec![Event::default()
                .event("status")
                .json_data(status)
                .map_err(anyhow::Error::from)])
        }
        Ok(crate::server::deployment::logs::LogEvent::BacklogLoaded { count }) => {
            stream::iter(vec![Event::default()
                .event("backlog_complete")
                .json_data(serde_json::json!({ "count": count }))
                .map_err(anyhow::Error::from)])
        }
        Err(e) => {
            // Send error as SSE event
            error!("Log stream error: {:?}", e);
            stream::iter(vec![Err(e)])
        }
    });

    Ok(Sse::new(sse_stream).keep_alive(KeepAlive::default()))
}

/// Query log counts for a deployment.
///
/// GET /projects/{project_name}/deployments/{deployment_id}/logs/counts
pub async fn query_deployment_log_volume(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((project_name, deployment_id)): Path<(String, String)>,
    axum_extra::extract::Query(params): axum_extra::extract::Query<LogVolumeParams>,
) -> Result<Json<crate::server::deployment::logs::LogVolumeResponse>, ServerError> {
    let project = projects::find_by_name(&state.db_pool, &project_name)
        .await
        .internal_err("Failed to fetch project")?
        .ok_or_else(|| ServerError::not_found(format!("Project '{}' not found", project_name)))?;

    let (_user, is_sa) = auth
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

    if !is_sa {
        crate::server::project::handlers::ensure_project_access_or_admin(&state, &_user, &project)
            .await?;
    }

    let deployment = db_deployments::find_by_project_and_deployment_id(
        &state.db_pool,
        project.id,
        &deployment_id,
    )
    .await
    .internal_err("Failed to fetch deployment")?
    .ok_or_else(|| {
        ServerError::not_found(format!(
            "Deployment '{}' not found for project '{}'",
            deployment_id, project_name
        ))
    })?;

    let start_time = parse_log_time(Some(&params.start))?
        .ok_or_else(|| ServerError::bad_request("Log range start is required"))?;
    let end_time = parse_log_time(Some(&params.end))?
        .ok_or_else(|| ServerError::bad_request("Log range end is required"))?;
    if start_time >= end_time {
        return Err(ServerError::bad_request(
            "Log range start must be before end",
        ));
    }

    validate_log_levels(&params.level, state.runtime_log_backend.levels())?;
    let levels = params.level.clone();
    let search = params
        .search
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    // Cap step + bucket count so a caller can't coerce `build_count_buckets`
    // into emitting an arbitrarily long array. 86_400 = 1d (max meaningful
    // bucket); 10_000 buckets keeps the volume response bounded regardless
    // of the time range the caller picks.
    let step_seconds = params.step_seconds.clamp(1, 86_400);
    let range_secs = (end_time - start_time).num_seconds().max(0);
    if range_secs / step_seconds > 10_000 {
        return Err(ServerError::bad_request(
            "Time range and step_seconds produce too many buckets (max 10000). \
             Widen step_seconds or pick a shorter range.",
        ));
    }

    let volume = state
        .runtime_log_backend
        .query_volume(
            &deployment,
            &project,
            crate::server::deployment::logs::LogVolumeQuery {
                start_time,
                end_time,
                step_seconds,
                levels,
                search,
            },
        )
        .await
        .map_err(|e| {
            let error_msg = format!("{:#}", e);
            if error_msg.contains("exceeds the limit") {
                ServerError::bad_request(
                    "Selected time range is too large for the log backend. Pick a shorter range.",
                )
                .expected()
            } else {
                ServerError::internal_anyhow(e, "Failed to fetch log volume")
            }
        })?;

    Ok(Json(volume))
}

/// Validate caller-supplied `?level=` values against the configured backend's
/// advertised level set. Returns a 400 with the accepted values as suggestions
/// when an unknown level is passed so typos like `warning` (vs `warn`) surface
/// loudly instead of silently returning empty results. An empty `levels` list
/// means "no filter" and is accepted.
fn validate_log_levels(levels: &[String], allowed: &[&'static str]) -> Result<(), ServerError> {
    for level in levels {
        if !allowed.iter().any(|a| a.eq_ignore_ascii_case(level)) {
            return Err(ServerError::bad_request(format!(
                "Unknown log level '{}'. See /api/v1/logs/capabilities for accepted values.",
                level
            ))
            .with_suggestions(Some(allowed.iter().map(|s| (*s).to_string()).collect())));
        }
    }
    Ok(())
}

fn parse_log_time(
    value: Option<&str>,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, ServerError> {
    match value {
        Some(value) if !value.trim().is_empty() => {
            let parsed = chrono::DateTime::parse_from_rfc3339(value).map_err(|_| {
                ServerError::bad_request(format!("Invalid RFC3339 timestamp: {}", value))
            })?;
            Ok(Some(parsed.with_timezone(&chrono::Utc)))
        }
        _ => Ok(None),
    }
}

fn default_log_count_step_seconds() -> i64 {
    60
}

#[cfg(test)]
mod tests {
    use super::{
        normalize_env_override_is_protected, resolve_resource_check_inputs,
        validate_container_resource_names, validate_env_override, validate_env_override_key,
        validate_identity_audiences,
    };
    use crate::server::deployment::models::{ContainerSpec, EnvOverride};
    use axum::http::StatusCode;
    use std::collections::BTreeMap;

    fn make_spec(
        name: &str,
        replicas: Option<u32>,
        cpu: Option<&str>,
        mem: Option<&str>,
    ) -> ContainerSpec {
        ContainerSpec {
            name: name.to_string(),
            image: Some("img".to_string()),
            port: None,
            replicas,
            cpu: cpu.map(str::to_string),
            memory: mem.map(str::to_string),
            env_overrides: vec![],
            health_check: None,
        }
    }

    #[test]
    fn resource_check_inputs_single_container_passthrough() {
        // No `containers` block → use the deployment-wide effective values.
        let (total, per) = resolve_resource_check_inputs(None, 3, "500m", "256Mi");
        assert_eq!(total, 3);
        assert_eq!(per, vec![("500m", "256Mi")]);
    }

    #[test]
    fn resource_check_inputs_sums_replicas_across_containers() {
        // The platform/environment replica cap applies to the WHOLE
        // deployment, so this helper must sum across containers — not
        // check each in isolation.
        let specs = vec![
            make_spec("frontend", Some(2), None, None),
            make_spec("backend", Some(3), None, None),
            make_spec("worker", Some(4), None, None),
        ];
        let (total, per) = resolve_resource_check_inputs(Some(&specs), 1, "500m", "256Mi");
        assert_eq!(
            total, 9,
            "expected sum across containers, not per-container"
        );
        assert_eq!(
            per.len(),
            3,
            "per-container CPU/memory still checked individually"
        );
    }

    #[test]
    fn resource_check_inputs_falls_back_to_defaults_per_container() {
        // Containers that omit replicas/cpu/memory inherit the deployment
        // defaults — matching how the reconciler resolves them.
        let specs = vec![
            make_spec("api", Some(2), Some("1"), Some("512Mi")),
            make_spec("worker", None, None, None), // inherits effective defaults
        ];
        let (total, per) = resolve_resource_check_inputs(Some(&specs), 5, "500m", "256Mi");
        assert_eq!(total, 7, "2 + (default 5) = 7");
        assert_eq!(per[0], ("1", "512Mi"));
        assert_eq!(per[1], ("500m", "256Mi"));
    }

    #[test]
    fn resource_check_inputs_empty_containers_treated_as_legacy() {
        // An empty `containers` slice (shouldn't happen in practice, but
        // defensive) falls through to the single-container path so we don't
        // accidentally accept replicas=0 for every multi-container request.
        let (total, per) = resolve_resource_check_inputs(Some(&[]), 4, "500m", "256Mi");
        assert_eq!(total, 4);
        assert_eq!(per, vec![("500m", "256Mi")]);
    }

    #[test]
    fn container_resource_names_accepts_normal_names() {
        validate_container_resource_names(
            "my-app",
            "default",
            "20260101-000000",
            &["app", "api", "worker"],
        )
        .expect("normal names should be accepted");
    }

    #[test]
    fn container_resource_names_rejects_overlong_service_name() {
        // The deployment group and container name share the 63-char Service-name
        // budget. A long group + container overflows it.
        let long_group = "g".repeat(60);
        let err =
            validate_container_resource_names("proj", &long_group, "20260101-000000", &["api"])
                .unwrap_err();
        assert!(err.message.contains("63-character"), "got: {}", err.message);
    }

    #[test]
    fn container_resource_names_single_container_app_is_checked() {
        // Single-container deployments also emit a suffixed Service `<group>-app`.
        let long_group = "g".repeat(61);
        let err =
            validate_container_resource_names("proj", &long_group, "20260101-000000", &["app"])
                .unwrap_err();
        assert!(err.message.contains("63-character"), "got: {}", err.message);
    }

    #[test]
    fn env_override_key_validation_rejects_empty_keys() {
        assert!(!validate_env_override_key(""));
        assert!(validate_env_override_key("VALID_KEY_123"));
    }

    #[test]
    fn identity_audiences_accepts_valid_entries() {
        let mut audiences = BTreeMap::new();
        audiences.insert("aws".to_string(), "sts.amazonaws.com".to_string());
        audiences.insert("vault".to_string(), "https://vault.example.com".to_string());
        assert!(validate_identity_audiences(&audiences).is_ok());
        assert!(validate_identity_audiences(&BTreeMap::new()).is_ok());
    }

    #[test]
    fn identity_audiences_rejects_bad_filename() {
        let mut audiences = BTreeMap::new();
        audiences.insert("bad/name".to_string(), "sts.amazonaws.com".to_string());
        assert!(validate_identity_audiences(&audiences).is_err());

        let mut dotdot = BTreeMap::new();
        dotdot.insert("..".to_string(), "sts.amazonaws.com".to_string());
        assert!(validate_identity_audiences(&dotdot).is_err());
    }

    #[test]
    fn identity_audiences_rejects_empty_audience() {
        let mut audiences = BTreeMap::new();
        audiences.insert("aws".to_string(), "  ".to_string());
        assert!(validate_identity_audiences(&audiences).is_err());
    }

    #[test]
    fn env_override_validation_rejects_port_overrides() {
        let err = validate_env_override(&EnvOverride {
            key: "PORT".to_string(),
            value: "3000".to_string(),
            is_secret: false,
            is_protected: Some(false),
            source: None,
            for_environment: None,
        })
        .unwrap_err();

        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            err.message,
            "PORT cannot be set via env overrides. Use http_port/--http-port instead."
        );
    }

    #[test]
    fn env_override_validation_rejects_protected_non_secrets() {
        let err = validate_env_override(&EnvOverride {
            key: "API_KEY".to_string(),
            value: "value".to_string(),
            is_secret: false,
            is_protected: Some(true),
            source: None,
            for_environment: None,
        })
        .unwrap_err();

        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            err.message,
            "Env override 'API_KEY' cannot be protected unless it is also secret."
        );
    }

    #[test]
    fn env_override_validation_rejects_empty_keys() {
        let err = validate_env_override(&EnvOverride {
            key: String::new(),
            value: "value".to_string(),
            is_secret: false,
            is_protected: Some(false),
            source: None,
            for_environment: None,
        })
        .unwrap_err();

        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            err.message,
            "Invalid env var key '' (must be alphanumeric with underscores)"
        );
    }

    #[test]
    fn env_override_validation_defaults_secret_overrides_to_protected() {
        let is_protected = validate_env_override(&EnvOverride {
            key: "API_KEY".to_string(),
            value: "secret".to_string(),
            is_secret: true,
            is_protected: None,
            source: None,
            for_environment: None,
        })
        .unwrap();

        assert!(is_protected);
    }

    #[test]
    fn env_override_normalization_preserves_explicit_unprotected_secret() {
        let is_protected = normalize_env_override_is_protected(&EnvOverride {
            key: "API_KEY".to_string(),
            value: "secret".to_string(),
            is_secret: true,
            is_protected: Some(false),
            source: None,
            for_environment: None,
        });

        assert!(!is_protected);
    }

    fn make_override(key: &str, for_environment: Option<&str>) -> EnvOverride {
        EnvOverride {
            key: key.to_string(),
            value: "v".to_string(),
            is_secret: false,
            is_protected: None,
            source: None,
            for_environment: for_environment.map(|s| s.to_string()),
        }
    }

    #[test]
    fn filter_env_overrides_global_always_included() {
        let overrides = vec![make_override("A", None), make_override("B", None)];
        let result = super::filter_env_overrides_by_environment(&overrides, Some("production"));
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn filter_env_overrides_scoped_included_when_env_matches() {
        let overrides = vec![make_override("A", Some("production"))];
        let result = super::filter_env_overrides_by_environment(&overrides, Some("production"));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].key, "A");
    }

    #[test]
    fn filter_env_overrides_scoped_excluded_when_env_differs() {
        let overrides = vec![make_override("A", Some("staging"))];
        let result = super::filter_env_overrides_by_environment(&overrides, Some("production"));
        assert!(result.is_empty());
    }

    #[test]
    fn filter_env_overrides_mixed_global_and_scoped() {
        let overrides = vec![
            make_override("GLOBAL", None),
            make_override("PROD_ONLY", Some("production")),
            make_override("STG_ONLY", Some("staging")),
        ];
        let result = super::filter_env_overrides_by_environment(&overrides, Some("production"));
        assert_eq!(result.len(), 2);
        let keys: Vec<&str> = result.iter().map(|o| o.key.as_str()).collect();
        assert!(keys.contains(&"GLOBAL"));
        assert!(keys.contains(&"PROD_ONLY"));
    }

    #[test]
    fn filter_env_overrides_all_scoped_none_matching() {
        let overrides = vec![
            make_override("A", Some("staging")),
            make_override("B", Some("dev")),
        ];
        let result = super::filter_env_overrides_by_environment(&overrides, Some("production"));
        assert!(result.is_empty());
    }

    #[test]
    fn filter_env_overrides_no_resolved_env_only_globals_pass() {
        let overrides = vec![
            make_override("GLOBAL", None),
            make_override("SCOPED", Some("production")),
        ];
        let result = super::filter_env_overrides_by_environment(&overrides, None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].key, "GLOBAL");
    }

    #[sqlx::test]
    async fn resolve_deployment_target_single_env_auto_selects(pool: sqlx::PgPool) {
        use crate::db::{environments, models::ProjectStatus, projects, users};

        let user = users::create(&pool, "deploy-test@example.com")
            .await
            .unwrap();
        let project = projects::create(
            &pool,
            "single-env-project",
            ProjectStatus::Stopped,
            "public".to_string(),
            Some(user.id),
            None,
            None,
        )
        .await
        .unwrap();

        // Create a single environment
        environments::create(&pool, project.id, "staging", Some("staging"), false, "blue")
            .await
            .unwrap();

        // Neither environment nor group specified → should auto-select the single env
        let (group, env) = super::resolve_deployment_target(&pool, project.id, None, None)
            .await
            .unwrap();
        assert_eq!(group, "staging");
        assert_eq!(env.unwrap().name, "staging");
    }

    #[sqlx::test]
    async fn resolve_deployment_target_fails_with_multiple_envs(pool: sqlx::PgPool) {
        use crate::db::{environments, models::ProjectStatus, projects, users};

        let user = users::create(&pool, "deploy-test@example.com")
            .await
            .unwrap();
        let project = projects::create(
            &pool,
            "multi-env-project",
            ProjectStatus::Stopped,
            "public".to_string(),
            Some(user.id),
            None,
            None,
        )
        .await
        .unwrap();

        // Create two environments
        environments::create(&pool, project.id, "staging", Some("staging"), false, "blue")
            .await
            .unwrap();
        environments::create(
            &pool,
            project.id,
            "production",
            Some("default"),
            true,
            "green",
        )
        .await
        .unwrap();

        // Neither environment nor group specified → should fail with multiple envs
        let err = super::resolve_deployment_target(&pool, project.id, None, None)
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert!(
            err.message.contains("Multiple environments"),
            "unexpected error message: {}",
            err.message
        );
    }

    #[sqlx::test]
    async fn resolve_deployment_target_group_only_multiple_envs_fails(pool: sqlx::PgPool) {
        use crate::db::{environments, models::ProjectStatus, projects, users};

        let user = users::create(&pool, "deploy-test@example.com")
            .await
            .unwrap();
        let project = projects::create(
            &pool,
            "group-multi-env-project",
            ProjectStatus::Stopped,
            "public".to_string(),
            Some(user.id),
            None,
            None,
        )
        .await
        .unwrap();

        // Create two environments
        environments::create(&pool, project.id, "staging", Some("staging"), false, "blue")
            .await
            .unwrap();
        environments::create(
            &pool,
            project.id,
            "production",
            Some("default"),
            true,
            "green",
        )
        .await
        .unwrap();

        // Non-primary group with multiple envs → should fail
        let err = super::resolve_deployment_target(&pool, project.id, None, Some("mr/123"))
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert!(
            err.message.contains("Multiple environments"),
            "unexpected error message: {}",
            err.message
        );
    }

    #[sqlx::test]
    async fn resolve_deployment_target_cross_group_rejected(pool: sqlx::PgPool) {
        use crate::db::{environments, models::ProjectStatus, projects, users};

        let user = users::create(&pool, "deploy-test@example.com")
            .await
            .unwrap();
        let project = projects::create(
            &pool,
            "cross-group-project",
            ProjectStatus::Stopped,
            "public".to_string(),
            Some(user.id),
            None,
            None,
        )
        .await
        .unwrap();

        environments::create(
            &pool,
            project.id,
            "production",
            Some("default"),
            true,
            "green",
        )
        .await
        .unwrap();
        environments::create(&pool, project.id, "staging", Some("staging"), false, "blue")
            .await
            .unwrap();

        // "default" is primary of "production", but env is "staging" → should reject
        let err =
            super::resolve_deployment_target(&pool, project.id, Some("staging"), Some("default"))
                .await
                .unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert!(
            err.message.contains("primary deployment group"),
            "unexpected error message: {}",
            err.message
        );
    }

    #[sqlx::test]
    async fn resolve_deployment_target_none_none_multiple_envs_fails(pool: sqlx::PgPool) {
        use crate::db::{environments, models::ProjectStatus, projects, users};

        let user = users::create(&pool, "deploy-test@example.com")
            .await
            .unwrap();
        let project = projects::create(
            &pool,
            "nonenone-multi-project",
            ProjectStatus::Stopped,
            "public".to_string(),
            Some(user.id),
            None,
            None,
        )
        .await
        .unwrap();

        // Create two environments
        environments::create(&pool, project.id, "staging", Some("staging"), false, "blue")
            .await
            .unwrap();
        environments::create(
            &pool,
            project.id,
            "production",
            Some("default"),
            true,
            "green",
        )
        .await
        .unwrap();

        // Neither group nor environment with multiple envs → should fail
        let err = super::resolve_deployment_target(&pool, project.id, None, None)
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert!(
            err.message.contains("Multiple environments"),
            "unexpected error message: {}",
            err.message
        );
    }

    #[sqlx::test]
    async fn resolve_deployment_target_cross_group_ok(pool: sqlx::PgPool) {
        use crate::db::{environments, models::ProjectStatus, projects, users};

        let user = users::create(&pool, "deploy-test@example.com")
            .await
            .unwrap();
        let project = projects::create(
            &pool,
            "cross-group-ok-project",
            ProjectStatus::Stopped,
            "public".to_string(),
            Some(user.id),
            None,
            None,
        )
        .await
        .unwrap();

        environments::create(
            &pool,
            project.id,
            "production",
            Some("default"),
            true,
            "green",
        )
        .await
        .unwrap();

        // "default" is primary of "production", env is also "production" → should succeed
        let (group, env) = super::resolve_deployment_target(
            &pool,
            project.id,
            Some("production"),
            Some("default"),
        )
        .await
        .unwrap();
        assert_eq!(group, "default");
        assert_eq!(env.unwrap().name, "production");
    }

    #[sqlx::test]
    async fn resolve_deployment_target_group_not_primary_single_env(pool: sqlx::PgPool) {
        use crate::db::{environments, models::ProjectStatus, projects, users};

        let user = users::create(&pool, "deploy-test@example.com")
            .await
            .unwrap();
        let project = projects::create(
            &pool,
            "group-single-env-project",
            ProjectStatus::Stopped,
            "public".to_string(),
            Some(user.id),
            None,
            None,
        )
        .await
        .unwrap();

        environments::create(&pool, project.id, "staging", Some("staging"), false, "blue")
            .await
            .unwrap();

        // Non-primary group with single env → auto-resolve to that env
        let (group, env) =
            super::resolve_deployment_target(&pool, project.id, None, Some("mr/123"))
                .await
                .unwrap();
        assert_eq!(group, "mr/123");
        assert_eq!(env.unwrap().name, "staging");
    }
}
