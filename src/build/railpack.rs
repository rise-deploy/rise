// Railpack builds (buildx & buildctl variants)

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use tracing::{debug, info, warn};

use super::buildkit::ensure_buildx_builder;
use super::proxy;
use super::registry::docker_push_with_output;
use super::ssl::embed_ssl_cert_in_plan;
use super::BuildPushMode;

/// BuildKit frontend type for buildctl
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum BuildctlFrontend {
    /// Standard Dockerfile frontend (dockerfile.v0)
    Dockerfile,
    /// Railpack gateway frontend (gateway.v0 + railpack-frontend)
    Railpack,
}

/// Options for building with Railpacks
pub(crate) struct RailpackBuildOptions<'a> {
    pub app_path: &'a str,
    pub image_tag: &'a str,
    pub container_cli: &'a str,
    pub buildx_supports_push: bool,
    pub use_buildctl: bool,
    pub push_mode: BuildPushMode,
    pub buildkit_host: Option<&'a str>,
    pub env: &'a [String],
    pub no_cache: bool,
    pub platform: &'a str,
    pub output: super::CommandOutput,
}

/// RAII guard for cleaning up temp files and directories
struct CleanupGuard {
    path: std::path::PathBuf,
    is_directory: bool,
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        if self.path.exists() {
            if self.is_directory {
                let _ = std::fs::remove_dir_all(&self.path);
                debug!("Cleaned up temp directory: {}", self.path.display());
            } else {
                let _ = std::fs::remove_file(&self.path);
                debug!("Cleaned up temp file: {}", self.path.display());
            }
        }
    }
}

/// Build image with Railpacks
pub(crate) fn build_image_with_railpacks(options: RailpackBuildOptions) -> Result<()> {
    // Check railpack CLI availability
    let railpack_check = Command::new("railpack").arg("--version").output();
    if railpack_check.is_err() {
        bail!(
            "railpack CLI not found. Ensure the railpack CLI is installed and available in PATH.\n\
             In production, this should be available in the rise-builder image."
        );
    }

    // Create .railpack-build directory in app_path
    let build_dir = Path::new(options.app_path).join(".railpack-build");
    let dir_existed = build_dir.exists();

    if !dir_existed {
        fs::create_dir(&build_dir).with_context(|| {
            format!("Failed to create build directory: {}", build_dir.display())
        })?;
    }

    let plan_file = build_dir.join("railpack-plan.json");
    let info_file = build_dir.join("info.json");

    // Set up cleanup guards
    // If we created the directory, clean up the entire directory
    // Otherwise, just clean up the individual files
    let _cleanup_guard = if !dir_existed {
        CleanupGuard {
            path: build_dir,
            is_directory: true,
        }
    } else {
        // When directory existed, we'll clean up files individually
        // Store the first file in the guard, we'll use a separate guard for the second
        CleanupGuard {
            path: plan_file.clone(),
            is_directory: false,
        }
    };

    let _info_guard = if dir_existed {
        Some(CleanupGuard {
            path: info_file.clone(),
            is_directory: false,
        })
    } else {
        None
    };

    // Read proxy vars and parse user-provided env vars before railpack prepare
    let mut all_secrets = proxy::read_and_transform_proxy_vars();
    let user_env_vars = proxy::parse_env_vars(options.env)?;
    all_secrets.extend(user_env_vars);

    // Add SSL env vars before railpack prepare so they are declared in the plan
    // and exposed as secrets during build-time RUN steps.
    if let Some(ssl_cert_file) = super::env_var_non_empty("SSL_CERT_FILE") {
        if Path::new(&ssl_cert_file).exists() {
            let ssl_cert_target = super::ssl::SSL_CERT_PATHS[0];
            for var in super::ssl::SSL_ENV_VARS {
                all_secrets
                    .entry(var.to_string())
                    .or_insert_with(|| ssl_cert_target.to_string());
            }
        }
    }

    info!("Running railpack prepare for: {}", options.app_path);

    // Run railpack prepare with --env flags so secrets are declared in the plan
    // (this enables railpack's secrets-hash cache invalidation mechanism)
    let mut cmd = Command::new("railpack");
    cmd.arg("prepare")
        .arg(options.app_path)
        .arg("--plan-out")
        .arg(&plan_file)
        .arg("--info-out")
        .arg(&info_file);

    for key in all_secrets.keys() {
        cmd.arg("--env")
            .arg(format!("{}={}", key, all_secrets[key]));
    }

    // Log command with redacted secret values
    if tracing::enabled!(tracing::Level::DEBUG) {
        let redacted_env: Vec<String> = all_secrets
            .keys()
            .map(|k| format!("--env {}=<redacted>", k))
            .collect();
        debug!(
            "Executing: railpack prepare {} --plan-out {} --info-out {} {}",
            options.app_path,
            plan_file.display(),
            info_file.display(),
            redacted_env.join(" ")
        );
    }

    let status = super::run_command(cmd, "Failed to execute railpack prepare", options.output)?;

    if !status.success() {
        bail!("railpack prepare failed with status: {}", status);
    }

    // Verify plan file was created
    if !plan_file.exists() {
        bail!(
            "railpack prepare did not create plan file at {}",
            plan_file.display()
        );
    }

    info!("✓ Railpack prepare completed");

    // Embed SSL certificate if SSL_CERT_FILE is set
    if let Some(ssl_cert_file) = super::env_var_non_empty("SSL_CERT_FILE") {
        let cert_path = Path::new(&ssl_cert_file);
        if cert_path.exists() {
            embed_ssl_cert_in_plan(&plan_file, cert_path)?;
        } else {
            warn!(
                "SSL_CERT_FILE set to '{}' but file not found",
                ssl_cert_file
            );
        }
    }

    // Debug log plan contents
    if let Ok(plan_contents) = fs::read_to_string(&plan_file) {
        debug!("Railpack plan.json contents:\n{}", plan_contents);
    }

    // Build with buildx or buildctl
    if options.use_buildctl {
        build_with_buildctl(
            options.app_path,
            &plan_file,
            options.image_tag,
            options.push_mode == BuildPushMode::Inline,
            options.buildkit_host,
            &all_secrets,
            &HashMap::new(), // No local contexts for Railpack
            BuildctlFrontend::Railpack,
            options.no_cache,
            options.container_cli,
            options.platform,
            options.output,
        )?;
    } else {
        build_with_buildx(
            options.app_path,
            &plan_file,
            options.image_tag,
            options.container_cli,
            options.buildx_supports_push,
            options.push_mode,
            options.buildkit_host,
            &all_secrets,
            options.no_cache,
            options.platform,
            options.output,
        )?;
    }

    Ok(())
}

/// Build with docker buildx
#[allow(clippy::too_many_arguments)]
fn build_with_buildx(
    app_path: &str,
    plan_file: &Path,
    image_tag: &str,
    container_cli: &str,
    buildx_supports_push: bool,
    push_mode: BuildPushMode,
    buildkit_host: Option<&str>,
    secrets: &HashMap<String, String>,
    no_cache: bool,
    platform: &str,
    output: super::CommandOutput,
) -> Result<()> {
    // Check buildx availability
    if !super::docker::is_buildx_available(container_cli) {
        bail!(
            "{} buildx not available. Install buildx or use railpack:buildctl backend instead.",
            container_cli
        );
    }

    info!(
        "Building image with {} buildx: {}",
        container_cli, image_tag
    );

    // If buildkit_host is provided, we need to create/use a builder pointing to it
    let builder_name = if let Some(host) = buildkit_host {
        Some(ensure_buildx_builder(container_cli, host, output)?)
    } else {
        None
    };

    let mut cmd = Command::new(container_cli);
    cmd.arg("buildx")
        .arg("build")
        .arg("--build-arg")
        .arg("BUILDKIT_SYNTAX=ghcr.io/railwayapp/railpack-frontend")
        .arg("-f")
        .arg(plan_file)
        .arg("-t")
        .arg(image_tag)
        .arg("--platform")
        .arg(platform);

    // Use the managed builder if available
    if let Some(ref builder) = builder_name {
        cmd.arg("--builder").arg(builder);
    }

    // Add no-cache flag if requested
    if no_cache {
        cmd.arg("--no-cache");
    }

    let needs_fallback_push =
        super::docker::configure_buildx_output(&mut cmd, push_mode, buildx_supports_push);

    // Resolve host gateway IP and rewrite proxy URLs in secrets.
    // Prefer the BuildKit container name from BUILDKIT_HOST (docker-container://...)
    // over the builder name, since they may differ.
    let buildkit_host_env = std::env::var("BUILDKIT_HOST").ok();
    let container_name = buildkit_host_env
        .as_deref()
        .and_then(|h| h.strip_prefix("docker-container://"))
        .or(builder_name.as_deref());

    let effective_secrets = super::buildkit::resolve_and_apply_host_gateway(
        &mut cmd,
        container_cli,
        secrets,
        container_name,
        buildkit_host_env.is_some(),
    );

    // Add secrets via prefixed env vars so the docker CLI keeps its original
    // proxy vars while build containers get the transformed values.
    proxy::add_secrets_to_command(&mut cmd, &effective_secrets);

    cmd.arg(app_path);

    debug!("Executing command: {:?}", cmd);

    let status = super::run_command(
        cmd,
        &format!("Failed to execute {} buildx build", container_cli),
        output,
    )?;

    if !status.success() {
        bail!(
            "{} buildx build failed with status: {}",
            container_cli,
            status
        );
    }

    if needs_fallback_push {
        docker_push_with_output(container_cli, image_tag, output)?;
    }

    Ok(())
}

/// Build with buildctl
///
/// Supports both Dockerfile and Railpack frontends:
/// - Dockerfile: Uses `--frontend=dockerfile.v0` for standard Dockerfiles
/// - Railpack: Uses `--frontend=gateway.v0` with railpack-frontend
///
/// The `secrets` HashMap contains environment variable secrets:
/// - key: environment variable name
/// - value: the actual secret value (passed to the build via prefixed env vars)
///
/// The `local_contexts` HashMap contains named build contexts:
/// - key: context name (e.g., "rise-internal-ssl-cert")
/// - value: local path to the context directory
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_with_buildctl(
    app_path: &str,
    dockerfile_or_plan: &Path,
    image_tag: &str,
    push: bool,
    buildkit_host: Option<&str>,
    secrets: &HashMap<String, String>,
    local_contexts: &HashMap<String, String>,
    frontend: BuildctlFrontend,
    no_cache: bool,
    container_cli: &str,
    platform: &str,
    output: super::CommandOutput,
) -> Result<()> {
    // Check buildctl availability
    let buildctl_check = Command::new("buildctl").arg("--version").output();
    if buildctl_check.is_err() {
        bail!("buildctl not found. Install buildctl or use docker:buildx backend instead.");
    }

    info!("Building image with buildctl: {}", image_tag);

    let mut cmd = Command::new("buildctl");
    cmd.arg("build")
        .arg("--local")
        .arg(format!("context={}", app_path))
        .arg("--local")
        .arg(format!(
            "dockerfile={}",
            dockerfile_or_plan
                .parent()
                .unwrap_or(Path::new(app_path))
                .display()
        ));

    // Set frontend based on type
    match frontend {
        BuildctlFrontend::Dockerfile => {
            cmd.arg("--frontend=dockerfile.v0");
            // Add opt for filename if not the default "Dockerfile"
            if let Some(filename) = dockerfile_or_plan.file_name() {
                let filename_str = filename.to_string_lossy();
                if filename_str != "Dockerfile" {
                    cmd.arg("--opt").arg(format!("filename={}", filename_str));
                }
            }
        }
        BuildctlFrontend::Railpack => {
            cmd.arg("--frontend=gateway.v0")
                .arg("--opt")
                .arg("source=ghcr.io/railwayapp/railpack-frontend");
        }
    }

    // Set BUILDKIT_HOST if provided
    if let Some(host) = buildkit_host {
        cmd.env("BUILDKIT_HOST", host);
    }

    // Add local contexts (named build contexts).
    // Each named context needs both --local (to register the source)
    // and --opt context:<name>=local:<name> (to map it for the Dockerfile frontend).
    for (name, path) in local_contexts {
        cmd.arg("--local").arg(format!("{}={}", name, path));
        cmd.arg("--opt")
            .arg(format!("context:{}=local:{}", name, name));
    }

    // Add secrets via prefixed env vars so the CLI keeps its original
    // proxy vars while build containers get the transformed values.
    proxy::add_secrets_to_command(&mut cmd, secrets);

    // Disable build cache via frontend option (buildctl has no --no-cache flag)
    if no_cache {
        cmd.arg("--opt").arg("no-cache=");
    }

    // --output must be last: its value is the next positional arg
    if push {
        cmd.arg("--output").arg(format!(
            "type=image,name={},push=true,platform={}",
            image_tag, platform
        ));

        debug!("Executing command: {:?}", cmd);

        let status = super::run_command(cmd, "Failed to execute buildctl build", output)?;
        if !status.success() {
            bail!("buildctl build failed with status: {}", status);
        }
    } else {
        // Output as docker tar stream and pipe into `docker load` so the
        // image is available in the local Docker daemon.
        cmd.arg("--output").arg(format!(
            "type=docker,name={},platform={}",
            image_tag, platform
        ));
        cmd.stdout(std::process::Stdio::piped());

        debug!("Executing command: {:?} | {} load", cmd, container_cli);

        let mut buildctl_child = cmd.spawn().context("Failed to execute buildctl build")?;
        let buildctl_stdout = buildctl_child
            .stdout
            .take()
            .context("Failed to capture buildctl stdout")?;

        let mut docker_load_command = Command::new(container_cli);
        docker_load_command.arg("load").stdin(buildctl_stdout);
        let docker_load = super::run_command(
            docker_load_command,
            &format!("Failed to execute {} load", container_cli),
            output,
        )?;

        let buildctl_status = buildctl_child
            .wait()
            .context("Failed to wait for buildctl")?;
        if !buildctl_status.success() {
            bail!("buildctl build failed with status: {}", buildctl_status);
        }
        if !docker_load.success() {
            bail!("{} load failed with status: {}", container_cli, docker_load);
        }
    }

    Ok(())
}
