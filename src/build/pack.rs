// Buildpacks implementation (pack CLI)

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;
use tracing::{debug, info};

use super::ssl::{SSL_CERT_PATHS, SSL_ENV_VARS};

/// Build image using Cloud Native Buildpacks (pack CLI)
pub(crate) fn build_image_with_buildpacks(
    app_path: &str,
    image_tag: &str,
    builder: Option<&str>,
    buildpacks: &[String],
    env: &[String],
    no_cache: bool,
    platform: &str,
    output: super::CommandOutput,
) -> Result<()> {
    // Check if pack CLI is available
    let pack_check = Command::new("pack").arg("version").output();

    if pack_check.is_err() {
        bail!(
            "pack CLI not found. Please install it from https://buildpacks.io/docs/tools/pack/\n\
             On macOS: brew install buildpacks/tap/pack\n\
             On Linux: see https://buildpacks.io/docs/tools/pack/"
        );
    }

    let builder_image = builder.unwrap_or("heroku/builder:24");
    info!("Using builder: {}", builder_image);

    let mut cmd = Command::new("pack");
    cmd.arg("build")
        .arg(image_tag)
        .arg("--path")
        .arg(app_path)
        .arg("--docker-host")
        .arg("inherit")
        .arg("--network")
        .arg("host")
        .arg("--builder")
        .arg(builder_image)
        .arg("--platform")
        .arg(platform);

    // Add clear-cache flag if requested
    if no_cache {
        cmd.arg("--clear-cache");
    }

    // Add buildpacks if specified
    if !buildpacks.is_empty() {
        info!("Using buildpacks: {:?}", buildpacks);
        for buildpack in buildpacks {
            cmd.arg("--buildpack").arg(buildpack);
        }
    }

    // Add environment variables if specified
    if !env.is_empty() {
        info!("Using environment variables: {:?}", env);
        for env_var in env {
            cmd.arg("--env").arg(env_var);
        }
    }

    // Add proxy environment variables
    let proxy_vars = super::proxy::read_and_transform_proxy_vars();
    if !proxy_vars.is_empty() {
        info!("Injecting proxy variables for pack build");
        for arg in super::proxy::format_for_pack(&proxy_vars) {
            cmd.arg("--env").arg(arg);
        }
    }

    // Never use --publish - always build locally and push separately
    // This avoids code bifurcation and allows CA certificate injection

    // If SSL_CERT_FILE is set, inject CA certificate into lifecycle container
    if let Some(ca_cert_path) = super::env_var_non_empty("SSL_CERT_FILE") {
        let cert_path = Path::new(&ca_cert_path);

        // Validate the file exists
        if !cert_path.exists() {
            bail!("CA certificate file not found: {}", ca_cert_path);
        }

        // Convert to absolute path if relative
        let absolute_path = if cert_path.is_absolute() {
            cert_path.to_path_buf()
        } else {
            std::env::current_dir()
                .context("Failed to get current directory")?
                .join(cert_path)
        };

        // Resolve symlinks to get the actual file path
        let resolved_path = absolute_path.canonicalize().with_context(|| {
            format!(
                "Failed to resolve certificate path: {}",
                absolute_path.display()
            )
        })?;

        let resolved_path_str = resolved_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Certificate path contains invalid UTF-8"))?;

        // Mount the CA certificate to all common system CA paths since we can't predict
        // which base image/distro the buildpack will use (Debian, Alpine, RedHat, etc.)
        for ssl_path in SSL_CERT_PATHS {
            cmd.arg("--volume")
                .arg(format!("{resolved_path_str}:{ssl_path}:ro"));
        }

        // Set multiple SSL environment variables to ensure CA trust works across different
        // language ecosystems (Node.js, Python, Nix, AWS SDK, etc.)
        for ssl_env_name in SSL_ENV_VARS {
            cmd.arg("--env")
                .arg(format!("{ssl_env_name}=/etc/ssl/certs/ca-certificates.crt"));
        }

        info!(
            "Injecting CA certificate from: {} (resolved from: {})",
            resolved_path_str, ca_cert_path
        );
    }

    debug!("Executing command: {:?}", cmd);

    let status = super::run_command(cmd, "Failed to execute pack build", output)?;

    if !status.success() {
        bail!("pack build failed with status: {}", status);
    }

    Ok(())
}
