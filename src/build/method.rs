// Build method selection and configuration

use anyhow::{bail, Result};
use clap::Args;
use std::path::Path;
use tracing::info;

use crate::config::{Config, ContainerCli};

/// Build method for container images
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum BuildMethod {
    Docker {
        use_buildx: bool,
    },
    Pack,
    Railpack {
        use_buildctl: bool,
    },
    /// Plain buildctl with Dockerfile (without railpack)
    Buildctl,
}

/// Build-related CLI arguments that can be flattened into command structs
#[derive(Debug, Clone, Args)]
pub struct BuildArgs {
    /// Build backend (docker[:build|:buildx|:buildctl], pack, railpack[:buildx|:buildctl])
    #[arg(long)]
    pub backend: Option<String>,

    /// Buildpack builder to use (only for pack backend)
    #[arg(long)]
    pub builder: Option<String>,

    /// Buildpack(s) to use (only for pack backend). Can be specified multiple times.
    #[arg(long = "buildpack", short = 'B')]
    pub buildpacks: Vec<String>,

    /// Build-time arguments (for build configuration only).
    /// Format: KEY=VALUE (with explicit value) or KEY (reads from current environment).
    ///
    /// Examples:
    ///   -b NODE_ENV=production -b API_VERSION=1.2.3
    ///   -b DATABASE_URL  (reads DATABASE_URL from current environment)
    ///
    /// Can also be configured in rise.toml under [build] section as `args`.
    /// CLI values are merged with rise.toml values.
    ///
    /// WARNING: Build args are for build configuration (compiler flags,
    /// tool versions, feature toggles), NOT runtime secrets. For runtime secrets,
    /// use '-e' / '--env' on deploy, or 'rise env set --secret'.
    #[arg(long = "build-arg", short = 'b', value_name = "KEY=VALUE")]
    pub build_env: Vec<String>,

    /// Container CLI to use (docker or podman)
    #[arg(long)]
    pub container_cli: Option<String>,

    /// Enable managed BuildKit daemon with SSL certificate support
    #[arg(long, value_parser = clap::value_parser!(bool), default_missing_value = "true", num_args = 0..=1)]
    pub managed_buildkit: Option<bool>,

    /// Path to Dockerfile (relative to app path / rise.toml location). Defaults to "Dockerfile" or "Containerfile"
    #[arg(long)]
    pub dockerfile: Option<String>,

    /// Default build context (docker/podman only) - the context directory for the build.
    /// This is the path argument to `docker build <path>`. Defaults to app path.
    /// Path is relative to the app path / rise.toml location.
    #[arg(long = "context")]
    pub build_context: Option<String>,

    /// Build contexts for multi-stage builds (docker/podman only). Can be specified multiple times.
    /// Format: name=path where path is relative to app path / rise.toml location
    #[arg(long = "build-context")]
    pub build_contexts: Vec<String>,

    /// Disable build cache (equivalent to docker build --no-cache, pack build --clear-cache)
    #[arg(long)]
    pub no_cache: bool,

    /// Target platform for the container image build (e.g., linux/amd64, linux/arm64).
    /// When unset, falls back to the backend-advertised cluster arch (during
    /// `rise deploy`) or the host architecture.
    #[arg(long)]
    pub platform: Option<String>,
}

/// Options for building container images
#[derive(Debug, Clone)]
pub(crate) struct BuildOptions {
    pub image_tag: String,
    pub app_path: String,
    pub backend: Option<String>,
    pub builder: Option<String>,
    pub buildpacks: Vec<String>,
    pub env: Vec<String>,
    pub container_cli: ContainerCli,
    /// Whether --container-cli was explicitly provided (for "ignored" warnings)
    pub explicit_container_cli: bool,
    /// None = auto-detect based on SSL_CERT_FILE and BUILDKIT_HOST
    /// Some(true) = explicitly enable managed buildkit
    /// Some(false) = explicitly disable managed buildkit
    pub managed_buildkit: Option<bool>,
    pub push: bool,
    /// Path to Dockerfile (relative to app_path / rise.toml location)
    pub dockerfile: Option<String>,
    /// Default build context (relative to app_path / rise.toml location)
    /// This is the path argument to `docker build <path>`. Defaults to app_path if None.
    /// Note: This value is resolved to an absolute path in build_image() before use.
    pub build_context: Option<String>,
    /// Build contexts for multi-stage builds (docker/podman only)
    /// Format: name -> path (relative to app_path / rise.toml location)
    /// Note: These paths are resolved to absolute paths in build_image() before use.
    pub build_contexts: std::collections::HashMap<String, String>,
    /// Disable build cache
    pub no_cache: bool,
    /// Target platform (e.g., "linux/amd64")
    pub platform: String,
    /// Where `platform` was sourced from (CLI / env / rise.toml / backend / host).
    /// Lets callers report or react to the precedence level that won.
    pub platform_source: crate::build::PlatformSource,
}

impl BuildOptions {
    /// Create BuildOptions from BuildArgs and Config.
    ///
    /// Configuration precedence (highest to lowest):
    /// 1. CLI flags (BuildArgs)
    /// 2. Environment variables (RISE_*)
    /// 3. Project config file (rise.toml / .rise.toml)
    /// 4. Global config file (via Config getters)
    /// 5. Auto-detection/defaults (via Config getters)
    ///
    /// When `preloaded_config` is provided, its `.build` section is used instead
    /// of loading rise.toml from disk, avoiding duplicate file reads/warnings.
    ///
    /// `backend_platform_hint` is the `target_platform` advertised by the
    /// backend (from the registry-credentials response); it sits below
    /// rise.toml in precedence but above the host-arch fallback.
    pub(crate) fn from_build_args(
        config: &Config,
        image_tag: String,
        app_path: String,
        build_args: &BuildArgs,
        preloaded_config: Option<crate::build::config::ProjectBuildConfig>,
        backend_platform_hint: Option<&str>,
    ) -> Self {
        use tracing::warn;

        // Use preloaded config if available, otherwise load from disk
        let project_config = if let Some(cfg) = preloaded_config {
            cfg.build
        } else {
            match crate::build::config::load_full_project_config(&app_path) {
                Ok(cfg) => cfg.and_then(|c| c.build),
                Err(e) => {
                    warn!(
                        "Failed to load project config: {:#}. Continuing without it.",
                        e
                    );
                    None
                }
            }
        };

        // Read RISE_PLATFORM exactly once here (instead of inside
        // resolve_platform) and pass it explicitly. Keeps resolve_platform
        // pure / unit-testable without touching process-global env.
        let env_platform = crate::build::env_var_non_empty("RISE_PLATFORM");
        let (platform, platform_source) = crate::build::resolve_platform(
            build_args.platform.as_deref(),
            env_platform.as_deref(),
            project_config.as_ref().and_then(|c| c.platform.as_deref()),
            backend_platform_hint,
        );

        // Merge: CLI > Project > Environment (via Config) > Global (via Config) > Defaults
        Self {
            image_tag,
            app_path,

            // String options - use first non-None value
            backend: build_args
                .backend
                .clone()
                .or_else(|| project_config.as_ref().and_then(|c| c.backend.clone())),

            builder: build_args
                .builder
                .clone()
                .or_else(|| project_config.as_ref().and_then(|c| c.builder.clone())),

            container_cli: {
                let explicit = build_args
                    .container_cli
                    .clone()
                    .or_else(|| crate::build::env_var_non_empty("RISE_CONTAINER_CLI"))
                    .or_else(|| {
                        project_config
                            .as_ref()
                            .and_then(|c| c.container_cli.clone())
                    });
                match explicit {
                    Some(name) => ContainerCli::from_command(name),
                    None => config.get_container_cli(),
                }
            },
            explicit_container_cli: build_args.container_cli.is_some(),

            // Vector options - all vectors merge config + CLI values (append)
            buildpacks: {
                let mut packs = project_config
                    .as_ref()
                    .and_then(|c| c.buildpacks.clone())
                    .unwrap_or_default();
                packs.extend(build_args.buildpacks.clone());
                packs
            },

            env: {
                let mut env = project_config
                    .as_ref()
                    .and_then(|c| c.args.clone())
                    .unwrap_or_default();
                env.extend(build_args.build_env.clone());
                env
            },

            // Boolean options: CLI flag > env var > project config > global config > auto-detect
            managed_buildkit: build_args
                .managed_buildkit
                .or_else(|| crate::build::parse_bool_env_var("RISE_MANAGED_BUILDKIT"))
                .or_else(|| project_config.as_ref().and_then(|c| c.managed_buildkit))
                .or(config.managed_buildkit),
            dockerfile: build_args
                .dockerfile
                .clone()
                .or_else(|| project_config.as_ref().and_then(|c| c.dockerfile.clone())),

            build_context: build_args.build_context.clone().or_else(|| {
                project_config
                    .as_ref()
                    .and_then(|c| c.build_context.clone())
            }),

            // Build contexts - merge config + CLI values (CLI overrides config for same name)
            build_contexts: {
                let mut contexts = project_config
                    .as_ref()
                    .and_then(|c| c.build_contexts.clone())
                    .unwrap_or_default();

                // Parse and merge CLI build contexts (format: "name=path")
                for ctx in &build_args.build_contexts {
                    if let Some((name, path)) = ctx.split_once('=') {
                        contexts.insert(name.to_string(), path.to_string());
                    } else {
                        warn!(
                            "Invalid build context format '{}'. Expected 'name=path'. Ignoring.",
                            ctx
                        );
                    }
                }

                contexts
            },

            no_cache: build_args.no_cache
                || project_config
                    .as_ref()
                    .and_then(|c| c.no_cache)
                    .unwrap_or(false),

            platform,
            platform_source,

            push: false,
        }
    }

    /// Builder method to set push flag
    pub(crate) fn with_push(mut self, push: bool) -> Self {
        self.push = push;
        self
    }
}

impl BuildMethod {
    /// Parse backend string into BuildMethod
    pub(crate) fn from_backend_str(backend: &str) -> Result<Self> {
        match backend {
            "docker" | "docker:build" => Ok(BuildMethod::Docker { use_buildx: false }),
            "docker:buildx" => Ok(BuildMethod::Docker { use_buildx: true }),
            "buildctl" | "docker:buildctl" => Ok(BuildMethod::Buildctl),
            "pack" => Ok(BuildMethod::Pack),
            "railpack" | "railpack:buildx" => Ok(BuildMethod::Railpack {
                use_buildctl: false,
            }),
            "railpack:buildctl" => Ok(BuildMethod::Railpack { use_buildctl: true }),
            _ => bail!(
                "Invalid build backend '{}'. Supported: docker, docker:build, docker:buildx, buildctl, docker:buildctl, pack, railpack, railpack:buildctl",
                backend
            ),
        }
    }
}

/// Select build method based on explicit backend or auto-detection
/// Returns (BuildMethod, Option<dockerfile_path>)
pub(crate) fn select_build_method(
    app_path: &str,
    backend: Option<&str>,
    dockerfile: Option<&str>,
    container_cli: &str,
) -> Result<(BuildMethod, Option<String>)> {
    // Determine dockerfile path
    let (dockerfile_path, dockerfile_relative) = if let Some(df) = dockerfile {
        let path = Path::new(app_path).join(df);
        (path, Some(df.to_string()))
    } else {
        // Auto-detect: Dockerfile first, then Containerfile
        let dockerfile = Path::new(app_path).join("Dockerfile");
        let containerfile = Path::new(app_path).join("Containerfile");
        if dockerfile.exists() && dockerfile.is_file() {
            (dockerfile, Some("Dockerfile".to_string()))
        } else if containerfile.exists() && containerfile.is_file() {
            info!("Detected Containerfile");
            (containerfile, Some("Containerfile".to_string()))
        } else {
            // No dockerfile found
            (Path::new(app_path).join("Dockerfile"), None)
        }
    };

    if let Some(backend_str) = backend {
        // Explicit backend specified
        let method = BuildMethod::from_backend_str(backend_str)?;
        Ok((method, dockerfile_relative))
    } else {
        // Auto-detect based on dockerfile presence
        if dockerfile_path.exists() && dockerfile_path.is_file() {
            // Check if buildx is available
            let use_buildx = super::docker::is_buildx_available(container_cli);
            if use_buildx {
                info!(
                    "Detected {}, using docker:buildx backend",
                    dockerfile_relative.as_deref().unwrap_or("Dockerfile")
                );
            } else {
                info!(
                    "Detected {}, using docker backend",
                    dockerfile_relative.as_deref().unwrap_or("Dockerfile")
                );
            }
            Ok((BuildMethod::Docker { use_buildx }, dockerfile_relative))
        } else {
            info!("No Dockerfile found, using railpack backend");
            Ok((
                BuildMethod::Railpack {
                    use_buildctl: false,
                },
                None,
            ))
        }
    }
}

/// Check if a build method requires BuildKit
pub(crate) fn requires_buildkit(method: &BuildMethod) -> bool {
    matches!(
        method,
        BuildMethod::Docker { use_buildx: true }
            | BuildMethod::Railpack { .. }
            | BuildMethod::Buildctl
    )
}
