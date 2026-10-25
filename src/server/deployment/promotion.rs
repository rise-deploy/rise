//! Creating a deployment from another one (ADR-0007).
//!
//! Redeploys, rollbacks, promotions and Rollback Guard replacements are all
//! "create a deployment in environment E from deployment D". This module is
//! the one implementation of that: the promotion-source gate, the migration
//! barrier check, target-relative sizing, image pinning, and
//! [`create_from_source`], which writes the row, its complete environment
//! variable materialization and its creation event in a single transaction.
//! [`plan`] previews the same decisions without writing anything.

use chrono::{DateTime, Utc};
use serde::Serialize;
use tracing::{error, info, warn};
use uuid::Uuid;

use super::models::{self, ContainerSpec, EnvOverride};
use crate::db::deployments as db_deployments;
use crate::db::migration_barriers::{self, MigrationBarrier};
use crate::db::models::{
    CreatedVia, Deployment, DeploymentStatus, EnvVarSource, Environment, Project,
};
use crate::server::error::{ServerError, ServerErrorExt};
use crate::server::extensions::InjectedEnvVarValue;
use crate::server::registry::ImageTagType;
use crate::server::state::AppState;
use rise_deployment_spec::project_config::DEFAULT_CONTAINER_NAME;

// ---------------------------------------------------------------------------
// Classification and the promotion-source gate
// ---------------------------------------------------------------------------

/// How a deployment created from `source_environment_id` into
/// `target_environment_id` is recorded: crossing environments is a promotion.
pub fn classify_created_via(
    source_environment_id: Uuid,
    target_environment_id: Uuid,
) -> CreatedVia {
    if source_environment_id == target_environment_id {
        CreatedVia::Redeploy
    } else {
        CreatedVia::Promote
    }
}

/// Outcome of the promotion-source gate for one create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    /// The target has no promotion source, or the create comes from it (or
    /// from the target itself).
    Allowed,
    /// Denied by policy, but admitted for an unscoped human administrator.
    AdminBypass { required: String },
    /// Denied: the target only accepts deployments promoted from `required`.
    Denied { required: String },
}

impl GateDecision {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::AdminBypass { .. } => "admin_bypass",
            Self::Denied { .. } => "denied",
        }
    }
}

/// Decide whether a deployment may be created in `target`.
///
/// `source_env_id` is the environment of the deployment the create is made
/// from, `None` for a fresh build or `--image`. A target with a promotion
/// source `S` admits only creates from a deployment in `S` or in the target
/// itself (a redeploy or rollback of something already admitted). Everything
/// else is denied unless `admin_unscoped`: an unscoped, human administrator
/// session — never a service account or a `--scope` login.
///
/// `required_name` is `S`'s name, for messages.
pub fn gate(
    target: &Environment,
    source_env_id: Option<Uuid>,
    required_name: Option<&str>,
    admin_unscoped: bool,
) -> GateDecision {
    let Some(required_id) = target.promotion_source_environment_id else {
        return GateDecision::Allowed;
    };
    if source_env_id.is_some_and(|id| id == required_id || id == target.id) {
        return GateDecision::Allowed;
    }
    let required = required_name.unwrap_or("its promotion source").to_string();
    if admin_unscoped {
        GateDecision::AdminBypass { required }
    } else {
        GateDecision::Denied { required }
    }
}

/// The 403 for [`GateDecision::Denied`].
pub fn gate_denied_error(target: &str, required: &str) -> ServerError {
    ServerError::forbidden(format!(
        "Environment '{target}' only accepts deployments promoted from '{required}'. \
         Promote a {required} deployment: rise promote <deployment-id> -E {target}"
    ))
    .with_error_code("promotion_source_required")
    .with_details(serde_json::json!({
        "environment": target,
        "promotion_source_environment": required,
    }))
}

/// The warning returned when an administrator bypassed the gate.
pub fn gate_bypass_warning(target: &str, required: &str) -> String {
    format!(
        "Environment '{target}' only accepts deployments promoted from '{required}'; \
         this deployment was admitted because you are an administrator"
    )
}

// ---------------------------------------------------------------------------
// Migration barriers
// ---------------------------------------------------------------------------

/// Outcome of the migration-barrier check for a candidate deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BarrierOutcome {
    /// A forward move, an equal version, or nothing to cross.
    Ok,
    /// The candidate declares no `migration_version`, so whether it crosses a
    /// barrier cannot be told.
    Unknown { has_barriers: bool },
    /// Moving the environment from `environment` down to `candidate` crosses
    /// these barriers.
    Crossed {
        candidate: i64,
        environment: i64,
        barriers: Vec<MigrationBarrier>,
    },
}

impl BarrierOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Unknown { .. } => "unknown",
            Self::Crossed { .. } => "crossed",
        }
    }
}

/// What the ledger has to be asked about a candidate, before asking it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BarrierRange {
    Ok,
    Unknown,
    /// Barriers `v` with `low_exclusive < v <= high_inclusive` are crossed.
    Check {
        low_exclusive: i64,
        high_inclusive: i64,
    },
}

/// `candidate` is the deployment's `migration_version`, `high_water` the
/// environment's (the highest version that may have run its migrations).
fn barrier_range(candidate: Option<i64>, high_water: Option<i64>) -> BarrierRange {
    match (candidate, high_water) {
        // Nothing in the environment declared a version: nothing to protect.
        (_, None) => BarrierRange::Ok,
        (None, Some(_)) => BarrierRange::Unknown,
        (Some(a), Some(b)) if a >= b => BarrierRange::Ok,
        (Some(a), Some(b)) => BarrierRange::Check {
            low_exclusive: a,
            high_inclusive: b,
        },
    }
}

/// Check `candidate` against the target environment's high-water mark and
/// the project's barrier ledger.
pub async fn check_barriers(
    pool: &sqlx::PgPool,
    project_id: Uuid,
    target_env_id: Uuid,
    candidate: Option<i64>,
) -> Result<BarrierOutcome, ServerError> {
    let high_water = db_deployments::migration_high_water(pool, target_env_id)
        .await
        .internal_err("Failed to read the environment's migration version")?;
    check_barriers_against(pool, project_id, candidate, high_water).await
}

async fn check_barriers_against(
    pool: &sqlx::PgPool,
    project_id: Uuid,
    candidate: Option<i64>,
    high_water: Option<i64>,
) -> Result<BarrierOutcome, ServerError> {
    match barrier_range(candidate, high_water) {
        BarrierRange::Ok => Ok(BarrierOutcome::Ok),
        BarrierRange::Unknown => {
            let has_barriers = migration_barriers::project_has_any(pool, project_id)
                .await
                .internal_err("Failed to read migration barriers")?;
            Ok(BarrierOutcome::Unknown { has_barriers })
        }
        BarrierRange::Check {
            low_exclusive,
            high_inclusive,
        } => {
            let barriers =
                migration_barriers::list_crossed(pool, project_id, low_exclusive, high_inclusive)
                    .await
                    .internal_err("Failed to read migration barriers")?;
            if barriers.is_empty() {
                Ok(BarrierOutcome::Ok)
            } else {
                Ok(BarrierOutcome::Crossed {
                    candidate: low_exclusive,
                    environment: high_inclusive,
                    barriers,
                })
            }
        }
    }
}

/// A barrier as returned to clients.
#[derive(Debug, Clone, Serialize)]
pub struct BarrierView {
    pub version: i64,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recorded_by: Option<String>,
    pub recorded_at: String,
}

impl From<&MigrationBarrier> for BarrierView {
    fn from(b: &MigrationBarrier) -> Self {
        Self {
            version: b.version,
            reason: b.reason.clone(),
            recorded_by: b.recorded_by_email.clone(),
            recorded_at: b.recorded_at.to_rfc3339(),
        }
    }
}

fn barrier_list(barriers: &[MigrationBarrier]) -> String {
    barriers
        .iter()
        .map(|b| format!("v{} ({})", b.version, b.reason))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The 409 for a crossed barrier on a manual create.
pub fn barrier_crossed_error(
    target: &str,
    candidate: i64,
    environment: i64,
    barriers: &[MigrationBarrier],
) -> ServerError {
    ServerError::conflict(format!(
        "Deploying migration version {candidate} to '{target}' (at migration version \
         {environment}) crosses migration barrier(s) {}: the database may no longer work with \
         this code. Retry with --force to deploy anyway.",
        barrier_list(barriers)
    ))
    .with_error_code("migration_barrier")
    .with_details(serde_json::json!({
        "candidate_version": candidate,
        "environment_version": environment,
        "barriers": barriers.iter().map(BarrierView::from).collect::<Vec<_>>(),
    }))
}

/// The warning for a forced crossing.
pub fn barrier_forced_warning(
    target: &str,
    candidate: i64,
    environment: i64,
    barriers: &[MigrationBarrier],
) -> String {
    format!(
        "Forced across migration barrier(s) {}: '{target}' is at migration version \
         {environment}, this deployment declares {candidate}",
        barrier_list(barriers)
    )
}

/// The warning for an undeclared version in a project that records barriers.
pub fn barrier_unknown_warning(target: &str) -> String {
    format!(
        "This deployment declares no migration_version, so Rise cannot tell whether it is \
         compatible with the database of '{target}'. Set [project].migration_version in rise.toml"
    )
}

// ---------------------------------------------------------------------------
// Target-relative sizing
// ---------------------------------------------------------------------------

/// One change made to an inherited resource value to fit the target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResourceAdjustment {
    pub container: String,
    pub field: &'static str,
    pub from: String,
    pub to: String,
    pub reason: String,
}

/// The resource range a deployment into an environment must fit: the
/// environment's own constraints, falling back to the platform's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveConstraints {
    pub environment: String,
    pub min_replicas: u32,
    pub max_replicas: u32,
    pub min_cpu: String,
    pub max_cpu: String,
    pub min_memory: String,
    pub max_memory: String,
}

impl EffectiveConstraints {
    pub fn resolve(
        platform: Option<&crate::server::settings::DeploymentConstraints>,
        environment: &Environment,
    ) -> Self {
        let platform = platform.cloned().unwrap_or_default();
        Self {
            environment: environment.name.clone(),
            min_replicas: environment
                .min_replicas
                .map(|v| v as u32)
                .unwrap_or(platform.min_replicas),
            max_replicas: environment
                .max_replicas
                .map(|v| v as u32)
                .unwrap_or(platform.max_replicas),
            min_cpu: environment.min_cpu.clone().unwrap_or(platform.min_cpu),
            max_cpu: environment.max_cpu.clone().unwrap_or(platform.max_cpu),
            min_memory: environment
                .min_memory
                .clone()
                .unwrap_or(platform.min_memory),
            max_memory: environment
                .max_memory
                .clone()
                .unwrap_or(platform.max_memory),
        }
    }
}

/// Resource values the request set explicitly. They are not clamped: the
/// caller validates them like any other request value.
#[derive(Debug, Clone, Default)]
pub struct ExplicitResources {
    pub replicas: Option<u32>,
    pub cpu: Option<String>,
    pub memory: Option<String>,
}

/// The sizing of a deployment created from another one.
#[derive(Debug, Clone)]
pub struct ClampedResources {
    pub replicas: u32,
    pub cpu: String,
    pub memory: String,
    /// The source's per-container specs with cpu/memory clamped, for a
    /// multi-container source.
    pub containers: Option<Vec<ContainerSpec>>,
    pub adjustments: Vec<ResourceAdjustment>,
}

fn clamp_quantity(
    container: &str,
    field: &'static str,
    value: &str,
    min: &str,
    max: &str,
    environment: &str,
    adjustments: &mut Vec<ResourceAdjustment>,
) -> Result<String, ServerError> {
    let clamp = match field {
        "cpu" => super::quantity::clamp_cpu_to_range,
        _ => super::quantity::clamp_memory_to_range,
    };
    let (clamped, changed) = clamp(value, min, max).map_err(|e| {
        ServerError::bad_request(format!(
            "Cannot fit inherited {field} '{value}' of container '{container}' into \
             environment '{environment}': {e}"
        ))
    })?;
    if changed {
        adjustments.push(ResourceAdjustment {
            container: container.to_string(),
            field,
            from: value.to_string(),
            to: clamped.clone(),
            reason: format!("{environment} allows {field} in [{min}, {max}]"),
        });
    }
    Ok(clamped)
}

fn clamp_replicas(
    container: &str,
    value: u32,
    constraints: &EffectiveConstraints,
    adjustments: &mut Vec<ResourceAdjustment>,
) -> u32 {
    let (lo, hi) = (constraints.min_replicas, constraints.max_replicas);
    let clamped = value.max(lo).min(hi.max(lo));
    if clamped != value {
        let reason = if clamped > value {
            format!("{} requires ≥ {lo}", constraints.environment)
        } else {
            format!("{} allows ≤ {hi}", constraints.environment)
        };
        adjustments.push(ResourceAdjustment {
            container: container.to_string(),
            field: "replicas",
            from: value.to_string(),
            to: clamped.to_string(),
            reason,
        });
    }
    clamped
}

/// Fit the resources a promotion inherits from `source` into the target's
/// `constraints`.
///
/// Inherited values are clamped into range and every change is reported;
/// `explicit` values are taken as they are. `specs` are the source's
/// per-container specs (multi-container): their cpu/memory are clamped per
/// container, while replicas, which the range bounds in total, cannot be
/// split automatically — a sum out of range is a 400.
pub fn clamp_inherited_resources(
    source: &Deployment,
    specs: Option<Vec<ContainerSpec>>,
    explicit: &ExplicitResources,
    constraints: &EffectiveConstraints,
) -> Result<ClampedResources, ServerError> {
    let mut adjustments = Vec::new();
    let multi = specs.as_ref().is_some_and(|s| !s.is_empty());

    // Deployment-level values. For a multi-container source they only back
    // up unset container fields, so their adjustments are not reported.
    let mut level_adjustments = Vec::new();
    let replicas = match explicit.replicas {
        Some(r) => r,
        None => clamp_replicas(
            DEFAULT_CONTAINER_NAME,
            source.replicas.max(0) as u32,
            constraints,
            &mut level_adjustments,
        ),
    };
    let cpu = match &explicit.cpu {
        Some(cpu) => cpu.clone(),
        None => clamp_quantity(
            DEFAULT_CONTAINER_NAME,
            "cpu",
            &source.cpu,
            &constraints.min_cpu,
            &constraints.max_cpu,
            &constraints.environment,
            &mut level_adjustments,
        )?,
    };
    let memory = match &explicit.memory {
        Some(memory) => memory.clone(),
        None => clamp_quantity(
            DEFAULT_CONTAINER_NAME,
            "memory",
            &source.memory,
            &constraints.min_memory,
            &constraints.max_memory,
            &constraints.environment,
            &mut level_adjustments,
        )?,
    };
    if !multi {
        adjustments = level_adjustments;
    }

    let containers = match specs {
        Some(mut specs) if multi => {
            let total: u32 = specs.iter().map(|s| s.replicas.unwrap_or(replicas)).sum();
            if total < constraints.min_replicas || total > constraints.max_replicas {
                return Err(ServerError::bad_request(format!(
                    "The containers of the source deployment run {total} replicas in total, but \
                     environment '{}' allows [{}, {}]. Inherited per-container replicas are not \
                     adjusted automatically; ask an administrator to adjust the environment's \
                     replica constraints",
                    constraints.environment, constraints.min_replicas, constraints.max_replicas
                )));
            }
            for spec in &mut specs {
                let spec_cpu = spec.cpu.clone().unwrap_or_else(|| cpu.clone());
                spec.cpu = Some(clamp_quantity(
                    &spec.name,
                    "cpu",
                    &spec_cpu,
                    &constraints.min_cpu,
                    &constraints.max_cpu,
                    &constraints.environment,
                    &mut adjustments,
                )?);
                let spec_memory = spec.memory.clone().unwrap_or_else(|| memory.clone());
                spec.memory = Some(clamp_quantity(
                    &spec.name,
                    "memory",
                    &spec_memory,
                    &constraints.min_memory,
                    &constraints.max_memory,
                    &constraints.environment,
                    &mut adjustments,
                )?);
            }
            Some(specs)
        }
        _ => None,
    };

    Ok(ClampedResources {
        replicas,
        cpu,
        memory,
        containers,
        adjustments,
    })
}

/// Decode a deployment's persisted per-container specs.
pub fn decode_source_specs(source: &Deployment) -> Result<Option<Vec<ContainerSpec>>, ServerError> {
    match source.containers.as_ref() {
        Some(v) => models::decode_side_data(v).map(Some).map_err(|e| {
            ServerError::internal(format!(
                "Source deployment {} ({}) has a non-NULL `containers` column that could not be \
                 decoded into Vec<ContainerSpec>: {:?}",
                source.id, source.deployment_id, e
            ))
        }),
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Image pinning
// ---------------------------------------------------------------------------

/// The deployment whose image `source` runs: itself, or the one it was
/// created from.
pub fn image_origin_id(source: &Deployment) -> Uuid {
    source.rolled_back_from_deployment_id.unwrap_or(source.id)
}

/// A promotion's images, pinned to immutable digests.
#[derive(Debug, Clone, Default)]
pub struct PinnedImages {
    /// Single-container: the digest reference to store as `image_digest`.
    pub image_digest: Option<String>,
    /// Multi-container: the side-data with every image pinned.
    pub containers: Option<serde_json::Value>,
    /// What to show the user as "the" pinned image (single-container only).
    pub pinned_image: Option<String>,
}

fn image_unresolvable(image: &str, e: impl std::fmt::Display) -> ServerError {
    ServerError::bad_request(format!(
        "Cannot pin image '{image}' for promotion: {e}. The source deployment's image must still \
         be in the registry"
    ))
    .with_error_code("image_unresolvable")
}

/// Pin the image(s) a promotion of `source` will run, so the target runs
/// exactly what the source environment ran even if a tag moves later.
///
/// `containers` is the side-data the new deployment will carry (`None` for a
/// single-container deployment); images already pinned (`@sha256:`) are kept.
pub async fn pin_images(
    state: &AppState,
    project: &Project,
    source: &Deployment,
    containers: Option<&serde_json::Value>,
) -> Result<PinnedImages, ServerError> {
    if let Some(side_data) = containers {
        let mut specs: Vec<ContainerSpec> = models::decode_side_data(side_data)
            .internal_err("Failed to decode container side-data")?;
        if !specs.is_empty() {
            for spec in &mut specs {
                let Some(image) = spec.image.clone() else {
                    return Err(image_unresolvable(&spec.name, "the container has no image"));
                };
                if image.contains("@sha256:") {
                    continue;
                }
                let pinned = super::handlers::resolve_image_digest(
                    &state.oci_client,
                    &state.registry_provider,
                    &image,
                    &project.name,
                )
                .await
                .map_err(|e| image_unresolvable(&image, format!("{e:#}")))?;
                spec.image = Some(pinned);
            }
            let encoded =
                models::encode_side_data(&specs).internal_err("Failed to serialise containers")?;
            return Ok(PinnedImages {
                image_digest: None,
                containers: Some(encoded),
                pinned_image: None,
            });
        }
    }

    if let Some(digest) = source.image_digest.as_ref() {
        return Ok(PinnedImages {
            image_digest: Some(digest.clone()),
            containers: None,
            pinned_image: Some(digest.clone()),
        });
    }

    let origin_id = image_origin_id(source);
    let origin_tag = if origin_id == source.id {
        source.deployment_id.clone()
    } else {
        db_deployments::find_by_id(&state.db_pool, origin_id)
            .await
            .internal_err("Failed to look up the image's origin deployment")?
            .map(|d| d.deployment_id)
            .ok_or_else(|| {
                image_unresolvable(
                    &source.deployment_id,
                    "the deployment that built the image no longer exists",
                )
            })?
    };
    let tag =
        state
            .registry_provider
            .get_image_tag(&project.name, &origin_tag, ImageTagType::Internal);
    let pinned = super::handlers::resolve_image_digest(
        &state.oci_client,
        &state.registry_provider,
        &tag,
        &project.name,
    )
    .await
    .map_err(|e| image_unresolvable(&tag, format!("{e:#}")))?;
    Ok(PinnedImages {
        image_digest: Some(pinned.clone()),
        containers: None,
        pinned_image: Some(pinned),
    })
}

// ---------------------------------------------------------------------------
// Environment variables
// ---------------------------------------------------------------------------

/// A deployment environment variable ready to write: validated, and
/// encrypted when secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedEnvVar {
    pub key: String,
    pub value: String,
    pub is_secret: bool,
    pub is_protected: bool,
    pub source: String,
}

/// The `source` stored for a request override. Only client-allowed values
/// (`toml`, `cli`) are accepted, anything else becomes `cli`; a `rise.toml`
/// variable scoped to an environment is stored as `toml:<env>`.
pub fn override_source(env_override: &EnvOverride) -> EnvVarSource {
    let source = env_override
        .source
        .as_deref()
        .and_then(EnvVarSource::parse)
        .filter(|s| s.is_client_allowed())
        .unwrap_or(EnvVarSource::Cli);
    match (source, &env_override.for_environment) {
        (EnvVarSource::Toml, Some(env)) => EnvVarSource::TomlEnv(env.clone()),
        (source, _) => source,
    }
}

/// Validate, filter (by `for_environment`) and encrypt request overrides for
/// a deployment into `target_environment`.
pub async fn prepare_env_overrides(
    state: &AppState,
    overrides: &[EnvOverride],
    target_environment: &str,
) -> Result<Vec<PreparedEnvVar>, ServerError> {
    let filtered =
        super::handlers::filter_env_overrides_by_environment(overrides, Some(target_environment));
    let mut prepared = Vec::with_capacity(filtered.len());
    for env_override in filtered {
        let is_protected = super::handlers::validate_env_override(env_override)?;
        let value = if env_override.is_secret {
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
        prepared.push(PreparedEnvVar {
            key: env_override.key.clone(),
            value,
            is_secret: env_override.is_secret,
            is_protected,
            source: override_source(env_override).as_str().into_owned(),
        });
    }
    Ok(prepared)
}

/// Upsert `vars` onto a deployment, later entries winning.
pub async fn write_env_vars(
    conn: &mut sqlx::PgConnection,
    deployment_id: Uuid,
    vars: &[PreparedEnvVar],
) -> Result<(), ServerError> {
    for var in vars {
        crate::db::env_vars::upsert_deployment_env_var(
            &mut *conn,
            deployment_id,
            &var.key,
            &var.value,
            var.is_secret,
            var.is_protected,
            &var.source,
        )
        .await
        .internal_err(format!("Failed to set environment variable '{}'", var.key))?;
    }
    Ok(())
}

/// Run every registered extension's `before_deployment` hook for a
/// deployment into `group`, collecting the variables they inject.
pub async fn run_extension_hooks(
    state: &AppState,
    project_id: Uuid,
    group: &str,
) -> Result<Vec<PreparedEnvVar>, ServerError> {
    let mut vars = Vec::new();
    for (_, extension) in state.extension_registry.iter() {
        let injected = extension
            .before_deployment(project_id, group)
            .await
            .map_err(|e| {
                ServerError::internal_anyhow(
                    e,
                    format!(
                        "Extension type '{}' before_deployment hook failed",
                        extension.extension_type()
                    ),
                )
            })?;
        for var in injected {
            let (value, is_secret, is_protected) = match var.value {
                InjectedEnvVarValue::Plain(v) => (v, false, false),
                InjectedEnvVarValue::Secret { encrypted, .. } => (encrypted, true, false),
                InjectedEnvVarValue::Protected { encrypted, .. } => (encrypted, true, true),
            };
            vars.push(PreparedEnvVar {
                key: var.key,
                value,
                is_secret,
                is_protected,
                source: EnvVarSource::Extension.as_str().into_owned(),
            });
        }
    }
    Ok(vars)
}

/// The Rise system variables (`RISE_*`, `PORT` excluded) for `deployment`.
pub async fn rise_system_env_vars(
    state: &AppState,
    deployment: &Deployment,
    project: &Project,
    environment_name: Option<&str>,
) -> Result<Vec<PreparedEnvVar>, ServerError> {
    let deployment_urls = state
        .deployment_backend
        .get_deployment_urls(deployment, project)
        .await
        .internal_err("Failed to get deployment URLs")?;
    Ok(models::rise_system_env_vars(
        &state.public_url,
        &deployment.deployment_group,
        &deployment_urls,
        environment_name,
    )
    .into_iter()
    .map(|(key, value)| PreparedEnvVar {
        key: key.to_string(),
        value,
        is_secret: false,
        is_protected: false,
        source: EnvVarSource::System.as_str().into_owned(),
    })
    .collect())
}

/// Write the Rise system variables (`PORT` excluded) for `deployment`.
pub async fn insert_rise_env_vars_with(
    conn: &mut sqlx::PgConnection,
    state: &AppState,
    deployment: &Deployment,
    project: &Project,
    environment_name: Option<&str>,
) -> Result<(), ServerError> {
    let vars = rise_system_env_vars(state, deployment, project, environment_name).await?;
    write_env_vars(conn, deployment.id, &vars).await
}

// ---------------------------------------------------------------------------
// The primitive
// ---------------------------------------------------------------------------

/// Where a new deployment's project-level variables come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvMode {
    /// The target environment's current project variables, plus the source's
    /// `rise.toml`/CLI variables (environment-scoped ones only for the target).
    ProjectCurrent,
    /// The source's variable snapshot, minus `system` and `extension`
    /// variables, which are derived fresh.
    SourceSnapshot,
}

/// Everything [`create_from_source`] needs. Validation (gate, barriers,
/// sizing, pinning) is the caller's: this only writes.
pub struct CreateFromSource<'a> {
    pub project: &'a Project,
    pub source: &'a Deployment,
    pub target_env: &'a Environment,
    pub group: &'a str,
    /// The new deployment's `deployment_id` (see `utils::generate_deployment_id`).
    pub deployment_id: &'a str,
    pub created_by_id: Uuid,
    pub created_via: CreatedVia,
    pub env_mode: EnvMode,
    /// Request overrides; filtered for the target environment here.
    pub overrides: &'a [EnvOverride],
    pub http_port: u16,
    pub replicas: u32,
    pub cpu: String,
    pub memory: String,
    pub containers: Option<serde_json::Value>,
    pub routes: Option<serde_json::Value>,
    pub identity_audiences: serde_json::Value,
    pub image: Option<String>,
    pub image_digest: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub expiration_cap: Option<rise_backend_core::expiration::ExpirationCap>,
    pub job_url: Option<String>,
    pub pull_request_url: Option<String>,
    pub git_repository_url: Option<String>,
    /// Rollback Guard only: the unhealthy deployment this one replaces.
    pub rollback_guard_for: Option<Uuid>,
    /// Rollback Guard only: lock `rollback_guard_for` inside the transaction
    /// and re-check that it is still a subject after this many seconds of
    /// unhealthiness. The create is abandoned (`Ok(None)`) when it is not.
    pub guard_lock: Option<i32>,
    /// Merged into the creation event (a JSON object; empty for none).
    pub extra_event_attributes: serde_json::Value,
}

/// Create a deployment from `req.source` in `req.target_env`.
///
/// Extension hooks run and secret overrides are encrypted first; then the
/// row (status `Pushed`), all of its environment variables and its creation
/// event commit in one transaction (see [`create_from_source_in`]), so a
/// `Pushed` deployment never exists without its variables and a failed hook
/// leaves nothing behind.
///
/// Returns `Ok(None)` only for a Rollback Guard create that lost: the subject
/// recovered or no longer qualifies (`guard_lock`), or another replacement
/// for it already exists.
pub async fn create_from_source(
    state: &AppState,
    req: CreateFromSource<'_>,
) -> Result<Option<Deployment>, ServerError> {
    let extension_vars = run_extension_hooks(state, req.project.id, req.group).await?;
    let overrides = prepare_env_overrides(state, req.overrides, &req.target_env.name).await?;
    let project = req.project;
    let project_name = project.name.clone();
    let environment_name = req.target_env.name.clone();

    let created = create_from_source_in(
        &state.db_pool,
        req,
        extension_vars,
        overrides,
        |deployment: Deployment| async move {
            rise_system_env_vars(state, &deployment, project, Some(&environment_name)).await
        },
    )
    .await?;

    if created.is_some() {
        if let Some(ref kube_client) = state.kube_client {
            if let Err(e) = super::crd::trigger_resync(kube_client, &project_name).await {
                warn!(
                    project = %project_name,
                    "Failed to trigger CRD resync after creating deployment from another: {:?}", e
                );
            }
        }
    }
    Ok(created)
}

/// The transactional part of [`create_from_source`], with everything that
/// needs more than the database already prepared: `extension_vars` (from the
/// extension hooks), `overrides` (validated and encrypted request overrides)
/// and `system_vars`, which derives the `RISE_*` variables of the new row.
///
/// Variables, later layers winning: the project's or the source's (per
/// [`EnvMode`]), `extension_vars`, `overrides`, then `PORT` and the system
/// variables. `req.overrides` is not read here.
pub async fn create_from_source_in<F, Fut>(
    pool: &sqlx::PgPool,
    req: CreateFromSource<'_>,
    extension_vars: Vec<PreparedEnvVar>,
    overrides: Vec<PreparedEnvVar>,
    system_vars: F,
) -> Result<Option<Deployment>, ServerError>
where
    F: FnOnce(Deployment) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<PreparedEnvVar>, ServerError>>,
{
    let (promoted_from_deployment_id, promoted_from_environment_id) =
        if req.created_via == CreatedVia::Promote {
            (Some(req.source.id), Some(req.source.environment_id))
        } else {
            (None, None)
        };
    let extra_event_attributes = req
        .extra_event_attributes
        .as_object()
        .filter(|o| !o.is_empty())
        .map(|_| &req.extra_event_attributes);

    let mut tx = pool
        .begin()
        .await
        .internal_err("Failed to start transaction")?;

    if let Some(timeout_seconds) = req.guard_lock {
        let subject = req.rollback_guard_for.ok_or_else(|| {
            ServerError::internal("A Rollback Guard lock needs the deployment it replaces")
        })?;
        let locked = db_deployments::lock_rollback_guard_subject(&mut tx, subject, timeout_seconds)
            .await
            .internal_err("Failed to lock the Rollback Guard subject")?;
        if locked.is_none() {
            info!(
                subject = %subject,
                "Rollback Guard subject no longer qualifies; not creating a replacement"
            );
            if let Err(e) = tx.rollback().await {
                warn!("Failed to roll back Rollback Guard transaction: {:?}", e);
            }
            return Ok(None);
        }
    }

    let created = db_deployments::create(
        &mut *tx,
        db_deployments::CreateDeploymentParams {
            deployment_id: req.deployment_id,
            project_id: req.project.id,
            created_by_id: req.created_by_id,
            status: DeploymentStatus::Pushed,
            image: req.image.as_deref(),
            image_digest: req.image_digest.as_deref(),
            rolled_back_from_deployment_id: Some(image_origin_id(req.source)),
            deployment_group: req.group,
            environment_id: req.target_env.id,
            expires_at: req.expires_at,
            http_port: i32::from(req.http_port),
            is_active: false,
            job_url: req.job_url.as_deref(),
            pull_request_url: req.pull_request_url.as_deref(),
            git_repository_url: req.git_repository_url.as_deref(),
            replicas: req.replicas as i32,
            cpu: &req.cpu,
            memory: &req.memory,
            identity_audiences: req.identity_audiences.clone(),
            containers: req.containers.as_ref(),
            routes: req.routes.as_ref(),
            expiration_cap: req.expiration_cap.as_ref(),
            created_via: req.created_via,
            promoted_from_deployment_id,
            promoted_from_environment_id,
            migration_version: req.source.migration_version,
            rollback_guard_for_deployment_id: req.rollback_guard_for,
            extra_event_attributes,
        },
    )
    .await;
    let deployment = match created {
        Ok(deployment) => deployment,
        Err(e)
            if req.rollback_guard_for.is_some()
                && db_deployments::is_rollback_guard_candidate_conflict(&e) =>
        {
            info!(
                subject = ?req.rollback_guard_for,
                "A Rollback Guard replacement already exists; not creating another"
            );
            return Ok(None);
        }
        Err(e) => {
            return Err(ServerError::internal_anyhow(
                e,
                "Failed to create deployment",
            ))
        }
    };

    match req.env_mode {
        EnvMode::ProjectCurrent => {
            crate::db::env_vars::copy_project_env_vars_to_deployment(
                &mut *tx,
                req.project.id,
                deployment.id,
                req.target_env.id,
                &req.target_env.name,
            )
            .await
            .internal_err("Failed to copy environment variables")?;
            crate::db::env_vars::copy_config_env_vars_to_deployment(
                &mut *tx,
                req.source.id,
                deployment.id,
                &req.target_env.name,
            )
            .await
            .internal_err("Failed to copy rise.toml and CLI variables")?;
        }
        EnvMode::SourceSnapshot => {
            crate::db::env_vars::copy_snapshot_env_vars_to_deployment(
                &mut *tx,
                req.source.id,
                deployment.id,
            )
            .await
            .internal_err("Failed to copy environment variables")?;
        }
    }
    write_env_vars(&mut tx, deployment.id, &extension_vars).await?;
    write_env_vars(&mut tx, deployment.id, &overrides).await?;
    crate::db::env_vars::upsert_deployment_env_var(
        &mut *tx,
        deployment.id,
        "PORT",
        &req.http_port.to_string(),
        false,
        false,
        "system",
    )
    .await
    .internal_err("Failed to insert PORT env var")?;
    let system = system_vars(deployment.clone()).await?;
    write_env_vars(&mut tx, deployment.id, &system).await?;

    tx.commit()
        .await
        .internal_err("Failed to commit deployment")?;

    info!(
        "Created deployment {} ({}) from {} into '{}'",
        deployment.deployment_id, req.created_via, req.source.deployment_id, req.target_env.name
    );

    Ok(Some(deployment))
}

// ---------------------------------------------------------------------------
// Policy evaluation and the plan (preview)
// ---------------------------------------------------------------------------

/// The gate and barrier decisions for one create, as both the create handler
/// and [`plan`] see them.
#[derive(Debug, Clone)]
pub struct PolicyEvaluation {
    pub gate: GateDecision,
    /// The target's promotion source, by name.
    pub promotion_source_environment: Option<String>,
    pub candidate_version: Option<i64>,
    pub high_water: Option<i64>,
    pub barriers: BarrierOutcome,
}

/// Evaluate the promotion-source gate and the migration barriers for a
/// create into `target_env`, from `source` (or a fresh build declaring
/// `fresh_version`). Database only.
pub async fn evaluate_policy(
    pool: &sqlx::PgPool,
    project_id: Uuid,
    source: Option<&Deployment>,
    fresh_version: Option<i64>,
    target_env: &Environment,
    admin_unscoped: bool,
) -> Result<PolicyEvaluation, ServerError> {
    let promotion_source_environment = match target_env.promotion_source_environment_id {
        Some(id) => crate::db::environments::find_by_id(pool, id)
            .await
            .internal_err("Failed to look up the promotion source environment")?
            .map(|e| e.name),
        None => None,
    };
    let gate = gate(
        target_env,
        source.map(|s| s.environment_id),
        promotion_source_environment.as_deref(),
        admin_unscoped,
    );
    let candidate_version = match source {
        Some(source) => source.migration_version,
        None => fresh_version,
    };
    let high_water = db_deployments::migration_high_water(pool, target_env.id)
        .await
        .internal_err("Failed to read the environment's migration version")?;
    let barriers = check_barriers_against(pool, project_id, candidate_version, high_water).await?;
    Ok(PolicyEvaluation {
        gate,
        promotion_source_environment,
        candidate_version,
        high_water,
        barriers,
    })
}

/// Who asks for a plan.
#[derive(Debug, Clone, Copy)]
pub struct Caller {
    /// An unscoped, human administrator session (may bypass the gate).
    pub admin_unscoped: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanSource {
    pub deployment_id: String,
    pub environment: String,
    pub status: String,
    pub image: Option<String>,
    pub image_digest: Option<String>,
    pub migration_version: Option<i64>,
    pub git_revision: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanCurrent {
    pub deployment_id: String,
    pub status: String,
    pub image: Option<String>,
    pub image_digest: Option<String>,
    pub migration_version: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanTarget {
    pub environment: String,
    pub group: String,
    pub current: Option<PlanCurrent>,
    pub migration_high_water: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanGate {
    pub decision: &'static str,
    pub promotion_source_environment: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanMigration {
    pub status: &'static str,
    pub candidate_version: Option<i64>,
    pub environment_version: Option<i64>,
    pub barriers: Vec<BarrierView>,
}

/// What creating a deployment from `source` into a target would do.
#[derive(Debug, Clone, Serialize)]
pub struct CreateFromPlan {
    /// `redeploy` or `promote`.
    pub kind: CreatedVia,
    pub source: PlanSource,
    pub target: PlanTarget,
    pub pinned_image: Option<String>,
    pub gate: PlanGate,
    pub migration: PlanMigration,
    pub adjustments: Vec<ResourceAdjustment>,
    pub use_source_env_vars_allowed: bool,
    pub warnings: Vec<String>,
}

/// Preview creating a deployment from `source` into `target_env`/`group`,
/// from the same decisions [`create_from_source`]'s callers make.
#[allow(clippy::too_many_arguments)]
pub async fn plan(
    state: &AppState,
    project: &Project,
    source: &Deployment,
    source_env: &Environment,
    target_env: &Environment,
    group: &str,
    caller: Caller,
    use_source_env_vars: bool,
) -> Result<CreateFromPlan, ServerError> {
    let kind = classify_created_via(source.environment_id, target_env.id);
    let policy = evaluate_policy(
        &state.db_pool,
        project.id,
        Some(source),
        None,
        target_env,
        caller.admin_unscoped,
    )
    .await?;

    let mut warnings = Vec::new();
    match &policy.gate {
        GateDecision::AdminBypass { required } => {
            warnings.push(gate_bypass_warning(&target_env.name, required))
        }
        GateDecision::Denied { .. } | GateDecision::Allowed => {}
    }
    if let BarrierOutcome::Unknown { has_barriers: true } = policy.barriers {
        warnings.push(barrier_unknown_warning(&target_env.name));
    }
    let use_source_env_vars_allowed = kind == CreatedVia::Redeploy;
    if use_source_env_vars && !use_source_env_vars_allowed {
        warnings.push(
            "use_source_env_vars is not allowed across environments: a promotion recomputes \
             variables for the target"
                .to_string(),
        );
    }
    if !state_machine_can_create_from(source) {
        warnings.push(format!(
            "Deployment '{}' cannot be used as a source: its image is not available (status '{}')",
            source.deployment_id, source.status
        ));
    }

    let mut adjustments = Vec::new();
    let mut pinned_image = None;
    if kind == CreatedVia::Promote {
        let constraints =
            EffectiveConstraints::resolve(state.deployment_constraints.as_ref(), target_env);
        match decode_source_specs(source).and_then(|specs| {
            clamp_inherited_resources(source, specs, &ExplicitResources::default(), &constraints)
        }) {
            Ok(clamped) => adjustments = clamped.adjustments,
            Err(e) => warnings.push(e.message),
        }
        match pin_images(state, project, source, source.containers.as_ref()).await {
            Ok(pinned) => pinned_image = pinned.pinned_image,
            Err(e) => warnings.push(e.message),
        }
    }

    let provenance = crate::db::deployment_events::provenance_batch(&state.db_pool, &[source.id])
        .await
        .map_err(|e| {
            error!("Failed to read deployment provenance: {:?}", e);
            e
        })
        .ok()
        .and_then(|mut map| map.remove(&source.id))
        .unwrap_or_default();
    let source_image = display_image(state, source, project).await;

    let current =
        db_deployments::find_active_deployment_for_group(&state.db_pool, project.id, group)
            .await
            .internal_err("Failed to look up the target's active deployment")?;
    let current = match current {
        Some(d) => Some(PlanCurrent {
            image: display_image(state, &d, project).await,
            deployment_id: d.deployment_id,
            status: d.status.to_string(),
            image_digest: d.image_digest,
            migration_version: d.migration_version,
        }),
        None => None,
    };

    let (environment_version, barrier_views) = match &policy.barriers {
        BarrierOutcome::Crossed {
            environment,
            barriers,
            ..
        } => (
            Some(*environment),
            barriers.iter().map(BarrierView::from).collect(),
        ),
        _ => (policy.high_water, Vec::new()),
    };

    Ok(CreateFromPlan {
        kind,
        source: PlanSource {
            deployment_id: source.deployment_id.clone(),
            environment: source_env.name.clone(),
            status: source.status.to_string(),
            image: source_image,
            image_digest: source.image_digest.clone(),
            migration_version: source.migration_version,
            git_revision: provenance.git_revision,
        },
        target: PlanTarget {
            environment: target_env.name.clone(),
            group: group.to_string(),
            current,
            migration_high_water: policy.high_water,
        },
        pinned_image,
        gate: PlanGate {
            decision: policy.gate.as_str(),
            promotion_source_environment: policy.promotion_source_environment.clone(),
        },
        migration: PlanMigration {
            status: policy.barriers.as_str(),
            candidate_version: policy.candidate_version,
            environment_version,
            barriers: barrier_views,
        },
        adjustments,
        use_source_env_vars_allowed,
        warnings,
    })
}

fn state_machine_can_create_from(source: &Deployment) -> bool {
    super::state_machine::can_create_from(source)
}

/// The image a deployment runs, for display: `None` for multi-container.
async fn display_image(state: &AppState, d: &Deployment, project: &Project) -> Option<String> {
    if d.image.is_some() {
        return d.image.clone();
    }
    if d.containers.is_some() {
        return None;
    }
    Some(super::utils::get_deployment_image_tag(state, d, project).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::ProjectStatus;
    use crate::db::{deployments, environments, projects, users};
    use sqlx::PgPool;

    fn env(id: Uuid, name: &str, source: Option<Uuid>) -> Environment {
        Environment {
            id,
            project_id: Uuid::nil(),
            name: name.to_string(),
            primary_deployment_group: Some(name.to_string()),
            is_production: false,
            color: "green".to_string(),
            min_replicas: None,
            max_replicas: None,
            min_cpu: None,
            max_cpu: None,
            min_memory: None,
            max_memory: None,
            max_deployment_expiration: None,
            promotion_source_environment_id: source,
            rollback_guard_enabled: false,
            rollback_guard_unhealthy_timeout_seconds: 300,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn gate_table() {
        let staging = Uuid::new_v4();
        let production = Uuid::new_v4();
        let dev = Uuid::new_v4();
        let open = env(production, "production", None);
        let gated = env(production, "production", Some(staging));
        let denied = GateDecision::Denied {
            required: "staging".to_string(),
        };

        // No policy: anything goes, fresh builds included.
        assert_eq!(gate(&open, None, None, false), GateDecision::Allowed);
        assert_eq!(gate(&open, Some(dev), None, false), GateDecision::Allowed);
        // From the promotion source.
        assert_eq!(
            gate(&gated, Some(staging), Some("staging"), false),
            GateDecision::Allowed
        );
        // Same environment: a redeploy or rollback of something admitted.
        assert_eq!(
            gate(&gated, Some(production), Some("staging"), false),
            GateDecision::Allowed
        );
        // Another environment, and a fresh build.
        assert_eq!(gate(&gated, Some(dev), Some("staging"), false), denied);
        assert_eq!(gate(&gated, None, Some("staging"), false), denied);
        // An unscoped human admin bypasses; a service account or scoped
        // session reaches the gate with `admin_unscoped = false`.
        assert_eq!(
            gate(&gated, None, Some("staging"), true),
            GateDecision::AdminBypass {
                required: "staging".to_string()
            }
        );
    }

    #[tokio::test]
    async fn gate_denial_carries_code_and_details() {
        use axum::response::IntoResponse;
        let response = gate_denied_error("production", "staging").into_response();
        assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error_code"], "promotion_source_required");
        assert_eq!(body["details"]["environment"], "production");
        assert_eq!(body["details"]["promotion_source_environment"], "staging");
        assert!(body["error"]
            .as_str()
            .unwrap()
            .contains("rise promote <deployment-id> -E production"));
    }

    #[test]
    fn created_via_classification() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        assert_eq!(classify_created_via(a, a), CreatedVia::Redeploy);
        assert_eq!(classify_created_via(a, b), CreatedVia::Promote);
    }

    #[test]
    fn barrier_range_table() {
        // Forward and equal moves.
        assert_eq!(barrier_range(Some(5), Some(4)), BarrierRange::Ok);
        assert_eq!(barrier_range(Some(4), Some(4)), BarrierRange::Ok);
        // Nothing in the environment declared a version.
        assert_eq!(barrier_range(Some(1), None), BarrierRange::Ok);
        assert_eq!(barrier_range(None, None), BarrierRange::Ok);
        // Undeclared candidate.
        assert_eq!(barrier_range(None, Some(3)), BarrierRange::Unknown);
        // Backward: the ledger decides.
        assert_eq!(
            barrier_range(Some(2), Some(5)),
            BarrierRange::Check {
                low_exclusive: 2,
                high_inclusive: 5
            }
        );
    }

    fn deployment(replicas: i32, cpu: &str, memory: &str) -> Deployment {
        Deployment {
            id: Uuid::new_v4(),
            deployment_id: "20261020-101500".to_string(),
            project_id: Uuid::nil(),
            created_by_id: Uuid::nil(),
            status: DeploymentStatus::Healthy,
            deployment_group: "default".to_string(),
            environment_id: Uuid::nil(),
            expires_at: None,
            termination_reason: None,
            completed_at: None,
            error_message: None,
            build_logs: None,
            controller_metadata: serde_json::json!({}),
            image: None,
            image_digest: None,
            rolled_back_from_deployment_id: None,
            http_port: 8080,
            needs_reconcile: false,
            is_active: true,
            deploying_started_at: None,
            first_healthy_at: None,
            job_url: None,
            pull_request_url: None,
            git_repository_url: None,
            replicas,
            cpu: cpu.to_string(),
            memory: memory.to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            identity_credential_hash: None,
            identity_audiences: serde_json::json!({}),
            containers: None,
            routes: None,
            created_via: CreatedVia::Deploy,
            promoted_from_deployment_id: None,
            promoted_from_environment_id: None,
            migration_version: None,
            unhealthy_since: None,
            rollback_guard_for_deployment_id: None,
        }
    }

    fn constraints() -> EffectiveConstraints {
        EffectiveConstraints {
            environment: "production".to_string(),
            min_replicas: 2,
            max_replicas: 4,
            min_cpu: "250m".to_string(),
            max_cpu: "1".to_string(),
            min_memory: "128Mi".to_string(),
            max_memory: "1Gi".to_string(),
        }
    }

    #[test]
    fn single_container_inherited_values_are_clamped_and_reported() {
        let source = deployment(1, "2", "512Mi");
        let clamped =
            clamp_inherited_resources(&source, None, &ExplicitResources::default(), &constraints())
                .unwrap();
        assert_eq!(clamped.replicas, 2);
        assert_eq!(clamped.cpu, "1");
        assert_eq!(clamped.memory, "512Mi");
        assert!(clamped.containers.is_none());
        assert_eq!(
            clamped
                .adjustments
                .iter()
                .map(|a| (
                    a.container.as_str(),
                    a.field,
                    a.from.as_str(),
                    a.to.as_str()
                ))
                .collect::<Vec<_>>(),
            vec![("app", "replicas", "1", "2"), ("app", "cpu", "2", "1")]
        );
        assert_eq!(clamped.adjustments[0].reason, "production requires ≥ 2");
    }

    #[test]
    fn explicit_values_are_not_clamped() {
        let source = deployment(1, "2", "512Mi");
        let clamped = clamp_inherited_resources(
            &source,
            None,
            &ExplicitResources {
                replicas: Some(9),
                cpu: Some("4".to_string()),
                memory: None,
            },
            &constraints(),
        )
        .unwrap();
        assert_eq!(clamped.replicas, 9);
        assert_eq!(clamped.cpu, "4");
        assert!(clamped.adjustments.is_empty());
    }

    fn spec(name: &str, replicas: u32, cpu: &str, memory: &str) -> ContainerSpec {
        ContainerSpec {
            name: name.to_string(),
            image: Some(format!("reg/app:{name}")),
            port: None,
            replicas: Some(replicas),
            cpu: Some(cpu.to_string()),
            memory: Some(memory.to_string()),
            env_overrides: vec![],
            health_check: None,
        }
    }

    #[test]
    fn multi_container_clamps_per_container_cpu_and_memory() {
        let source = deployment(1, "500m", "256Mi");
        let clamped = clamp_inherited_resources(
            &source,
            Some(vec![
                spec("web", 2, "100m", "2Gi"),
                spec("worker", 1, "500m", "256Mi"),
            ]),
            &ExplicitResources::default(),
            &constraints(),
        )
        .unwrap();
        let specs = clamped.containers.unwrap();
        assert_eq!(specs[0].cpu.as_deref(), Some("250m"));
        assert_eq!(specs[0].memory.as_deref(), Some("1Gi"));
        assert_eq!(specs[1].cpu.as_deref(), Some("500m"));
        assert_eq!(
            clamped
                .adjustments
                .iter()
                .map(|a| (a.container.as_str(), a.field))
                .collect::<Vec<_>>(),
            vec![("web", "cpu"), ("web", "memory")]
        );
    }

    #[test]
    fn multi_container_total_replicas_out_of_range_is_an_error() {
        let source = deployment(1, "500m", "256Mi");
        let err = clamp_inherited_resources(
            &source,
            Some(vec![
                spec("web", 3, "500m", "256Mi"),
                spec("worker", 3, "500m", "256Mi"),
            ]),
            &ExplicitResources::default(),
            &constraints(),
        )
        .unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
        assert!(
            err.message.contains("6 replicas in total"),
            "{}",
            err.message
        );
    }

    #[test]
    fn toml_override_scoped_to_an_environment_is_stored_as_toml_env() {
        let mut o = EnvOverride {
            key: "LOG_LEVEL".to_string(),
            value: "info".to_string(),
            is_secret: false,
            is_protected: None,
            source: Some("toml".to_string()),
            for_environment: Some("production".to_string()),
        };
        assert_eq!(
            override_source(&o),
            EnvVarSource::TomlEnv("production".to_string())
        );
        o.for_environment = None;
        assert_eq!(override_source(&o), EnvVarSource::Toml);
        o.source = Some("cli".to_string());
        o.for_environment = Some("production".to_string());
        assert_eq!(override_source(&o), EnvVarSource::Cli);
        // Server-managed sources are never accepted from a client.
        o.source = Some("toml:production".to_string());
        assert_eq!(override_source(&o), EnvVarSource::Cli);
        o.source = Some("system".to_string());
        assert_eq!(override_source(&o), EnvVarSource::Cli);
    }

    async fn seed(pool: &PgPool) -> (Uuid, Uuid, Environment, Environment) {
        let user = users::create(pool, "promotion@example.com").await.unwrap();
        let project = projects::create(
            pool,
            "promotion-project",
            ProjectStatus::Stopped,
            "public".to_string(),
            Some(user.id),
            None,
            None,
        )
        .await
        .unwrap();
        let production_id = environments::test_environment_id(pool, project.id).await;
        let staging = environments::create(
            pool,
            project.id,
            "staging",
            Some("staging"),
            false,
            "blue",
            None,
        )
        .await
        .unwrap();
        let production = environments::update_promotion_policy(
            pool,
            production_id,
            Some(Some(staging.id)),
            None,
            None,
        )
        .await
        .unwrap();
        (project.id, user.id, staging, production)
    }

    async fn seed_deployment(
        pool: &PgPool,
        project_id: Uuid,
        user_id: Uuid,
        env: &Environment,
        deployment_id: &str,
        migration_version: Option<i64>,
        started: bool,
    ) -> Deployment {
        let group = env.primary_deployment_group.clone().unwrap();
        let d = deployments::create(
            pool,
            deployments::CreateDeploymentParams {
                deployment_id,
                project_id,
                created_by_id: user_id,
                status: DeploymentStatus::Pushed,
                image: Some("nginx"),
                image_digest: Some("docker.io/library/nginx@sha256:abc"),
                rolled_back_from_deployment_id: None,
                deployment_group: &group,
                environment_id: env.id,
                expires_at: None,
                http_port: 80,
                is_active: false,
                job_url: None,
                pull_request_url: None,
                git_repository_url: None,
                replicas: 1,
                cpu: "500m",
                memory: "256Mi",
                identity_audiences: serde_json::json!({}),
                containers: None,
                routes: None,
                expiration_cap: None,
                created_via: CreatedVia::Deploy,
                promoted_from_deployment_id: None,
                promoted_from_environment_id: None,
                migration_version,
                rollback_guard_for_deployment_id: None,
                extra_event_attributes: None,
            },
        )
        .await
        .unwrap();
        if started {
            deployments::update_status(pool, d.id, DeploymentStatus::Deploying, None)
                .await
                .unwrap()
        } else {
            d
        }
    }

    #[sqlx::test]
    async fn check_barriers_table(pool: PgPool) {
        let (project_id, user_id, _staging, production) = seed(&pool).await;
        // Nothing has run in production yet.
        assert_eq!(
            check_barriers(&pool, project_id, production.id, Some(1))
                .await
                .unwrap(),
            BarrierOutcome::Ok
        );

        seed_deployment(
            &pool,
            project_id,
            user_id,
            &production,
            "d-5",
            Some(5),
            true,
        )
        .await;
        // A version that never started does not raise the high-water mark.
        seed_deployment(
            &pool,
            project_id,
            user_id,
            &production,
            "d-9",
            Some(9),
            false,
        )
        .await;
        migration_barriers::create(&pool, project_id, 4, "drops column", None)
            .await
            .unwrap();

        let outcome = |c| check_barriers(&pool, project_id, production.id, c);
        // Forward and equal.
        assert_eq!(outcome(Some(6)).await.unwrap(), BarrierOutcome::Ok);
        assert_eq!(outcome(Some(5)).await.unwrap(), BarrierOutcome::Ok);
        // Backward, but not across a barrier (4 < 4 is false: (4, 5]).
        assert_eq!(outcome(Some(4)).await.unwrap(), BarrierOutcome::Ok);
        // Backward across barrier 4.
        match outcome(Some(3)).await.unwrap() {
            BarrierOutcome::Crossed {
                candidate,
                environment,
                barriers,
            } => {
                assert_eq!((candidate, environment), (3, 5));
                assert_eq!(barriers.iter().map(|b| b.version).collect::<Vec<_>>(), [4]);
            }
            other => panic!("expected Crossed, got {other:?}"),
        }
        // Undeclared.
        assert_eq!(
            outcome(None).await.unwrap(),
            BarrierOutcome::Unknown { has_barriers: true }
        );
    }

    #[sqlx::test]
    async fn evaluate_policy_gates_and_checks_barriers(pool: PgPool) {
        let (project_id, user_id, staging, production) = seed(&pool).await;
        seed_deployment(
            &pool,
            project_id,
            user_id,
            &production,
            "d-prod",
            Some(5),
            true,
        )
        .await;
        migration_barriers::create(&pool, project_id, 5, "renames table", None)
            .await
            .unwrap();
        let from_staging =
            seed_deployment(&pool, project_id, user_id, &staging, "d-stg", Some(4), true).await;

        let promoted = evaluate_policy(
            &pool,
            project_id,
            Some(&from_staging),
            None,
            &production,
            false,
        )
        .await
        .unwrap();
        assert_eq!(promoted.gate, GateDecision::Allowed);
        assert_eq!(
            promoted.promotion_source_environment.as_deref(),
            Some("staging")
        );
        assert_eq!(promoted.candidate_version, Some(4));
        assert_eq!(promoted.high_water, Some(5));
        assert!(matches!(promoted.barriers, BarrierOutcome::Crossed { .. }));

        // A fresh build is denied; its own declared version is the candidate.
        let fresh = evaluate_policy(&pool, project_id, None, Some(6), &production, false)
            .await
            .unwrap();
        assert_eq!(
            fresh.gate,
            GateDecision::Denied {
                required: "staging".to_string()
            }
        );
        assert_eq!(fresh.barriers, BarrierOutcome::Ok);

        let bypass = evaluate_policy(&pool, project_id, None, Some(6), &production, true)
            .await
            .unwrap();
        assert!(matches!(bypass.gate, GateDecision::AdminBypass { .. }));

        // Staging has no policy.
        let staging_fresh = evaluate_policy(&pool, project_id, None, None, &staging, false)
            .await
            .unwrap();
        assert_eq!(staging_fresh.gate, GateDecision::Allowed);
    }

    fn request<'a>(
        project: &'a Project,
        source: &'a Deployment,
        target_env: &'a Environment,
        deployment_id: &'a str,
        created_via: CreatedVia,
        env_mode: EnvMode,
    ) -> CreateFromSource<'a> {
        CreateFromSource {
            project,
            source,
            target_env,
            group: target_env.primary_deployment_group.as_deref().unwrap(),
            deployment_id,
            created_by_id: source.created_by_id,
            created_via,
            env_mode,
            overrides: &[],
            http_port: 9090,
            replicas: source.replicas as u32,
            cpu: source.cpu.clone(),
            memory: source.memory.clone(),
            containers: None,
            routes: None,
            identity_audiences: serde_json::json!({}),
            image: source.image.clone(),
            image_digest: source.image_digest.clone(),
            expires_at: None,
            expiration_cap: None,
            job_url: None,
            pull_request_url: None,
            git_repository_url: None,
            rollback_guard_for: None,
            guard_lock: None,
            extra_event_attributes: serde_json::json!({}),
        }
    }

    fn var(key: &str, value: &str, source: &str) -> PreparedEnvVar {
        PreparedEnvVar {
            key: key.to_string(),
            value: value.to_string(),
            is_secret: false,
            is_protected: false,
            source: source.to_string(),
        }
    }

    async fn vars_of(pool: &PgPool, deployment_id: Uuid) -> Vec<(String, String, String)> {
        crate::db::env_vars::list_deployment_env_vars(pool, deployment_id)
            .await
            .unwrap()
            .into_iter()
            .map(|v| (v.key, v.value, v.source))
            .collect()
    }

    async fn seed_source_vars(pool: &PgPool, source: Uuid) {
        for (key, value, src) in [
            ("FROM_TOML", "t", "toml"),
            ("STAGING_ONLY", "s", "toml:staging"),
            ("PROD_TOML", "p", "toml:production"),
            ("FROM_CLI", "c", "cli"),
            ("GLOBAL_VAR", "old", "global"),
            ("EXT_VAR", "stale", "extension"),
            ("PORT", "80", "system"),
        ] {
            crate::db::env_vars::upsert_deployment_env_var(
                pool, source, key, value, false, false, src,
            )
            .await
            .unwrap();
        }
    }

    #[sqlx::test]
    async fn promotion_recomputes_variables_for_the_target_in_one_transaction(pool: PgPool) {
        let (project_id, user_id, staging, production) = seed(&pool).await;
        let project = projects::find_by_id(&pool, project_id)
            .await
            .unwrap()
            .unwrap();
        let source =
            seed_deployment(&pool, project_id, user_id, &staging, "d-stg", Some(7), true).await;
        seed_source_vars(&pool, source.id).await;
        crate::db::env_vars::upsert_project_env_var(
            &pool,
            project_id,
            "GLOBAL_VAR",
            "new",
            false,
            false,
            None,
        )
        .await
        .unwrap();
        crate::db::env_vars::upsert_project_env_var(
            &pool,
            project_id,
            "PROD_ONLY",
            "yes",
            false,
            false,
            Some(production.id),
        )
        .await
        .unwrap();

        let created = create_from_source_in(
            &pool,
            request(
                &project,
                &source,
                &production,
                "d-prod",
                CreatedVia::Promote,
                EnvMode::ProjectCurrent,
            ),
            vec![var("EXT_VAR", "fresh", "extension")],
            vec![var("FROM_CLI", "override", "cli")],
            |_| async { Ok(vec![var("RISE_ENVIRONMENT", "production", "system")]) },
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(created.status, DeploymentStatus::Pushed);
        assert_eq!(created.created_via, CreatedVia::Promote);
        assert_eq!(created.promoted_from_deployment_id, Some(source.id));
        assert_eq!(created.promoted_from_environment_id, Some(staging.id));
        assert_eq!(created.rolled_back_from_deployment_id, Some(source.id));
        assert_eq!(created.migration_version, Some(7));
        assert_eq!(created.environment_id, production.id);

        let vars = vars_of(&pool, created.id).await;
        let get = |k: &str| {
            vars.iter()
                .find(|(key, _, _)| key == k)
                .map(|(_, v, s)| (v.as_str(), s.as_str()))
        };
        assert_eq!(get("GLOBAL_VAR"), Some(("new", "global")));
        assert_eq!(get("PROD_ONLY"), Some(("yes", "env:production")));
        assert_eq!(get("FROM_TOML"), Some(("t", "toml")));
        assert_eq!(get("PROD_TOML"), Some(("p", "toml:production")));
        assert_eq!(get("STAGING_ONLY"), None);
        assert_eq!(get("FROM_CLI"), Some(("override", "cli")));
        assert_eq!(get("EXT_VAR"), Some(("fresh", "extension")));
        assert_eq!(get("PORT"), Some(("9090", "system")));
        assert_eq!(get("RISE_ENVIRONMENT"), Some(("production", "system")));
    }

    #[sqlx::test]
    async fn source_snapshot_skips_system_and_extension_variables(pool: PgPool) {
        let (project_id, user_id, staging, _production) = seed(&pool).await;
        let project = projects::find_by_id(&pool, project_id)
            .await
            .unwrap()
            .unwrap();
        let source =
            seed_deployment(&pool, project_id, user_id, &staging, "d-stg", None, true).await;
        seed_source_vars(&pool, source.id).await;

        let created = create_from_source_in(
            &pool,
            request(
                &project,
                &source,
                &staging,
                "d-stg-2",
                CreatedVia::Redeploy,
                EnvMode::SourceSnapshot,
            ),
            vec![var("EXT_VAR", "fresh", "extension")],
            vec![],
            |_| async { Ok(vec![]) },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(created.created_via, CreatedVia::Redeploy);
        assert_eq!(created.promoted_from_deployment_id, None);

        let vars = vars_of(&pool, created.id).await;
        let get = |k: &str| {
            vars.iter()
                .find(|(key, _, _)| key == k)
                .map(|(_, v, s)| (v.as_str(), s.as_str()))
        };
        assert_eq!(get("GLOBAL_VAR"), Some(("old", "global")));
        assert_eq!(get("STAGING_ONLY"), Some(("s", "toml:staging")));
        assert_eq!(get("EXT_VAR"), Some(("fresh", "extension")));
        assert_eq!(get("PORT"), Some(("9090", "system")));
    }

    #[sqlx::test]
    async fn a_failure_after_the_insert_leaves_no_deployment(pool: PgPool) {
        let (project_id, user_id, staging, _production) = seed(&pool).await;
        let project = projects::find_by_id(&pool, project_id)
            .await
            .unwrap()
            .unwrap();
        let source =
            seed_deployment(&pool, project_id, user_id, &staging, "d-stg", None, true).await;

        let err = create_from_source_in(
            &pool,
            request(
                &project,
                &source,
                &staging,
                "d-stg-2",
                CreatedVia::Redeploy,
                EnvMode::ProjectCurrent,
            ),
            vec![],
            vec![],
            |_| async { Err(ServerError::internal("URLs unavailable")) },
        )
        .await
        .unwrap_err();
        assert_eq!(err.message, "URLs unavailable");
        assert!(
            deployments::find_by_project_and_deployment_id(&pool, project_id, "d-stg-2")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test]
    async fn guard_lock_on_a_subject_that_does_not_qualify_creates_nothing(pool: PgPool) {
        let (project_id, user_id, staging, _production) = seed(&pool).await;
        let project = projects::find_by_id(&pool, project_id)
            .await
            .unwrap()
            .unwrap();
        let last_known_good =
            seed_deployment(&pool, project_id, user_id, &staging, "d-lkg", None, true).await;
        // Not Unhealthy: the lock's re-check fails.
        let subject =
            seed_deployment(&pool, project_id, user_id, &staging, "d-sick", None, true).await;

        let mut req = request(
            &project,
            &last_known_good,
            &staging,
            "d-guard",
            CreatedVia::RollbackGuard,
            EnvMode::SourceSnapshot,
        );
        req.rollback_guard_for = Some(subject.id);
        req.guard_lock = Some(60);
        let created = create_from_source_in(&pool, req, vec![], vec![], |_| async { Ok(vec![]) })
            .await
            .unwrap();
        assert!(created.is_none());
        assert!(
            deployments::find_by_project_and_deployment_id(&pool, project_id, "d-guard")
                .await
                .unwrap()
                .is_none()
        );
    }
}
