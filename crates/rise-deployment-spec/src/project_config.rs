//! Deployment specification and `rise.toml` project configuration types.
//!
//! This crate owns the pure data model shared by the Rise CLI and backend. Runtime
//! behavior that depends on filesystems, registries, databases, or controllers stays
//! in the root crate.

use crate::access::AccessRequirement;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

/// Root structure for rise.toml / .rise.toml configuration file
#[derive(Debug, Deserialize, Serialize, Default, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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

    /// Multi-container configuration. When non-empty, the top-level `[build]`
    /// and `[deploy]` tables act as defaults that each container inherits per
    /// field (see `ContainerConfig`). Each container becomes a separate K8s
    /// Deployment so replica counts scale independently.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub containers: BTreeMap<String, ContainerConfig>,

    /// Path-based ingress routing across containers. Path strings are the keys
    /// (e.g. `"/api"`, `"/"`); the value picks the target container. The route's
    /// port is always the target container's `port`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub routes: BTreeMap<String, RouteConfig>,
}

/// Configuration for one container in a multi-container deployment.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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

    /// Plain-text environment variables scoped to this container. Merged on top
    /// of any project-level env vars; container-scoped values win on conflict.
    #[serde(default)]
    pub env: BTreeMap<String, String>,

    /// Deployment resource configuration (replicas, cpu, memory, health_check).
    /// Mirrors the top-level `[deploy]` table; unset fields inherit the
    /// top-level `[deploy]` defaults.
    #[serde(default)]
    pub deploy: Option<DeployConfig>,
}

/// `health_check` may either be `false` (probes disabled) or a config block.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(untagged)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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

#[cfg(feature = "schema")]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RouteConfig {
    /// Target container name (must exist in `[containers]` and have `port` set).
    /// The route's port is always the target container's `port`.
    pub container: String,

    /// Per-route override of the ingress auth requirement. Overrides *only* the
    /// `access_requirement` of the project's access class for this route's path
    /// (the access class's `ingress_class` and `custom_annotations` are per-host
    /// and always apply project-wide). When absent, the route inherits the
    /// project's requirement. Lets a single route be opened (`None`) or tightened
    /// (`Member`) relative to the rest of the project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<AccessRequirement>,
}

/// Name of the implicit container the backend synthesises for a single-container
/// deployment (one with top-level `[build]`/`[deploy]` and no `[containers]`).
/// A single-container app is just the one-container case — not a "legacy" shape.
// Only referenced by the backend reconciler/handlers; allow dead-code in CLI-only builds.
#[allow(dead_code)]
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

/// One ingress route resolved from rise.toml. A route maps a path to a target
/// container; the effective port is always the target container's `port`.
#[derive(Debug, Clone)]
pub struct ResolvedRoute {
    pub path: String,
    pub container: String,
    /// Per-route ingress auth requirement override (see [`RouteConfig::access`]).
    pub access: Option<AccessRequirement>,
}

/// Outcome of `ProjectBuildConfig::resolve_containers()`.
#[derive(Debug, Clone, Default)]
pub struct ResolvedDeploy {
    pub containers: Vec<ResolvedContainer>,
    pub routes: Vec<ResolvedRoute>,
}

/// Inputs for synthesising the implicit `app` container used by single-container
/// projects at local-runtime/reconcile time.
#[derive(Debug, Clone, Default)]
pub struct ImplicitAppContainer {
    pub image: Option<String>,
    pub build: Option<BuildConfig>,
    pub port: u16,
    pub replicas: Option<u32>,
    pub cpu: Option<String>,
    pub memory: Option<String>,
    pub env: BTreeMap<String, String>,
    pub health_check: Option<HealthCheckSetting>,
}

impl ResolvedDeploy {
    /// Synthesize the runtime container layout for a single-container project.
    ///
    /// Single-container deployments are represented as one implicit `app`
    /// container routed at `/`. Callers provide the already-resolved source of
    /// truth for port/resources: `rise compose` uses CLI/rise.toml values, while
    /// the backend uses the persisted deployment row.
    pub fn implicit_app(input: ImplicitAppContainer) -> Self {
        Self {
            containers: vec![ResolvedContainer {
                name: DEFAULT_CONTAINER_NAME.to_string(),
                image: input.image,
                build: input.build,
                port: Some(input.port),
                replicas: input.replicas,
                cpu: input.cpu,
                memory: input.memory,
                env: input.env,
                health_check: input.health_check,
            }],
            routes: vec![ResolvedRoute {
                path: "/".to_string(),
                container: DEFAULT_CONTAINER_NAME.to_string(),
                access: None,
            }],
        }
    }
}

impl ProjectBuildConfig {
    /// Resolve the explicit `[containers]` + `[routes]` from rise.toml.
    ///
    /// Returns an empty resolution when there is no `[containers]` block — a
    /// single-container project (top-level `[build]`/`[deploy]`, or `--image`)
    /// is driven by the CLI's single-container flow, and the backend synthesises
    /// its implicit `app` container at reconcile time.
    ///
    /// Returns `Err` when a route references a container that has no `port`.
    /// The CLI's `load_full_project_config` validator catches this for the
    /// on-disk path, but this method is public and callers may construct
    /// `ProjectBuildConfig` in-memory or otherwise bypass the validator — so we
    /// enforce it inline to avoid silently wiring a K8s Service to port 0.
    pub fn resolve_deploy(&self) -> Result<ResolvedDeploy, String> {
        if self.containers.is_empty() {
            return Ok(ResolvedDeploy::default());
        }

        // Hoisted top-level defaults: these are identical for every container,
        // so compute the clones once instead of per-iteration inside `.map()`.
        let top_deploy = self.deploy.clone().unwrap_or_default();
        let top_build = self.build.clone();

        let containers: Vec<ResolvedContainer> = self
            .containers
            .iter()
            .map(|(name, c)| {
                let c_deploy = c.deploy.clone().unwrap_or_default();

                // Build: field-level merge of the container's `[build]` over the
                // top-level `[build]` defaults. A container with `image` takes no
                // build at all.
                let build = if c.image.is_some() {
                    None
                } else {
                    match (top_build.clone(), c.build.clone()) {
                        (Some(top), Some(own)) => Some(own.merge_over(&top)),
                        (Some(top), None) => Some(top),
                        (None, own) => own,
                    }
                };

                // Deploy: each field falls back to the top-level default.
                // `health_check` only inherits when the container has a port —
                // a port-less worker silently skips an inherited default probe.
                let health_check = c_deploy.health_check.clone().or_else(|| {
                    if c.port.is_some() {
                        top_deploy.health_check.clone()
                    } else {
                        None
                    }
                });

                ResolvedContainer {
                    name: name.clone(),
                    image: c.image.clone(),
                    build,
                    port: c.port,
                    replicas: c_deploy.replicas.or(top_deploy.replicas),
                    cpu: c_deploy.cpu.clone().or_else(|| top_deploy.cpu.clone()),
                    memory: c_deploy
                        .memory
                        .clone()
                        .or_else(|| top_deploy.memory.clone()),
                    env: c.env.clone(),
                    health_check,
                }
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
                    access: None,
                }]
            } else {
                Vec::new()
            }
        } else {
            let mut resolved = Vec::with_capacity(self.routes.len());
            for (path, route) in &self.routes {
                // A route maps a path to a container; the effective port is
                // always the target container's `port`. Reject routes whose
                // target has no port so we never wire a K8s Service to port 0.
                if self
                    .containers
                    .get(&route.container)
                    .and_then(|c| c.port)
                    .is_none()
                {
                    return Err(format!(
                        "route '{}' targets container '{}' which has no port set",
                        path, route.container
                    ));
                }
                resolved.push(ResolvedRoute {
                    path: path.clone(),
                    container: route.container.clone(),
                    access: route.access.clone(),
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
        // rather than silently wiring a K8s Service to port 0.
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
                access: None,
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
    fn resolve_deploy_carries_per_route_access_and_defaults_to_none() {
        let toml = r#"
            [containers.web]
            image = "web:latest"
            port = 8080

            [routes."/"]
            container = "web"

            [routes."/healthz"]
            container = "web"
            access = "none"

            [routes."/admin"]
            container = "web"
            access = "Member"
        "#;
        let config: ProjectBuildConfig = toml::from_str(toml).unwrap();
        let resolved = config.resolve_deploy().unwrap();
        let by_path = |p: &str| {
            resolved
                .routes
                .iter()
                .find(|r| r.path == p)
                .unwrap_or_else(|| panic!("route {p} missing"))
                .access
                .clone()
        };
        assert_eq!(by_path("/"), None, "no `access` → inherit (None option)");
        assert_eq!(by_path("/healthz"), Some(AccessRequirement::None));
        assert_eq!(by_path("/admin"), Some(AccessRequirement::Member));
    }

    #[test]
    fn resolve_deploy_default_single_route_has_no_access_override() {
        let toml = r#"
            [containers.web]
            image = "web:latest"
            port = 8080
        "#;
        let config: ProjectBuildConfig = toml::from_str(toml).unwrap();
        let resolved = config.resolve_deploy().unwrap();
        assert_eq!(resolved.routes.len(), 1);
        assert_eq!(resolved.routes[0].path, "/");
        assert_eq!(resolved.routes[0].access, None);
    }

    #[test]
    fn implicit_app_synthesizes_container_and_root_route() {
        let rd = ResolvedDeploy::implicit_app(ImplicitAppContainer {
            port: 3000,
            replicas: Some(2),
            cpu: Some("250m".to_string()),
            memory: Some("512Mi".to_string()),
            ..Default::default()
        });

        assert_eq!(rd.containers.len(), 1);
        let app = &rd.containers[0];
        assert_eq!(app.name, DEFAULT_CONTAINER_NAME);
        assert_eq!(app.port, Some(3000));
        assert_eq!(app.replicas, Some(2));
        assert_eq!(app.cpu.as_deref(), Some("250m"));
        assert_eq!(app.memory.as_deref(), Some("512Mi"));
        assert_eq!(rd.routes.len(), 1);
        assert_eq!(rd.routes[0].path, "/");
        assert_eq!(rd.routes[0].container, DEFAULT_CONTAINER_NAME);
    }

    fn resolved_named<'a>(rd: &'a ResolvedDeploy, name: &str) -> &'a ResolvedContainer {
        rd.containers
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("container '{name}' not resolved"))
    }

    /// Build a `ProjectBuildConfig` from top-level build/deploy defaults and a
    /// set of named containers (struct-literal init to satisfy clippy).
    fn config_with(
        build: Option<BuildConfig>,
        deploy: Option<DeployConfig>,
        containers: Vec<(&str, ContainerConfig)>,
    ) -> ProjectBuildConfig {
        ProjectBuildConfig {
            build,
            deploy,
            containers: containers
                .into_iter()
                .map(|(n, c)| (n.to_string(), c))
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn resolve_deploy_inherits_top_level_deploy_per_field() {
        // `api` overrides only cpu; everything else inherits the top-level deploy.
        let config = config_with(
            None,
            Some(DeployConfig {
                replicas: Some(3),
                cpu: Some("250m".to_string()),
                memory: Some("256Mi".to_string()),
                health_check: None,
            }),
            vec![(
                "api",
                ContainerConfig {
                    image: Some("nginx:latest".to_string()),
                    port: Some(8080),
                    deploy: Some(DeployConfig {
                        cpu: Some("500m".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )],
        );

        let rd = config.resolve_deploy().expect("resolve");
        let api = resolved_named(&rd, "api");
        assert_eq!(api.cpu.as_deref(), Some("500m")); // override wins
        assert_eq!(api.memory.as_deref(), Some("256Mi")); // inherited
        assert_eq!(api.replicas, Some(3)); // inherited
    }

    #[test]
    fn resolve_deploy_merges_build_field_by_field() {
        let config = config_with(
            Some(BuildConfig {
                backend: Some("docker".to_string()),
                ..Default::default()
            }),
            None,
            vec![
                // Container with its own [build] overriding only the Dockerfile.
                (
                    "web",
                    ContainerConfig {
                        port: Some(8080),
                        build: Some(BuildConfig {
                            dockerfile: Some("web/Dockerfile".to_string()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                ),
                // Container that builds but declares no [build]: inherits all.
                ("worker", ContainerConfig::default()),
            ],
        );

        let rd = config.resolve_deploy().expect("resolve");
        let web = resolved_named(&rd, "web").build.as_ref().expect("build");
        assert_eq!(web.backend.as_deref(), Some("docker")); // inherited
        assert_eq!(web.dockerfile.as_deref(), Some("web/Dockerfile")); // override
        let worker = resolved_named(&rd, "worker").build.as_ref().expect("build");
        assert_eq!(worker.backend.as_deref(), Some("docker")); // inherited whole
    }

    #[test]
    fn resolve_deploy_image_container_takes_no_build() {
        let config = config_with(
            Some(BuildConfig {
                backend: Some("docker".to_string()),
                ..Default::default()
            }),
            None,
            vec![(
                "redis",
                ContainerConfig {
                    image: Some("redis:7".to_string()),
                    port: Some(6379),
                    ..Default::default()
                },
            )],
        );

        let rd = config.resolve_deploy().expect("resolve");
        let redis = resolved_named(&rd, "redis");
        assert!(redis.build.is_none(), "image container must take no build");
        assert_eq!(redis.image.as_deref(), Some("redis:7"));
    }

    #[test]
    fn resolve_deploy_inherits_health_check_only_with_port() {
        let config = config_with(
            None,
            Some(DeployConfig {
                health_check: Some(HealthCheckSetting::Config(HealthCheckConfig {
                    path: Some("/healthz".to_string()),
                    ..Default::default()
                })),
                ..Default::default()
            }),
            vec![
                (
                    "web",
                    ContainerConfig {
                        image: Some("nginx".to_string()),
                        port: Some(8080),
                        ..Default::default()
                    },
                ),
                (
                    "worker",
                    ContainerConfig {
                        image: Some("busybox".to_string()),
                        ..Default::default()
                    },
                ),
            ],
        );

        let rd = config.resolve_deploy().expect("resolve");
        assert!(
            resolved_named(&rd, "web").health_check.is_some(),
            "port-having container inherits the default probe"
        );
        assert!(
            resolved_named(&rd, "worker").health_check.is_none(),
            "port-less container does not inherit a default probe"
        );
    }
}

/// Workload identity configuration
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct IdentityConfig {
    /// Audiences to auto-mint workload identity tokens for and mount as files.
    /// Map key = in-pod token filename, value = the token audience.
    #[serde(default)]
    pub audiences: BTreeMap<String, String>,
}

/// Per-environment configuration
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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

/// Deployment resource configuration.
///
/// Shared by the top-level `[deploy]`, per-environment `[environments.<name>.deploy]`,
/// and per-container `[containers.<name>.deploy]` tables. Top-level values act as
/// defaults that each container inherits per field.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DeployConfig {
    /// Number of replicas
    pub replicas: Option<u32>,

    /// CPU allocation. Either a fixed value that sets both the K8s request and
    /// limit (e.g. `"500m"`, `"1"`), or a `request-limit` range (e.g. `"128m-1"`
    /// → request `128m`, limit `1`). The request may not exceed the limit, and
    /// the limit is validated against the environment/platform allowed range.
    pub cpu: Option<String>,

    /// Memory allocation. Either a fixed value that sets both the K8s request
    /// and limit (e.g. `"256Mi"`, `"1Gi"`), or a `request-limit` range (e.g.
    /// `"64Mi-256Mi"`). Same request/limit and range rules as `cpu`.
    pub memory: Option<String>,

    /// Health check configuration. HTTP liveness+readiness probes are disabled
    /// by default — they must be explicitly configured here. Set a config block
    /// to enable probes with a custom path/timing, or `health_check = false` to
    /// explicitly mark them disabled. At the container level this requires the
    /// container to have a `port`.
    pub health_check: Option<HealthCheckSetting>,
}

/// Project metadata configuration
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProjectConfig {
    /// Project name
    pub name: String,

    /// Plain-text environment variables (non-secret)
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Build configuration options for a project
#[derive(Debug, Deserialize, Serialize, Default, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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

impl BuildConfig {
    /// Merge `self` over `default`, field by field. Fields set on `self` win;
    /// unset fields fall back to `default`. Used to apply a top-level `[build]`
    /// table as defaults under each container's own `[build]`.
    pub fn merge_over(&self, default: &BuildConfig) -> BuildConfig {
        BuildConfig {
            backend: self.backend.clone().or_else(|| default.backend.clone()),
            builder: self.builder.clone().or_else(|| default.builder.clone()),
            buildpacks: self
                .buildpacks
                .clone()
                .or_else(|| default.buildpacks.clone()),
            args: self.args.clone().or_else(|| default.args.clone()),
            container_cli: self
                .container_cli
                .clone()
                .or_else(|| default.container_cli.clone()),
            managed_buildkit: self.managed_buildkit.or(default.managed_buildkit),
            dockerfile: self
                .dockerfile
                .clone()
                .or_else(|| default.dockerfile.clone()),
            build_context: self
                .build_context
                .clone()
                .or_else(|| default.build_context.clone()),
            build_contexts: self
                .build_contexts
                .clone()
                .or_else(|| default.build_contexts.clone()),
            no_cache: self.no_cache.or(default.no_cache),
            platform: self.platform.clone().or_else(|| default.platform.clone()),
        }
    }
}
