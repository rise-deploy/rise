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
    /// optional port (defaults to the container's `port`).
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

    /// Port the container listens on. Required if the container should be
    /// reachable via the ingress (referenced from `[routes]`) or by sibling
    /// containers via `RISE_CONTAINER_HOST__*`. Need not be HTTP — the Service
    /// is plain TCP. Omit for workers.
    pub port: Option<u16>,

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

    /// Health check configuration. HTTP liveness+readiness probes default to ON
    /// (path `/`) only for containers reachable via `[routes]`; non-routed
    /// containers (e.g. a database) get no probe unless one is configured here.
    /// Set `health_check = false` to disable probes entirely; set a config block
    /// to force them on with custom settings. Requires `port` to be set.
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
    /// Target container name (must exist in `[containers]` and have `port` set).
    pub container: String,
    /// Override the container's `port` for this route (rare; usually omit).
    pub port: Option<u16>,
}

/// Name of the implicit container the backend synthesises for a single-container
/// deployment (one with top-level `[build]`/`[deploy]` and no `[containers]`).
/// A single-container app is just the one-container case — not a "legacy" shape.
// Only referenced by the backend reconciler/handlers; allow dead-code in CLI-only builds.
#[cfg_attr(not(feature = "backend"), allow(dead_code))]
pub const DEFAULT_CONTAINER_NAME: &str = "app";

/// A container resolved from an explicit `[containers.X]` entry in rise.toml.
#[derive(Debug, Clone)]
pub struct ResolvedContainer {
    pub name: String,
    pub image: Option<String>,
    pub build: Option<BuildConfig>,
    pub port: Option<u16>,
    pub replicas: Option<u32>,
    pub cpu: Option<String>,
    pub memory: Option<String>,
    pub env: BTreeMap<String, String>,
    pub health_check: Option<HealthCheckSetting>,
}

/// One ingress route resolved from rise.toml. Includes a port (defaulting to
/// the target container's `port`) so the reconciler doesn't need to chase
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
    /// Resolve the explicit `[containers]` + `[routes]` from rise.toml.
    ///
    /// Returns an empty resolution when there is no `[containers]` block — a
    /// single-container project (top-level `[build]`/`[deploy]`, or `--image`)
    /// is driven by the CLI's single-container flow, and the backend synthesises
    /// its implicit `app` container at reconcile time.
    ///
    /// Returns `Err` when a route references a container that has no `port`
    /// (and the route itself doesn't override `port`). The CLI's
    /// `load_full_project_config` validator catches this for the on-disk path,
    /// but this method is public and callers may construct `ProjectBuildConfig`
    /// in-memory or otherwise bypass the validator — so we enforce it inline to
    /// avoid silently wiring a K8s Service to port 0.
    pub fn resolve_deploy(&self) -> Result<ResolvedDeploy, String> {
        if self.containers.is_empty() {
            return Ok(ResolvedDeploy::default());
        }

        let containers: Vec<ResolvedContainer> = self
            .containers
            .iter()
            .map(|(name, c)| ResolvedContainer {
                name: name.clone(),
                image: c.image.clone(),
                build: c.build.clone(),
                port: c.port,
                replicas: c.replicas,
                cpu: c.cpu.clone(),
                memory: c.memory.clone(),
                env: c.env.clone(),
                health_check: c.health_check.clone(),
            })
            .collect();

        let routes = if self.routes.is_empty() {
            // No explicit routes: if exactly one container has a port, expose it
            // at `/`. Otherwise emit no routes (workers-only or ambiguous —
            // validation should have caught the latter).
            let routable: Vec<&ResolvedContainer> =
                containers.iter().filter(|c| c.port.is_some()).collect();
            if routable.len() == 1 {
                vec![ResolvedRoute {
                    path: "/".to_string(),
                    container: routable[0].name.clone(),
                    port: routable[0].port.expect("filtered for Some"),
                }]
            } else {
                Vec::new()
            }
        } else {
            let mut resolved = Vec::with_capacity(self.routes.len());
            for (path, route) in &self.routes {
                let port = match route
                    .port
                    .or_else(|| self.containers.get(&route.container).and_then(|c| c.port))
                {
                    Some(p) => p,
                    None => {
                        return Err(format!(
                            "route '{}' targets container '{}' which has no port set",
                            path, route.container
                        ));
                    }
                };
                resolved.push(ResolvedRoute {
                    path: path.clone(),
                    container: route.container.clone(),
                    port,
                });
            }
            resolved
        };

        Ok(ResolvedDeploy { containers, routes })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_deploy_errors_when_route_targets_container_without_http_port() {
        // Construct in-memory (bypassing load_full_project_config's validator)
        // to confirm resolve_deploy itself rejects port-less route targets
        // rather than silently producing ResolvedRoute { port: 0 }.
        let mut config = ProjectBuildConfig::default();
        config.containers.insert(
            "worker".to_string(),
            ContainerConfig {
                image: Some("foo:bar".to_string()),
                ..Default::default()
            },
        );
        config.routes.insert(
            "/".to_string(),
            RouteConfig {
                container: "worker".to_string(),
                port: None,
            },
        );

        let err = config
            .resolve_deploy()
            .expect_err("expected missing-port error");
        assert!(
            err.contains("no port") && err.contains("worker") && err.contains("'/'"),
            "got: {err}"
        );
    }

    #[test]
    fn resolve_deploy_uses_route_port_override_when_container_has_no_port() {
        // If the route supplies its own port, the container's missing port
        // is fine — we shouldn't error in that case.
        let mut config = ProjectBuildConfig::default();
        config.containers.insert(
            "worker".to_string(),
            ContainerConfig {
                image: Some("foo:bar".to_string()),
                ..Default::default()
            },
        );
        config.routes.insert(
            "/".to_string(),
            RouteConfig {
                container: "worker".to_string(),
                port: Some(8080),
            },
        );

        let resolved = config.resolve_deploy().expect("should succeed");
        assert_eq!(resolved.routes.len(), 1);
        assert_eq!(resolved.routes[0].port, 8080);
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
