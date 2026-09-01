// Docker/Dockerfile builds

use anyhow::{bail, Result};
use std::path::Path;
use std::process::Command;
use tracing::{debug, info, warn};

use super::dockerfile_ssl::{
    preprocess_dockerfile_for_ssl, SslCertContext, SSL_CERT_BUILD_CONTEXT,
};
use super::BuildPushMode;

/// Configure buildx output flags (`--push` / `--load`) on a command.
///
/// Returns `true` when a fallback `docker push` is needed after the build
/// (i.e. push was requested but `--push` is not supported).
pub(crate) fn configure_buildx_output(
    cmd: &mut Command,
    push_mode: BuildPushMode,
    buildx_supports_push: bool,
) -> bool {
    match push_mode {
        BuildPushMode::Inline if buildx_supports_push => {
            cmd.arg("--push");
            false
        }
        BuildPushMode::Inline => {
            cmd.arg("--load");
            true
        }
        BuildPushMode::Disabled | BuildPushMode::Deferred => {
            // Without --push, load into the local daemon so a caller-owned
            // later push can work.
            cmd.arg("--load");
            false
        }
    }
}

/// Check if buildx is available for the given container CLI
pub(crate) fn is_buildx_available(container_cli: &str) -> bool {
    Command::new(container_cli)
        .args(["buildx", "version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Options for building with Docker/Podman
pub(crate) struct DockerBuildOptions<'a> {
    pub app_path: &'a str,
    pub dockerfile: Option<&'a str>,
    pub image_tag: &'a str,
    pub container_cli: &'a str,
    pub buildx_supports_push: bool,
    pub use_buildx: bool,
    pub push_mode: BuildPushMode,
    pub buildkit_host: Option<&'a str>,
    pub env: &'a [String],
    pub build_context: Option<&'a str>,
    pub build_contexts: &'a std::collections::HashMap<String, String>,
    pub no_cache: bool,
    pub platform: &'a str,
    pub output: super::CommandOutput,
}

/// Build image using Docker or Podman with a Dockerfile
pub(crate) fn build_image_with_dockerfile(options: DockerBuildOptions) -> Result<()> {
    // Check if container CLI is available
    let cli_check = Command::new(options.container_cli)
        .arg("--version")
        .output();
    if cli_check.is_err() {
        bail!(
            "{} CLI not found. Please install Docker or Podman.",
            options.container_cli
        );
    }

    // Check for SSL certificate and determine if preprocessing is needed
    let ssl_cert_path = super::resolve_ssl_cert_file();

    // Warn if SSL cert is set but buildx is not being used
    if ssl_cert_path.is_some() && !options.use_buildx {
        warn!(
            "SSL_CERT_FILE is set but docker:build does not support BuildKit features \
             required for SSL certificate bind mounts. Use 'docker:buildx' backend for \
             SSL certificate support during builds."
        );
    }

    // Preprocess Dockerfile for SSL if using buildx and SSL cert is available
    let (_temp_dir, effective_dockerfile) = if options.use_buildx && ssl_cert_path.is_some() {
        let original_dockerfile = options
            .dockerfile
            .map(|df| Path::new(options.app_path).join(df))
            .unwrap_or_else(|| Path::new(options.app_path).join("Dockerfile"));

        if original_dockerfile.exists() {
            info!("SSL_CERT_FILE detected, preprocessing Dockerfile for bind mounts");
            let (temp_dir, processed_path) = preprocess_dockerfile_for_ssl(&original_dockerfile)?;
            (Some(temp_dir), Some(processed_path))
        } else {
            (
                None,
                options
                    .dockerfile
                    .map(|df| Path::new(options.app_path).join(df)),
            )
        }
    } else {
        (
            None,
            options
                .dockerfile
                .map(|df| Path::new(options.app_path).join(df)),
        )
    };

    let mut cmd = Command::new(options.container_cli);

    if options.use_buildx {
        // Check buildx availability
        if !is_buildx_available(options.container_cli) {
            bail!(
                "{} buildx not available. Install it or use docker:build backend instead.",
                options.container_cli
            );
        }

        cmd.arg("buildx");
        info!(
            "Building image with {} buildx: {}",
            options.container_cli, options.image_tag
        );
    } else {
        info!(
            "Building image with {}: {}",
            options.container_cli, options.image_tag
        );
    }

    cmd.arg("build").arg("-t").arg(options.image_tag);

    // Add no-cache flag if requested
    if options.no_cache {
        cmd.arg("--no-cache");
    }

    // Add dockerfile path if specified or preprocessed
    if let Some(ref df) = effective_dockerfile {
        cmd.arg("-f").arg(df);
    }

    // Add platform flag for consistent architecture
    cmd.arg("--platform").arg(options.platform);

    // Add SSL certificate using named build context (bind mount)
    // RAII cleanup via SslCertContext drop
    let _ssl_cert_context: Option<SslCertContext> = if options.use_buildx {
        if let Some(ref cert_path) = ssl_cert_path {
            // Create temp directory with cert for bind mount
            // Using a named build context keeps the cert separate from the main context,
            // reducing risk of accidental inclusion via generic COPY commands (though it
            // can still be referenced explicitly via COPY --from={SSL_CERT_BUILD_CONTEXT}).
            let context = SslCertContext::new(cert_path)?;

            // Add named build context for SSL certificate
            cmd.arg("--build-context").arg(format!(
                "{}={}",
                SSL_CERT_BUILD_CONTEXT,
                context.context_path.display()
            ));

            Some(context)
        } else {
            None
        }
    } else {
        None
    };

    // Add proxy build arguments with host gateway resolution
    let proxy_vars = super::proxy::read_and_transform_proxy_vars();
    if !proxy_vars.is_empty() {
        let container_name = options
            .buildkit_host
            .map(|h| h.strip_prefix("docker-container://").unwrap_or(h));

        let effective_proxy_vars = super::buildkit::resolve_and_apply_host_gateway(
            &mut cmd,
            options.container_cli,
            &proxy_vars,
            container_name,
            options.buildkit_host.is_some(),
        );

        info!("Injecting proxy variables for docker build");
        for (key, value) in &effective_proxy_vars {
            cmd.arg("--build-arg").arg(format!("{}={}", key, value));
        }
    }

    // Add user-specified build arguments
    for build_arg in options.env {
        cmd.arg("--build-arg").arg(build_arg);
    }

    // Add build contexts (additional named contexts for multi-stage builds)
    if !options.build_contexts.is_empty() {
        info!("Using {} build context(s)", options.build_contexts.len());
        for (name, path) in options.build_contexts {
            cmd.arg("--build-context").arg(format!("{}={}", name, path));
            debug!("Build context: {}={}", name, path);
        }
    }

    // Use custom build context or default to app_path
    let context_path = options.build_context.unwrap_or(options.app_path);
    cmd.arg(context_path);

    // Set BUILDKIT_HOST if provided and using buildx
    if options.use_buildx {
        if let Some(host) = options.buildkit_host {
            cmd.env("BUILDKIT_HOST", host);
        }
    }

    let needs_fallback_push = if options.use_buildx {
        configure_buildx_output(&mut cmd, options.push_mode, options.buildx_supports_push)
    } else {
        options.push_mode == BuildPushMode::Inline
    };

    debug!("Executing command: {:?}", cmd);

    let status = super::run_command(
        cmd,
        &format!("Failed to execute {} build", options.container_cli),
        options.output,
    )?;

    // Note: SslCertContext cleanup is automatic via RAII when it goes out of scope

    if !status.success() {
        bail!(
            "{} build failed with status: {}",
            options.container_cli,
            status
        );
    }

    if needs_fallback_push {
        super::registry::docker_push_with_output(
            options.container_cli,
            options.image_tag,
            options.output,
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::configure_buildx_output;
    use crate::build::BuildPushMode;
    use std::process::Command;

    fn args(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect()
    }

    #[test]
    fn buildx_uses_native_push_when_supported_by_default() {
        let mut cmd = Command::new("docker");

        let fallback = configure_buildx_output(&mut cmd, BuildPushMode::Inline, true);

        assert!(!fallback);
        assert_eq!(args(&cmd), vec!["--push"]);
    }

    #[test]
    fn buildx_load_for_later_push_loads_without_fallback_push() {
        let mut cmd = Command::new("docker");

        let fallback = configure_buildx_output(&mut cmd, BuildPushMode::Deferred, true);

        assert!(!fallback);
        assert_eq!(args(&cmd), vec!["--load"]);
    }

    #[test]
    fn buildx_without_push_loads_without_fallback_push() {
        let mut cmd = Command::new("docker");

        let fallback = configure_buildx_output(&mut cmd, BuildPushMode::Disabled, true);

        assert!(!fallback);
        assert_eq!(args(&cmd), vec!["--load"]);
    }

    #[test]
    fn buildx_inline_push_without_native_support_loads_and_fallback_pushes() {
        let mut cmd = Command::new("docker");

        let fallback = configure_buildx_output(&mut cmd, BuildPushMode::Inline, false);

        assert!(fallback);
        assert_eq!(args(&cmd), vec!["--load"]);
    }
}
