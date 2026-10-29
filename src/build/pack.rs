// Buildpacks implementation (pack CLI)

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use tracing::{debug, info};

use super::ssl::{SSL_CERT_PATHS, SSL_ENV_VARS};

pub(crate) struct BuildpackBuildOptions<'a> {
    pub app_path: &'a str,
    pub image_tag: &'a str,
    pub builder: Option<&'a str>,
    pub buildpacks: &'a [String],
    pub env: &'a [String],
    pub no_cache: bool,
    pub platform: &'a str,
    pub output: super::CommandOutput,
}

/// Build image using Cloud Native Buildpacks (pack CLI)
pub(crate) fn build_image_with_buildpacks(options: BuildpackBuildOptions<'_>) -> Result<()> {
    let BuildpackBuildOptions {
        app_path,
        image_tag,
        builder,
        buildpacks,
        env,
        no_cache,
        platform,
        output,
    } = options;
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

    let mut build_env = BTreeMap::new();
    for entry in env {
        let (key, value) = match entry.split_once('=') {
            Some((key, value)) => (key.to_string(), value.to_string()),
            None => (entry.clone(), std::env::var(entry).unwrap_or_default()),
        };
        build_env.insert(key, value);
    }
    build_env.extend(super::proxy::read_and_transform_proxy_vars());

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
            build_env.insert(
                ssl_env_name.to_string(),
                "/etc/ssl/certs/ca-certificates.crt".into(),
            );
        }

        info!(
            "Injecting CA certificate from: {} (resolved from: {})",
            resolved_path_str, ca_cert_path
        );
    }

    let _env_file = attach_env_file(&mut cmd, &build_env)?;
    debug!("Executing command: {}", super::command_for_log(&cmd));

    let status = super::run_command(cmd, "Failed to execute pack build", output)?;

    if !status.success() {
        bail!("pack build failed with status: {}", status);
    }

    Ok(())
}

/// Pack reads env files one line at a time and trims each line. Reject values
/// that cannot round-trip through that format instead of changing the value.
fn attach_env_file(
    command: &mut Command,
    values: &BTreeMap<String, String>,
) -> Result<Option<tempfile::NamedTempFile>> {
    if values.is_empty() {
        return Ok(None);
    }
    for (key, value) in values {
        if key.is_empty()
            || !key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        {
            bail!("Pack environment variable has an invalid name");
        }
        if value.contains(['\n', '\r', '\0']) || value.trim_end() != value {
            bail!("Pack env files cannot represent multiline values, NUL bytes, or trailing whitespace");
        }
    }
    let mut file = tempfile::Builder::new()
        .prefix("rise-pack-env-")
        .tempfile()
        .context("Failed to create private Pack env file")?;
    for (key, value) in values {
        writeln!(file, "{key}={value}").context("Failed to write private Pack env file")?;
    }
    command.arg("--env-file").arg(file.path());
    Ok(Some(file))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_env_file_keeps_values_out_of_argv_and_preserves_supported_values() {
        let values = BTreeMap::from([
            ("TOKEN".into(), "private=token#value".into()),
            ("EMPTY".into(), String::new()),
            ("SPACES".into(), " leading internal spaces".into()),
        ]);
        let mut command = Command::new("pack");
        let file = attach_env_file(&mut command, &values).unwrap().unwrap();
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args[0], "--env-file");
        assert_eq!(args[1], file.path().as_os_str());
        let line = super::super::command_for_log(&command);
        assert!(!line.contains("private=token#value"));
        let contents = std::fs::read_to_string(file.path()).unwrap();
        let parsed: BTreeMap<String, String> = contents
            .lines()
            .map(|line| {
                let (key, value) = line.trim().split_once('=').unwrap();
                (key.to_string(), value.to_string())
            })
            .collect();
        assert_eq!(parsed, values);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                file.as_file().metadata().unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn pack_rejects_unrepresentable_values_without_exposing_them() {
        for value in [
            "first\nsecond",
            "first\rsecond",
            "secret ",
            "secret\t",
            "secret\0",
        ] {
            let mut command = Command::new("pack");
            let error = attach_env_file(
                &mut command,
                &BTreeMap::from([("TOKEN".into(), value.into())]),
            )
            .unwrap_err();
            assert!(!error.to_string().contains(value));
            assert_eq!(command.get_args().count(), 0);
        }
    }
}
