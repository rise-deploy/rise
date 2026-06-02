// Local development runner - builds and runs container images locally

use anyhow::{bail, Context, Result};
use reqwest::Client;
use std::process::{Command, Stdio};
use tracing::{info, warn};

use crate::build::{self, BuildOptions};
use crate::cli::env;
use crate::config::Config;
use crate::rise_toml::ResolvedContainer;

fn optional_project_env_token(token_result: anyhow::Result<String>) -> Result<Option<String>> {
    match token_result {
        Ok(token) => Ok(Some(token)),
        Err(e) if crate::token_source::is_no_token_source_error(&e) => Ok(None),
        Err(e) => Err(e).context("Failed to resolve token for project environment variables"),
    }
}

/// Options for running a container locally
pub struct RunOptions<'a> {
    pub project_name: Option<&'a str>,
    /// When the project declares a `[containers]` table, selects which container
    /// to run. Required for multi-container projects (use `rise compose up` to
    /// run them all together). Must be `None` for single-container projects.
    pub container: Option<&'a str>,
    pub use_project_env: bool,
    pub path: &'a str,
    pub environment: Option<&'a str>,
    pub http_port: u16,
    /// Host port to publish. `None` defaults to the effective container port.
    pub expose: Option<u16>,
    pub run_env: &'a [(String, String)],
    pub build_args: &'a build::BuildArgs,
}

/// Build and run a container image locally for development
pub async fn run_locally(
    http_client: &Client,
    config: &Config,
    options: RunOptions<'_>,
) -> Result<()> {
    let backend_url = config.get_backend_url();

    // Load the project config once — used for container resolution and to fall
    // back to the rise.toml `[project]` name when `--project` is not passed.
    let toml_config = match build::config::load_full_project_config(options.path) {
        Ok(cfg) => cfg,
        Err(e) => {
            warn!("Failed to load rise.toml: {}", e);
            None
        }
    };

    // Resolve the multi-container layout (empty for single-container projects).
    let resolved = match &toml_config {
        Some(cfg) => cfg.resolve_deploy().map_err(|e| anyhow::anyhow!(e))?,
        None => Default::default(),
    };

    // Resolve project name: explicit `--project` wins, else rise.toml `[project]`.
    let project_name = options.project_name.map(str::to_string).or_else(|| {
        toml_config
            .as_ref()
            .and_then(|c| c.project.as_ref())
            .map(|p| p.name.clone())
    });

    // Pick what to run. `None` means the single-container / top-level `[build]`
    // path; `Some(container)` means a specific `[containers.X]` entry.
    let selected = select_run_target(options.container, &resolved, project_name.as_deref())?;

    // The effective port a routed container listens on drives both PORT and the
    // host mapping. A selected container's own `port` wins over `--http-port`.
    let effective_http_port = selected
        .as_ref()
        .and_then(|c| c.port)
        .unwrap_or(options.http_port);
    let expose = options.expose.unwrap_or(effective_http_port);

    // Resolve container CLI with the same precedence the build path uses (flag →
    // RISE_CONTAINER_CLI → rise.toml [build].container_cli → global config /
    // auto-detect), so a podman-configured project both builds and runs with podman.
    let container_cli = crate::cli::compose::resolve_container_cli(
        config,
        options.build_args,
        toml_config.as_ref(),
    );

    // Build (or reference, for pre-built images) the image to run.
    let image_tag = match &selected {
        Some(container) => {
            if let Some(image) = &container.image {
                info!(
                    "Container '{}' uses pre-built image '{}'",
                    container.name, image
                );
                image.clone()
            } else {
                let tag = local_image_tag(project_name.as_deref(), Some(&container.name));
                info!("Building image locally: {}", tag);
                // Feed from_build_args a config view with this container's
                // `[build]` hoisted to the top level (mirrors the deploy path in
                // src/cli/deployment/core.rs).
                let mut per_container = toml_config.clone().unwrap_or_default();
                per_container.build = container.build.clone();
                let build_options = BuildOptions::from_build_args(
                    config,
                    tag.clone(),
                    options.path.to_string(),
                    options.build_args,
                    Some(per_container),
                    None,
                )
                .with_push(false);
                build::build_image(build_options)?;
                tag
            }
        }
        None => {
            let tag = local_image_tag(project_name.as_deref(), None);
            info!("Building image locally: {}", tag);
            let build_options = BuildOptions::from_build_args(
                config,
                tag.clone(),
                options.path.to_string(),
                options.build_args,
                None,
                None,
            )
            .with_push(false);
            build::build_image(build_options)?;
            tag
        }
    };

    // Assemble the runtime environment. docker/podman apply repeated `-e` flags
    // last-wins, so we push in ascending precedence: project preview vars, then
    // container-scoped overrides, then PORT, then CLI `--env`.
    let mut run_env: Vec<(String, String)> = Vec::new();

    if options.use_project_env {
        if let Some(project_name) = &project_name {
            match optional_project_env_token(
                crate::token_source::resolve_token_with_retry(http_client, config).await,
            )? {
                Some(token) => match env::fetch_preview_env_vars(
                    http_client,
                    &backend_url,
                    &token,
                    project_name,
                    "default",
                    options.environment,
                )
                .await
                {
                    Ok((loadable_vars, protected_keys)) => {
                        if !loadable_vars.is_empty() {
                            info!(
                                "Loading {} environment variable{} from project '{}'",
                                loadable_vars.len(),
                                if loadable_vars.len() == 1 { "" } else { "s" },
                                project_name
                            );
                            for (key, value) in loadable_vars {
                                // PORT is set below — `--http-port` / container
                                // port always takes precedence.
                                if key == "PORT" {
                                    continue;
                                }
                                run_env.push((key, value));
                            }
                        }

                        // Warn about protected secret variables that cannot be loaded
                        if !protected_keys.is_empty() {
                            warn!(
                                "Project '{}' has {} protected secret{} that cannot be loaded locally:",
                                project_name,
                                protected_keys.len(),
                                if protected_keys.len() == 1 { "" } else { "s" }
                            );
                            for key in &protected_keys {
                                warn!("  - {}", key);
                            }
                            warn!("These secrets are provisioned automatically during deployment");
                        }
                    }
                    Err(e) => {
                        warn!(
                            "Failed to fetch environment variables from project '{}': {}",
                            project_name, e
                        );
                        warn!("Continuing without project environment variables");
                    }
                },
                None => {
                    warn!("No usable token source - cannot load project environment variables");
                    warn!("Run 'rise login' or configure a CI token source to authenticate");
                }
            }
        }
    }

    // Container-scoped env overrides from `[containers.X.env]`.
    if let Some(container) = &selected {
        for (key, value) in &container.env {
            run_env.push((key.clone(), value.clone()));
        }
    }

    // PORT always reflects the effective container port.
    run_env.push(("PORT".to_string(), effective_http_port.to_string()));

    // The controller always injects RISE_CONTAINER (the container's own name);
    // a single-container project is the implicit container named "app".
    let container_label = selected
        .as_ref()
        .map(|c| c.name.as_str())
        .unwrap_or(crate::rise_toml::DEFAULT_CONTAINER_NAME);
    run_env.push(("RISE_CONTAINER".to_string(), container_label.to_string()));

    // User-specified runtime env vars take final precedence.
    if !options.run_env.is_empty() {
        info!(
            "Setting {} runtime environment variable{}",
            options.run_env.len(),
            if options.run_env.len() == 1 { "" } else { "s" }
        );
        run_env.extend(options.run_env.iter().cloned());
    }

    if options.use_project_env && project_name.is_some() {
        info!("Project environment variables loaded (including extension vars)");
    }

    run_container(RunContainerSpec {
        container_cli: &container_cli,
        image_tag: &image_tag,
        expose,
        http_port: effective_http_port,
        env: &run_env,
    })
}

/// Inputs for a single `docker run` / `podman run` invocation.
struct RunContainerSpec<'a> {
    container_cli: &'a str,
    image_tag: &'a str,
    expose: u16,
    http_port: u16,
    env: &'a [(String, String)],
}

/// Run a single container image interactively, inheriting stdio.
fn run_container(spec: RunContainerSpec<'_>) -> Result<()> {
    info!("Starting container with {}...", spec.container_cli);

    let mut cmd = Command::new(spec.container_cli);
    cmd.arg("run")
        .arg("--rm")
        .arg("-it")
        .arg("-p")
        .arg(format!("{}:{}", spec.expose, spec.http_port))
        .arg("--add-host=host.docker.internal:host-gateway");

    for (key, value) in spec.env {
        cmd.arg("-e").arg(format!("{}={}", key, value));
    }

    cmd.arg(spec.image_tag);

    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    info!(
        "Running container: {} (port {}:{}, PORT={})",
        spec.image_tag, spec.expose, spec.http_port, spec.http_port
    );
    info!(
        "Application will be available at http://localhost:{}",
        spec.expose
    );
    info!("Press Ctrl+C to stop the container");

    let status = cmd.status().context("Failed to run container")?;

    if !status.success() {
        if let Some(code) = status.code() {
            bail!("Container exited with status code: {}", code);
        } else {
            bail!("Container was terminated by a signal");
        }
    }

    Ok(())
}

/// Decide which container `rise run` should run.
///
/// - Single-container project (no `[containers]`): returns `None`. Passing
///   `--container` is an error.
/// - Multi-container project: `--container <name>` is required and must name an
///   existing container; otherwise we point the user at `rise compose up`.
fn select_run_target(
    requested: Option<&str>,
    resolved: &crate::rise_toml::ResolvedDeploy,
    project_label: Option<&str>,
) -> Result<Option<ResolvedContainer>> {
    if resolved.containers.is_empty() {
        if requested.is_some() {
            bail!(
                "--container was given but this project has no [containers] table. \
                 Drop --container to run the single-container build."
            );
        }
        return Ok(None);
    }

    let names = || {
        resolved
            .containers
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };

    match requested {
        Some(name) => resolved
            .containers
            .iter()
            .find(|c| c.name == name)
            .cloned()
            .map(Some)
            .ok_or_else(|| {
                anyhow::anyhow!("container '{}' not found. Available: {}", name, names())
            }),
        None => bail!(
            "'{}' is a multi-container project. Run a single container with \
             `rise run --container <name>` (one of: {}), or run them all together \
             with `rise compose up`.",
            project_label.unwrap_or("this project"),
            names()
        ),
    }
}

/// Local image tag for a build: `rise-local-<project>[-<container>]`.
///
/// Delegates to the shared [`crate::cli::compose::local_image_tag`] so that a
/// `rise run --container` and a prior `rise compose up` resolve to the same
/// image. Only the `None` project default (`app`) is handled here.
fn local_image_tag(project_name: Option<&str>, container: Option<&str>) -> String {
    crate::cli::compose::local_image_tag(project_name.unwrap_or("app"), container)
}

#[cfg(test)]
mod tests {
    use super::optional_project_env_token;
    use crate::token_source::TokenSourceError;

    #[test]
    fn project_env_token_continues_when_no_token_source_exists() {
        let result = optional_project_env_token(Err(TokenSourceError::NoSource(
            "Not authenticated".to_string(),
        )
        .into()))
        .unwrap();

        assert_eq!(result, None);
    }

    #[test]
    fn project_env_token_errors_when_configured_token_source_fails() {
        let err = optional_project_env_token(Err(TokenSourceError::NonRetryable(
            "bad token config".to_string(),
        )
        .into()))
        .unwrap_err()
        .to_string();

        assert!(err.contains("Failed to resolve token for project environment variables"));
    }
}
