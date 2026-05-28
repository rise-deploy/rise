//! Shared type definitions for the `rise.toml` / `.rise.toml` project configuration file.
//!
//! These types are used by both the CLI (for reading/writing config) and the backend
//! (for generating a JSON Schema endpoint).

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

/// Root structure for rise.toml / .rise.toml configuration file
#[derive(Debug, Deserialize, Serialize, Default, Clone)]
#[cfg_attr(feature = "backend", derive(schemars::JsonSchema))]
pub struct ProjectBuildConfig {
    /// Optional version (must be 1 if present)
    pub version: Option<u32>,

    /// Project metadata (optional)
    #[serde(default)]
    pub project: Option<ProjectConfig>,

    /// Build configuration (optional)
    #[serde(default)]
    pub build: Option<BuildConfig>,

    /// Deployment resource configuration (optional)
    #[serde(default)]
    pub deploy: Option<DeployConfig>,

    /// Workload identity configuration (optional)
    #[serde(default)]
    pub identity: Option<IdentityConfig>,

    /// Per-environment configuration (optional)
    #[serde(default)]
    pub environments: BTreeMap<String, EnvironmentConfig>,

    /// Multi-container configuration. When non-empty, top-level `[build]` and
    /// `[deploy]` must not be set. Each container becomes a separate K8s
    /// Deployment so replica counts scale independently.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub containers: BTreeMap<String, ContainerConfig>,

    /// Path-based ingress routing across containers. Path strings are the keys
    /// (e.g. `"/api"`, `"/"`); the value picks the target container and an
    /// optional port (defaults to the container's `http_port`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub routes: BTreeMap<String, RouteConfig>,
}

/// Configuration for one container in a multi-container deployment.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "backend", derive(schemars::JsonSchema))]
pub struct ContainerConfig {
    /// Pre-built image reference. Exclusive with `build`.
    pub image: Option<String>,

    /// Build configuration to produce this container's image. Exclusive with `image`.
    pub build: Option<BuildConfig>,

    /// HTTP port the container listens on. Required if the container should be
    /// reachable via the ingress (referenced from `[routes]`). Omit for workers.
    pub http_port: Option<u16>,

    /// Number of replicas.
    pub replicas: Option<u32>,

    /// CPU allocation (e.g. "500m", "1") — sets both request and limit.
    pub cpu: Option<String>,

    /// Memory allocation (e.g. "256Mi", "1Gi") — sets both request and limit.
    pub memory: Option<String>,

    /// Plain-text environment variables scoped to this container. Merged on top
    /// of any project-level env vars; container-scoped values win on conflict.
    #[serde(default)]
    pub env: BTreeMap<String, String>,

    /// Health check configuration. When `http_port` is set and this is omitted,
    /// HTTP liveness+readiness probes default to ON (path `/`). Set
    /// `health_check = false` to disable probes entirely.
    pub health_check: Option<HealthCheckSetting>,
}

/// `health_check` may either be `false` (probes disabled) or a config block.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(untagged)]
#[cfg_attr(feature = "backend", derive(schemars::JsonSchema))]
pub enum HealthCheckSetting {
    /// Pass `false` to disable probes entirely. (Only the literal `false` is
    /// accepted; `true` is rejected so users always go through `Config`.)
    Disabled(BoolFalse),
    /// Customised probe configuration.
    Config(HealthCheckConfig),
}

/// Newtype that only deserializes the literal boolean `false`.
#[derive(Debug, Clone, Copy)]
pub struct BoolFalse;

impl serde::Serialize for BoolFalse {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bool(false)
    }
}

impl<'de> serde::Deserialize<'de> for BoolFalse {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let b = bool::deserialize(d)?;
        if b {
            Err(serde::de::Error::custom(
                "health_check = true is not allowed; omit the key to enable defaults or set it to false to disable",
            ))
        } else {
            Ok(BoolFalse)
        }
    }
}

#[cfg(feature = "backend")]
impl schemars::JsonSchema for BoolFalse {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "BoolFalse".into()
    }
    fn schema_id() -> std::borrow::Cow<'static, str> {
        concat!(module_path!(), "::BoolFalse").into()
    }
    fn json_schema(_gen: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "boolean",
            "const": false,
        })
    }
}

/// Health-check configuration block. All fields are optional and fall back to
/// the server's `HealthProbeConfig` defaults (path `/`, 10s initial delay, …).
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "backend", derive(schemars::JsonSchema))]
pub struct HealthCheckConfig {
    /// HTTP path to probe (default `/`).
    pub path: Option<String>,
    /// Seconds after container start before the first probe.
    pub initial_delay_seconds: Option<i32>,
    /// Seconds between probes.
    pub period_seconds: Option<i32>,
    /// Probe timeout in seconds.
    pub timeout_seconds: Option<i32>,
    /// Consecutive failures before the probe is considered failed.
    pub failure_threshold: Option<i32>,
    /// Enable liveness probe (default true).
    pub liveness_enabled: Option<bool>,
    /// Enable readiness probe (default true).
    pub readiness_enabled: Option<bool>,
}

/// One ingress path → container mapping.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "backend", derive(schemars::JsonSchema))]
pub struct RouteConfig {
    /// Target container name (must exist in `[containers]` and have `http_port`).
    pub container: String,
    /// Override the container's `http_port` for this route (rare; usually omit).
    pub port: Option<u16>,
}

/// Default container name used when synthesising a single container from the
/// legacy top-level `[build]` + `[deploy]` shape.
pub const DEFAULT_CONTAINER_NAME: &str = "app";

/// A container resolved from rise.toml — either an explicit `[containers.X]`
/// entry or the implicit `app` container synthesised from top-level
/// `[build]`/`[deploy]`. Always non-empty after resolution.
#[derive(Debug, Clone)]
pub struct ResolvedContainer {
    pub name: String,
    pub image: Option<String>,
    pub build: Option<BuildConfig>,
    pub http_port: Option<u16>,
    pub replicas: Option<u32>,
    pub cpu: Option<String>,
    pub memory: Option<String>,
    pub env: BTreeMap<String, String>,
    pub health_check: Option<HealthCheckSetting>,
    /// True when this container was synthesised from the legacy single-container
    /// shape (no `[containers]` block in rise.toml). The reconciler relies on
    /// this to keep emitting unsuffixed K8s resource names for back-compat.
    pub is_legacy: bool,
}

/// One ingress route resolved from rise.toml. Includes a port (defaulting to
/// the target container's http_port) so the reconciler doesn't need to chase
/// the container list to build the K8s Service reference.
#[derive(Debug, Clone)]
pub struct ResolvedRoute {
    pub path: String,
    pub container: String,
    pub port: u16,
}

/// Outcome of `ProjectBuildConfig::resolve_containers()`.
#[derive(Debug, Clone, Default)]
pub struct ResolvedDeploy {
    pub containers: Vec<ResolvedContainer>,
    pub routes: Vec<ResolvedRoute>,
}

impl ProjectBuildConfig {
    /// Resolve the canonical list of containers + routes downstream code should
    /// operate on, normalising legacy single-container configs (no
    /// `[containers]`) into an implicit `app` container.
    ///
    /// Returns an empty resolution when neither `[containers]` nor top-level
    /// `[build]` is present — callers (CLI) must still allow `--image` to
    /// drive an image-only deployment in that case.
    pub fn resolve_deploy(&self) -> ResolvedDeploy {
        if !self.containers.is_empty() {
            let containers: Vec<ResolvedContainer> = self
                .containers
                .iter()
                .map(|(name, c)| ResolvedContainer {
                    name: name.clone(),
                    image: c.image.clone(),
                    build: c.build.clone(),
                    http_port: c.http_port,
                    replicas: c.replicas,
                    cpu: c.cpu.clone(),
                    memory: c.memory.clone(),
                    env: c.env.clone(),
                    health_check: c.health_check.clone(),
                    is_legacy: false,
                })
                .collect();

            let routes = if self.routes.is_empty() {
                // No explicit routes: if exactly one container has http_port,
                // expose it at `/`. Otherwise emit no routes (workers-only or
                // ambiguous — validation should have caught the latter).
                let routable: Vec<&ResolvedContainer> = containers
                    .iter()
                    .filter(|c| c.http_port.is_some())
                    .collect();
                if routable.len() == 1 {
                    vec![ResolvedRoute {
                        path: "/".to_string(),
                        container: routable[0].name.clone(),
                        port: routable[0].http_port.expect("filtered for Some"),
                    }]
                } else {
                    Vec::new()
                }
            } else {
                self.routes
                    .iter()
                    .map(|(path, route)| {
                        let port = route
                            .port
                            .or_else(|| {
                                self.containers
                                    .get(&route.container)
                                    .and_then(|c| c.http_port)
                            })
                            .unwrap_or(0);
                        ResolvedRoute {
                            path: path.clone(),
                            container: route.container.clone(),
                            port,
                        }
                    })
                    .collect()
            };

            return ResolvedDeploy { containers, routes };
        }

        // Legacy single-container path: synthesise an implicit `app` container
        // when top-level [build] or [deploy] is present.
        if self.build.is_some() || self.deploy.is_some() {
            let deploy = self.deploy.clone().unwrap_or_default();
            let container = ResolvedContainer {
                name: DEFAULT_CONTAINER_NAME.to_string(),
                image: None,
                build: self.build.clone(),
                http_port: None,
                replicas: deploy.replicas,
                cpu: deploy.cpu,
                memory: deploy.memory,
                env: BTreeMap::new(),
                health_check: None,
                is_legacy: true,
            };
            return ResolvedDeploy {
                containers: vec![container],
                routes: Vec::new(),
            };
        }

        ResolvedDeploy::default()
    }
}

/// Workload identity configuration
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "backend", derive(schemars::JsonSchema))]
pub struct IdentityConfig {
    /// Audiences to auto-mint workload identity tokens for and mount as files.
    /// Map key = in-pod token filename, value = the token audience.
    #[serde(default)]
    pub audiences: BTreeMap<String, String>,
}

/// Per-environment configuration
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "backend", derive(schemars::JsonSchema))]
pub struct EnvironmentConfig {
    /// Whether this is the default environment for local deployments
    #[serde(default)]
    pub default: bool,

    /// Plain-text environment variables scoped to this environment
    #[serde(default)]
    pub env: BTreeMap<String, String>,

    /// Environment-specific deployment resource overrides
    #[serde(default)]
    pub deploy: Option<DeployConfig>,
}

/// Deployment resource configuration
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "backend", derive(schemars::JsonSchema))]
pub struct DeployConfig {
    /// Number of replicas
    pub replicas: Option<u32>,

    /// CPU allocation (e.g., "500m", "1") — sets both K8s request and limit
    pub cpu: Option<String>,

    /// Memory allocation (e.g., "256Mi", "1Gi") — sets both K8s request and limit
    pub memory: Option<String>,
}

/// Project metadata configuration
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "backend", derive(schemars::JsonSchema))]
pub struct ProjectConfig {
    /// Project name
    pub name: String,

    /// Plain-text environment variables (non-secret)
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Build configuration options for a project
#[derive(Debug, Deserialize, Serialize, Default, Clone)]
#[cfg_attr(feature = "backend", derive(schemars::JsonSchema))]
pub struct BuildConfig {
    /// Build backend (docker, docker:build, docker:buildx, buildctl, pack, railpack[:buildx], railpack:buildctl)
    pub backend: Option<String>,

    /// Buildpack builder to use (only for pack backend)
    pub builder: Option<String>,

    /// Buildpack(s) to use (only for pack backend)
    pub buildpacks: Option<Vec<String>>,

    /// Build arguments to pass to the build
    /// Format: KEY=VALUE or KEY (to pass from environment)
    #[serde(alias = "env")]
    pub args: Option<Vec<String>>,

    /// Container CLI to use (docker or podman)
    pub container_cli: Option<String>,

    /// Enable managed BuildKit daemon with SSL certificate support
    pub managed_buildkit: Option<bool>,

    /// Path to Dockerfile (relative to rise.toml location). Defaults to "Dockerfile" or "Containerfile"
    pub dockerfile: Option<String>,

    /// Default build context (docker/podman only) - the context directory for the build
    /// This is the path argument to `docker build <path>`. Defaults to rise.toml location.
    /// Path is relative to the rise.toml file location.
    pub build_context: Option<String>,

    /// Build contexts (docker/podman only) - additional named contexts for multi-stage builds
    /// Format: { "name" = "path" } where path is relative to the rise.toml file location
    #[serde(default)]
    pub build_contexts: Option<HashMap<String, String>>,

    /// Disable build cache
    pub no_cache: Option<bool>,

    /// Target platform for the container image build (e.g., "linux/amd64", "linux/arm64").
    /// Defaults to linux/amd64.
    pub platform: Option<String>,
}
