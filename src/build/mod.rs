// Build module - Container image building orchestration
//
// This module provides a clean API for building container images using various
// backends (Docker, Pack, Railpack) and handles related concerns like BuildKit
// daemon management, SSL certificate handling, and registry operations.

mod buildkit;
pub mod config;
mod docker;
mod dockerfile_ssl;
mod method;
mod pack;
mod proxy;
mod railpack;
mod registry;
mod ssl;

/// Fallback target platform when nothing more specific (CLI flag, env var,
/// rise.toml, backend hint) is provided. We default to the host architecture
/// so local dev (e.g. ARM Mac + ARM minikube) just works; production deploys
/// override via the backend's `target_platform` hint.
pub fn host_platform() -> String {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        // Pass through other arches verbatim (e.g. "s390x", "riscv64") —
        // OCI platform strings happen to match Rust's arch names for these.
        other => other,
    };
    format!("linux/{arch}")
}

/// Where the resolved build platform value originated from.
///
/// Returned alongside the platform string by [`resolve_platform`] so callers
/// can report or react to the precedence level that was selected (e.g. log
/// "using host arch" vs. "using backend hint").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformSource {
    /// From the `--platform` CLI flag.
    CliFlag,
    /// From the `RISE_PLATFORM` environment variable.
    EnvVar,
    /// From `[build].platform` in rise.toml.
    RiseToml,
    /// From the backend-advertised architecture (`runtime_arch` platform
    /// capability), mapped to a `linux/{arch}` platform string.
    BackendHint,
    /// Legacy `linux/amd64` default, used when the backend predates the
    /// platform capabilities endpoint (the endpoint returned 404). Kept
    /// distinct from [`PlatformSource::BackendHint`] so logs don't claim the
    /// cluster actually advertised this architecture.
    LegacyBackendDefault,
    /// Fell through to `host_platform()` / `std::env::consts::ARCH`.
    HostFallback,
}

/// The build-platform hint resolved from the backend, distinguishing a real
/// advertised architecture (from a supported capabilities endpoint) from the
/// legacy default used when the backend predates the endpoint. This lets
/// [`resolve_platform`] label the [`PlatformSource`] honestly instead of
/// passing both off as [`PlatformSource::BackendHint`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendPlatformHint {
    /// Architecture advertised by a supported capabilities endpoint.
    Advertised(String),
    /// Legacy default for a backend without the capabilities endpoint.
    LegacyDefault(String),
}

impl BackendPlatformHint {
    fn into_resolved(self) -> (String, PlatformSource) {
        match self {
            BackendPlatformHint::Advertised(p) => (p, PlatformSource::BackendHint),
            BackendPlatformHint::LegacyDefault(p) => (p, PlatformSource::LegacyBackendDefault),
        }
    }
}

/// Resolve the build platform from the standard precedence chain:
/// CLI flag → `RISE_PLATFORM` → rise.toml `[build].platform` →
/// backend-advertised hint → host architecture.
///
/// All inputs are passed in explicitly — this function never reads process
/// state (env, files) of its own. That keeps it pure and trivially testable
/// without `std::env::set_var` racing against parallel tests. Callers are
/// responsible for reading `RISE_PLATFORM` (typically via
/// [`env_var_non_empty`]) and passing it as `env`.
pub fn resolve_platform(
    cli: Option<&str>,
    env: Option<&str>,
    project: Option<&str>,
    backend_hint: Option<BackendPlatformHint>,
) -> (String, PlatformSource) {
    if let Some(v) = cli {
        return (v.to_string(), PlatformSource::CliFlag);
    }
    if let Some(v) = env {
        return (v.to_string(), PlatformSource::EnvVar);
    }
    if let Some(v) = project {
        return (v.to_string(), PlatformSource::RiseToml);
    }
    if let Some(hint) = backend_hint {
        return hint.into_resolved();
    }
    (host_platform(), PlatformSource::HostFallback)
}

pub use method::BuildArgs;
pub(crate) use method::{BuildMethod, BuildOptions};
pub(crate) use railpack::{build_with_buildctl, BuildctlFrontend, RailpackBuildOptions};
pub(crate) use registry::{
    docker_login, docker_pull, docker_push, docker_tag, inject_registry_auth,
};

use anyhow::{bail, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

use buildkit::{check_ssl_cert_and_warn, ensure_managed_buildkit_daemon};
use docker::{build_image_with_dockerfile, DockerBuildOptions};
use method::{requires_buildkit, select_build_method};
use pack::build_image_with_buildpacks;
use railpack::build_image_with_railpacks;

/// Read an environment variable, treating empty strings as if the variable is not set.
///
/// This helper ensures that empty environment variables (e.g., `SSL_CERT_FILE=""`) are
/// handled the same as unset variables, avoiding errors when code attempts to use
/// empty paths or values.
pub(crate) fn env_var_non_empty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .and_then(|v| if v.is_empty() { None } else { Some(v) })
}

/// Resolve the `SSL_CERT_FILE` environment variable, returning `Some(path)` if
/// the variable is set and the file exists. Logs a warning when the variable is
/// set but points to a non-existent file.
pub(crate) fn resolve_ssl_cert_file() -> Option<std::path::PathBuf> {
    let ssl_cert_file = env_var_non_empty("SSL_CERT_FILE")?;
    let path = std::path::PathBuf::from(&ssl_cert_file);
    if path.exists() {
        Some(path)
    } else {
        warn!(
            "SSL_CERT_FILE set to '{}' but file not found",
            ssl_cert_file
        );
        None
    }
}

/// Parse a boolean environment variable.
///
/// Returns `Some(true)` for "true"/"1", `Some(false)` for "false"/"0",
/// and `None` if the variable is unset or empty. This ensures the env var
/// is authoritative when present — setting it to "false" explicitly disables
/// the feature rather than falling through to the next precedence level.
///
/// Panics with a descriptive message if the value is not a recognized boolean.
pub(crate) fn parse_bool_env_var(key: &str) -> Option<bool> {
    let val = env_var_non_empty(key)?;
    match val.to_lowercase().as_str() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => panic!(
            "Invalid boolean value for {}: {:?} (expected true/false/1/0)",
            key, val
        ),
    }
}

/// Main entry point for building container images
pub(crate) fn build_image(options: BuildOptions) -> Result<()> {
    let container_cli = &options.container_cli;

    debug!(
        "Using container CLI: {} ({:?})",
        container_cli.command(),
        container_cli.runtime()
    );
    info!(
        "Building image '{}' from path '{}'",
        options.image_tag, options.app_path
    );

    // Verify path exists
    let app_path = Path::new(&options.app_path);
    if !app_path.exists() {
        bail!("Path '{}' does not exist", options.app_path);
    }
    if !app_path.is_dir() {
        bail!("Path '{}' is not a directory", options.app_path);
    }

    // Select build method
    let (build_method, dockerfile) = select_build_method(
        &options.app_path,
        options.backend.as_deref(),
        options.dockerfile.as_deref(),
        container_cli.command(),
    )?;

    // Determine if we should use managed buildkit
    let managed_buildkit = match options.managed_buildkit {
        Some(value) => {
            // Explicitly set by user (CLI flag, config, or env var)
            value
        }
        None => {
            // Auto-detect: enable if all conditions met:
            // 1. Backend requires BuildKit
            // 2. SSL_CERT_FILE is set (needs injection)
            // 3. BUILDKIT_HOST is NOT set (user not managing their own)
            env_var_non_empty("BUILDKIT_HOST").is_none()
                && requires_buildkit(&build_method)
                && env_var_non_empty("SSL_CERT_FILE").is_some()
        }
    };

    // Handle BuildKit daemon management
    let buildkit_host = if requires_buildkit(&build_method) && managed_buildkit {
        // Check if user already has BUILDKIT_HOST (even if managed_buildkit=true)
        if let Some(existing_host) = env_var_non_empty("BUILDKIT_HOST") {
            info!("Using existing BUILDKIT_HOST: {}", existing_host);
            Some(existing_host)
        } else {
            // Create/manage our own buildkit daemon
            let ssl_cert_path = env_var_non_empty("SSL_CERT_FILE").map(PathBuf::from);
            Some(ensure_managed_buildkit_daemon(
                ssl_cert_path.as_deref(),
                container_cli,
            )?)
        }
    } else {
        // Check for SSL cert warnings if managed buildkit disabled
        check_ssl_cert_and_warn(&build_method, managed_buildkit);
        None
    };

    // Resolve build_context relative to app_path
    let resolved_build_context = options.build_context.as_ref().map(|ctx| {
        let resolved = app_path.join(ctx);
        resolved.to_string_lossy().to_string()
    });

    // Resolve build_contexts paths relative to app_path
    let resolved_build_contexts: std::collections::HashMap<String, String> = options
        .build_contexts
        .iter()
        .map(|(name, path)| {
            let resolved = app_path.join(path);
            (name.clone(), resolved.to_string_lossy().to_string())
        })
        .collect();

    // Execute build based on selected method
    match build_method {
        BuildMethod::Docker { use_buildx } => {
            if options.builder.is_some() {
                warn!("--builder flag is ignored when using docker build method");
            }
            if !options.buildpacks.is_empty() {
                warn!("--buildpack flags are ignored when using docker build method");
            }
            build_image_with_dockerfile(DockerBuildOptions {
                app_path: &options.app_path,
                dockerfile: dockerfile.as_deref(),
                image_tag: &options.image_tag,
                container_cli: container_cli.command(),
                buildx_supports_push: container_cli.buildx_supports_push(),
                use_buildx,
                push: options.push,
                buildkit_host: buildkit_host.as_deref(),
                env: &options.env,
                build_context: resolved_build_context.as_deref(),
                build_contexts: &resolved_build_contexts,
                no_cache: options.no_cache,
                platform: &options.platform,
            })?;
        }
        BuildMethod::Pack => {
            if options.explicit_container_cli {
                warn!("--container-cli flag is ignored when using pack build method");
            }
            if options.managed_buildkit.is_some() {
                warn!("--managed-buildkit flag is ignored when using pack build method");
            }
            build_image_with_buildpacks(
                &options.app_path,
                &options.image_tag,
                options.builder.as_deref(),
                &options.buildpacks,
                &options.env,
                options.no_cache,
                &options.platform,
            )?;

            // Pack doesn't support push during build, so push separately if requested
            if options.push {
                registry::docker_push(container_cli.command(), &options.image_tag)?;
            }
        }
        BuildMethod::Railpack { use_buildctl } => {
            if options.builder.is_some() {
                warn!("--builder flag is ignored when using railpack build method");
            }
            if !options.buildpacks.is_empty() {
                warn!("--buildpack flags are ignored when using railpack build method");
            }
            if use_buildctl && options.explicit_container_cli {
                warn!("--container-cli flag is ignored when using railpack:buildctl build method");
            }

            build_image_with_railpacks(RailpackBuildOptions {
                app_path: &options.app_path,
                image_tag: &options.image_tag,
                container_cli: container_cli.command(),
                buildx_supports_push: container_cli.buildx_supports_push(),
                use_buildctl,
                push: options.push,
                buildkit_host: buildkit_host.as_deref(),
                env: &options.env,
                no_cache: options.no_cache,
                platform: &options.platform,
            })?;
        }
        BuildMethod::Buildctl => {
            if options.builder.is_some() {
                warn!("--builder flag is ignored when using buildctl build method");
            }
            if !options.buildpacks.is_empty() {
                warn!("--buildpack flags are ignored when using buildctl build method");
            }
            if options.explicit_container_cli {
                warn!("--container-cli flag is ignored when using buildctl build method");
            }
            // Check for SSL certificate
            let ssl_cert_path = resolve_ssl_cert_file();

            // Construct dockerfile path
            let original_dockerfile_path = dockerfile
                .as_ref()
                .map(|df| Path::new(&options.app_path).join(df))
                .unwrap_or_else(|| Path::new(&options.app_path).join("Dockerfile"));

            // Preprocess Dockerfile for SSL if cert is available
            let (_temp_dir, effective_dockerfile) = if ssl_cert_path.is_some() {
                if original_dockerfile_path.exists() {
                    info!("SSL_CERT_FILE detected, preprocessing Dockerfile for bind mounts");
                    let (temp_dir, processed_path) =
                        dockerfile_ssl::preprocess_dockerfile_for_ssl(&original_dockerfile_path)?;
                    (Some(temp_dir), processed_path)
                } else {
                    (None, original_dockerfile_path)
                }
            } else {
                (None, original_dockerfile_path)
            };

            // Parse env vars into HashMap for secrets
            let mut secrets = proxy::read_and_transform_proxy_vars();
            secrets.extend(proxy::parse_env_vars(&options.env)?);

            // Add SSL cert using named build context (bind mount)
            // RAII cleanup via SslCertContext drop
            let mut local_contexts = HashMap::new();
            let _ssl_cert_context: Option<dockerfile_ssl::SslCertContext> =
                if let Some(ref cert_path) = ssl_cert_path {
                    // Create temp directory with cert for bind mount
                    // Using a separate local context keeps the cert separate from the main context
                    // and reduces risk of accidental inclusion via generic COPY commands
                    let context = dockerfile_ssl::SslCertContext::new(cert_path)?;

                    // Add to local_contexts map for buildctl --local argument
                    local_contexts.insert(
                        dockerfile_ssl::SSL_CERT_BUILD_CONTEXT.to_string(),
                        context.context_path.to_string_lossy().to_string(),
                    );

                    Some(context)
                } else {
                    None
                };

            build_with_buildctl(
                &options.app_path,
                &effective_dockerfile,
                &options.image_tag,
                options.push,
                buildkit_host.as_deref(),
                &secrets,
                &local_contexts,
                BuildctlFrontend::Dockerfile,
                options.no_cache,
                container_cli.command(),
                &options.platform,
            )?;

            // Note: SslCertContext cleanup is automatic via RAII when it goes out of scope
        }
    }

    info!("✓ Successfully built image '{}'", options.image_tag);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_env_var_non_empty_with_empty_string() {
        // Test that empty string is treated as unset
        std::env::set_var("TEST_EMPTY_VAR", "");
        assert_eq!(env_var_non_empty("TEST_EMPTY_VAR"), None);
        std::env::remove_var("TEST_EMPTY_VAR");
    }

    #[test]
    fn test_env_var_non_empty_with_value() {
        // Test that non-empty value is returned
        std::env::set_var("TEST_VALUE_VAR", "some_value");
        assert_eq!(
            env_var_non_empty("TEST_VALUE_VAR"),
            Some("some_value".to_string())
        );
        std::env::remove_var("TEST_VALUE_VAR");
    }

    #[test]
    fn test_env_var_non_empty_with_unset() {
        // Test that unset variable returns None
        std::env::remove_var("TEST_UNSET_VAR");
        assert_eq!(env_var_non_empty("TEST_UNSET_VAR"), None);
    }

    #[test]
    fn test_env_var_non_empty_with_whitespace() {
        // Test that whitespace-only string is NOT treated as empty
        // (only fully empty strings are treated as unset)
        std::env::set_var("TEST_WHITESPACE_VAR", "   ");
        assert_eq!(
            env_var_non_empty("TEST_WHITESPACE_VAR"),
            Some("   ".to_string())
        );
        std::env::remove_var("TEST_WHITESPACE_VAR");
    }

    #[test]
    fn test_host_platform_format() {
        // Whatever the host arch, we always produce a "linux/<arch>" string.
        let p = host_platform();
        assert!(
            p.starts_with("linux/") && p.len() > "linux/".len(),
            "host_platform produced unexpected value: {p:?}"
        );
    }

    #[test]
    fn test_resolve_platform_cli_wins() {
        let (value, source) = resolve_platform(
            Some("linux/cli-wins"),
            Some("linux/env"),
            Some("linux/project"),
            Some(BackendPlatformHint::Advertised("linux/backend".to_string())),
        );
        assert_eq!(value, "linux/cli-wins");
        assert_eq!(source, PlatformSource::CliFlag);
    }

    #[test]
    fn test_resolve_platform_env_over_project_and_backend() {
        let (value, source) = resolve_platform(
            None,
            Some("linux/env-wins"),
            Some("linux/project"),
            Some(BackendPlatformHint::Advertised("linux/backend".to_string())),
        );
        assert_eq!(value, "linux/env-wins");
        assert_eq!(source, PlatformSource::EnvVar);
    }

    #[test]
    fn test_resolve_platform_project_over_backend() {
        let (value, source) = resolve_platform(
            None,
            None,
            Some("linux/project"),
            Some(BackendPlatformHint::Advertised("linux/backend".to_string())),
        );
        assert_eq!(value, "linux/project");
        assert_eq!(source, PlatformSource::RiseToml);
    }

    #[test]
    fn test_resolve_platform_backend_over_host() {
        let (value, source) = resolve_platform(
            None,
            None,
            None,
            Some(BackendPlatformHint::Advertised(
                "linux/backend-arch".to_string(),
            )),
        );
        assert_eq!(value, "linux/backend-arch");
        assert_eq!(source, PlatformSource::BackendHint);
    }

    #[test]
    fn test_resolve_platform_legacy_backend_default() {
        let (value, source) = resolve_platform(
            None,
            None,
            None,
            Some(BackendPlatformHint::LegacyDefault(
                "linux/amd64".to_string(),
            )),
        );
        assert_eq!(value, "linux/amd64");
        assert_eq!(source, PlatformSource::LegacyBackendDefault);
    }

    #[test]
    fn test_resolve_platform_host_fallback() {
        let (value, source) = resolve_platform(None, None, None, None);
        assert_eq!(value, host_platform());
        assert_eq!(source, PlatformSource::HostFallback);
    }
}
