//! `rise compose` — run a Rise project locally via Docker Compose.
//!
//! This module builds project containers locally and generates a Compose file
//! that wires them together on a shared network, mirroring production:
//!
//! - single-container projects run as the implicit `app` container;
//! - multi-container siblings reach each other by service name, and each gets
//!   the same `RISE_CONTAINER_HOST__<NAME>` env vars the deployed app would see;
//! - path-based `[routes]` are replicated by a Traefik router that discovers
//!   routing rules from Docker **labels** (no mounted config file), published
//!   on a single host port.
//!
//! `up`/`down` manage an ephemeral stack (no file left on disk); `generate`
//! writes a `compose.yaml` users can customize and run themselves.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde::Serialize;
use tracing::{info, warn};

use crate::build::{self, BuildOptions};
use crate::cli::env;
use crate::config::Config;
use crate::rise_toml::{ImplicitAppContainer, ProjectBuildConfig, ResolvedDeploy};

/// Default Traefik image used for the local router service.
const TRAEFIK_IMAGE: &str = "traefik:v3.7.4";

// ── Compose data model (a small, serializable subset of the spec) ──────────

#[derive(Debug, Serialize, PartialEq, Eq)]
struct ComposeFile {
    services: BTreeMap<String, Service>,
}

#[derive(Debug, Serialize, Default, PartialEq, Eq)]
struct Service {
    image: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    command: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    ports: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    environment: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    labels: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    volumes: Vec<String>,
}

// ── Pure generator ─────────────────────────────────────────────────────────

/// Build the Compose model for a resolved project.
///
/// `shared_env` is the project-level environment (e.g. the deployment preview
/// vars); per-container `[containers.X.env]` overrides are layered on top.
fn build_compose(
    project_name: &str,
    resolved: &ResolvedDeploy,
    shared_env: &BTreeMap<String, String>,
    router_port: u16,
) -> ComposeFile {
    let mut services: BTreeMap<String, Service> = BTreeMap::new();

    // `RISE_CONTAINER_HOST__<NAME>` is only injected for ≥2 containers, matching
    // the controller. Locally the value is `<service>:<port>` — Compose service
    // DNS resolves the bare name (production prefixes the deployment group).
    let host_vars: BTreeMap<String, String> = if resolved.containers.len() >= 2 {
        resolved
            .containers
            .iter()
            .filter_map(|c| {
                let port = c.port?;
                Some((
                    format!(
                        "RISE_CONTAINER_HOST__{}",
                        c.name.to_uppercase().replace('-', "_")
                    ),
                    format!("{}:{}", c.name, port),
                ))
            })
            .collect()
    } else {
        BTreeMap::new()
    };

    for container in &resolved.containers {
        // Injected host vars are the base layer: project-level (`shared_env`)
        // and per-container env override them. Mirrors the reconciler, which
        // skips an injected host var when a global env var of the same name
        // already exists (see webhook.rs).
        let mut environment = host_vars.clone();
        environment.extend(shared_env.clone());
        // Per-container overrides win over project-level vars.
        for (k, v) in &container.env {
            environment.insert(k.clone(), v.clone());
        }
        if let Some(port) = container.port {
            // Matches the reconciler: each port-having container gets PORT set
            // to its own declared port; workers (no `port`) keep whatever
            // deployment-wide PORT came in via `shared_env`, if any.
            environment.insert("PORT".to_string(), port.to_string());
        }
        // The controller always injects RISE_CONTAINER (the container's own name).
        environment.insert("RISE_CONTAINER".to_string(), container.name.clone());

        let image = container
            .image
            .clone()
            .unwrap_or_else(|| local_image_tag(project_name, Some(&container.name)));

        // Traefik labels for every route targeting this container.
        let mut labels = Vec::new();
        let routes: Vec<&crate::rise_toml::ResolvedRoute> = resolved
            .routes
            .iter()
            .filter(|r| r.container == container.name)
            .collect();
        if let (Some(port), false) = (container.port, routes.is_empty()) {
            labels.push("traefik.enable=true".to_string());
            for route in &routes {
                let router = router_name(&container.name, &route.path);
                labels.push(format!(
                    "traefik.http.routers.{router}.rule=PathPrefix(`{}`)",
                    route.path
                ));
                // Longest-prefix-first, matching the controller's route sort.
                labels.push(format!(
                    "traefik.http.routers.{router}.priority={}",
                    route.path.len()
                ));
                labels.push(format!(
                    "traefik.http.routers.{router}.service={}",
                    container.name
                ));
            }
            labels.push(format!(
                "traefik.http.services.{}.loadbalancer.server.port={}",
                container.name, port
            ));
        }

        services.insert(
            container.name.clone(),
            Service {
                image,
                environment,
                labels,
                ..Default::default()
            },
        );
    }

    // Add the Traefik router only when there is something to route.
    if resolved.routes.iter().any(|r| {
        resolved
            .containers
            .iter()
            .any(|c| c.name == r.container && c.port.is_some())
    }) {
        services.insert(
            "rise-router".to_string(),
            Service {
                image: TRAEFIK_IMAGE.to_string(),
                command: vec![
                    "--providers.docker=true".to_string(),
                    "--providers.docker.exposedbydefault=false".to_string(),
                    "--entrypoints.web.address=:80".to_string(),
                ],
                ports: vec![format!("{}:80", router_port)],
                environment: BTreeMap::from([(
                    "DOCKER_API_VERSION".to_string(),
                    "1.44".to_string(),
                )]),
                volumes: vec!["/var/run/docker.sock:/var/run/docker.sock:ro".to_string()],
                ..Default::default()
            },
        );
    }

    ComposeFile { services }
}

/// Local image tag for a build: `rise-local-<project>[-<container>]`.
///
/// Shared by `rise compose` and `rise run` so that `compose up` builds and a
/// later `rise run --container` resolve to the same image. The two call sites
/// MUST stay byte-identical — keep this the single source of truth.
pub(crate) fn local_image_tag(project_name: &str, container: Option<&str>) -> String {
    let project = project_name.replace(['/', ':'], "-");
    match container {
        Some(c) => format!("rise-local-{}-{}", project, c),
        None => format!("rise-local-{}", project),
    }
}

/// Deterministic, Compose-safe Traefik router name for a (container, path) pair.
///
/// The readable suffix maps every non-alphanumeric character to `-`, so distinct
/// paths can collapse to the same suffix (e.g. `/api-v1`, `/api/v1`, `/api.v1`
/// and `/api_v1` all yield `api-v1`). A short stable hash of the raw path is
/// appended to keep names unique — otherwise the colliding routers would share
/// `traefik.http.routers.<name>.*` labels and one route would silently clobber
/// the other.
fn router_name(container: &str, path: &str) -> String {
    let suffix: String = path
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let suffix = suffix.trim_matches('-');
    let hash = short_path_hash(path);
    if suffix.is_empty() {
        format!("{container}-root-{hash}")
    } else {
        format!("{container}-{suffix}-{hash}")
    }
}

/// Short, stable hex hash (FNV-1a, 32-bit) of a route path, used only to
/// disambiguate router names that would otherwise collide.
fn short_path_hash(path: &str) -> String {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in path.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("{hash:08x}")
}

/// Compose project name (`-p`): stable across up/down so `down` matches `up`.
fn compose_project_name(project_name: &str) -> String {
    let sanitized: String = project_name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("rise-{}", sanitized.trim_matches('-'))
}

// ── Shared resolution helpers ────────────────────────────────────────────

struct Resolved {
    toml_config: ProjectBuildConfig,
    resolved: ResolvedDeploy,
    project_name: String,
}

/// Load rise.toml and resolve the container layout for Compose.
///
/// Multi-container projects use their explicit `[containers]` table. A
/// single-container project is represented as the same implicit `app` container
/// that the backend and `rise run` use, with the CLI `--http-port` value as its
/// local port.
fn load_compose_project(
    path: &str,
    explicit_project: Option<&str>,
    single_container_http_port: u16,
) -> Result<Resolved> {
    let toml_config = build::config::load_full_project_config(path)?
        .ok_or_else(|| anyhow::anyhow!("no rise.toml found in '{}'", path))?;

    let mut resolved = toml_config
        .resolve_deploy()
        .map_err(|e| anyhow::anyhow!(e))?;
    if resolved.containers.is_empty() {
        let deploy = toml_config.deploy.clone().unwrap_or_default();
        resolved = ResolvedDeploy::implicit_app(ImplicitAppContainer {
            build: toml_config.build.clone(),
            port: single_container_http_port,
            replicas: deploy.replicas,
            cpu: deploy.cpu,
            memory: deploy.memory,
            health_check: deploy.health_check,
            ..Default::default()
        });
    }

    let project_name = explicit_project
        .map(str::to_string)
        .or_else(|| toml_config.project.as_ref().map(|p| p.name.clone()))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "could not determine project name; set [project] name in rise.toml or pass --project"
            )
        })?;

    Ok(Resolved {
        toml_config,
        resolved,
        project_name,
    })
}

/// Resolve just the project name (and the loaded toml, if any) without
/// requiring a valid `[containers]` table.
///
/// `down`/`ps`/`logs` operate on an already-running stack, so they shouldn't
/// fail just because the user edited rise.toml (e.g. emptied `[containers]`)
/// after `up`. Explicit `--project` wins; otherwise we fall back to rise.toml
/// `[project].name`. Errors only if neither is available.
fn resolve_project_name(
    path: &str,
    explicit_project: Option<&str>,
) -> Result<(Option<ProjectBuildConfig>, String)> {
    let toml_config = build::config::load_full_project_config(path)?;

    let project_name = explicit_project
        .map(str::to_string)
        .or_else(|| {
            toml_config
                .as_ref()
                .and_then(|c| c.project.as_ref())
                .map(|p| p.name.clone())
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "could not determine project name; set [project] name in rise.toml or pass --project"
            )
        })?;

    Ok((toml_config, project_name))
}

/// Local runtimes have one image store per container CLI. `rise compose` and
/// `rise run --container` both build and run from one runtime, so per-container
/// `container_cli` would build into one daemon and run from another.
pub(crate) fn reject_per_container_container_cli(
    toml_config: &ProjectBuildConfig,
    selected_container: Option<&str>,
) -> Result<()> {
    for (name, container) in &toml_config.containers {
        if selected_container.is_some_and(|selected| selected != name) {
            continue;
        }
        if container
            .build
            .as_ref()
            .and_then(|build| build.container_cli.as_ref())
            .is_some()
        {
            bail!(
                "per-container build.container_cli is not supported for local development \
                 ([containers.{name}.build]). Set top-level [build].container_cli or pass \
                 --container-cli so all local containers use one runtime."
            );
        }
    }
    Ok(())
}

/// Fetch project preview env vars. Missing credentials (not logged in, no CI
/// token source) degrade gracefully — warn and return an empty map so
/// `generate` keeps working offline — but a token source that exists and
/// *fails* is a hard error, matching `rise run`: silently starting a stack
/// without its project env (OAuth secrets etc.) is a confusing failure mode.
/// A failing preview fetch itself stays best-effort, also matching `rise run`.
async fn fetch_shared_env(
    http_client: &Client,
    config: &Config,
    project_name: &str,
    environment: Option<&str>,
) -> Result<BTreeMap<String, String>> {
    let token = match crate::token_source::resolve_token_with_retry(http_client, config).await {
        Ok(token) => token,
        Err(e) if crate::token_source::is_no_token_source_error(&e) => {
            warn!("Not logged in — continuing without project environment variables");
            warn!("Run 'rise login' or configure a CI token source to authenticate");
            return Ok(BTreeMap::new());
        }
        Err(e) => {
            return Err(e).context("Failed to resolve token for project environment variables");
        }
    };
    let env_vars = match env::fetch_preview_env_vars(
        http_client,
        &config.get_backend_url(),
        &token,
        project_name,
        "default",
        environment,
    )
    .await
    {
        Ok((loadable, protected)) => {
            if !protected.is_empty() {
                warn!(
                    "{} protected secret(s) cannot be loaded locally: {}",
                    protected.len(),
                    protected.join(", ")
                );
            }
            loadable.into_iter().collect()
        }
        Err(e) => {
            warn!("Failed to load project environment variables: {:?}", e);
            BTreeMap::new()
        }
    };
    Ok(env_vars)
}

/// Build every local container image (push=false).
///
/// Pre-built (`image`) containers are referenced verbatim and skipped. An
/// implicit single-container app may have no explicit `[build]` table; it still
/// builds with the same defaults as `rise run`.
fn build_all_local(
    config: &Config,
    path: &str,
    build_args: &build::BuildArgs,
    project_name: &str,
    res: &Resolved,
) -> Result<()> {
    for container in &res.resolved.containers {
        if let Some(image) = &container.image {
            info!(
                "Container '{}' uses pre-built image '{}', skipping build",
                container.name, image
            );
            continue;
        }
        let tag = local_image_tag(project_name, Some(&container.name));
        info!("Building container '{}' as {}", container.name, tag);

        let mut per_container = res.toml_config.clone();
        per_container.build = container.build.clone();
        let options = BuildOptions::from_build_args(
            config,
            tag,
            path.to_string(),
            build_args,
            Some(per_container),
            None,
        )
        .with_push(false);
        build::build_image(options)?;
    }
    Ok(())
}

/// Resolve the runtime container CLI command, mirroring the precedence the
/// build path uses (`BuildOptions::from_build_args`): `--container-cli` flag →
/// `RISE_CONTAINER_CLI` env → rise.toml `[build].container_cli` → global config
/// default (`Config::get_container_cli`, which reads the user's config and
/// otherwise auto-detects, preferring podman when docker is absent).
///
/// Shared by `rise compose` and `rise run` so a podman-configured project both
/// builds and runs with podman — including the global-config / auto-detect case,
/// not just an explicit flag/env/toml setting.
///
/// Note: only the *top-level* `[build].container_cli` is consulted. A
/// per-container `[containers.X.build].container_cli` affects that container's
/// image build, but there is exactly one runtime CLI for the whole stack.
pub(crate) fn resolve_container_cli(
    config: &Config,
    build_args: &build::BuildArgs,
    toml_config: Option<&ProjectBuildConfig>,
) -> String {
    build_args
        .container_cli
        .clone()
        .or_else(|| build::env_var_non_empty("RISE_CONTAINER_CLI"))
        .or_else(|| {
            toml_config
                .and_then(|c| c.build.as_ref())
                .and_then(|b| b.container_cli.clone())
        })
        .unwrap_or_else(|| config.get_container_cli().command().to_string())
}

// ── Command handlers ───────────────────────────────────────────────────────

/// Options for `rise compose generate`.
pub struct GenerateOptions<'a> {
    pub path: &'a str,
    pub project: Option<&'a str>,
    pub environment: Option<&'a str>,
    pub http_port: u16,
    pub router_port: u16,
    /// Write to stdout instead of a file.
    pub stdout: bool,
    /// Output file path (defaults to `<path>/compose.yaml`).
    pub output: Option<&'a str>,
}

/// Generate a persistent Compose file for a Rise project.
pub async fn generate(
    http_client: &Client,
    config: &Config,
    options: GenerateOptions<'_>,
) -> Result<()> {
    let res = load_compose_project(options.path, options.project, options.http_port)?;
    reject_per_container_container_cli(&res.toml_config, None)?;
    let shared_env =
        fetch_shared_env(http_client, config, &res.project_name, options.environment).await?;

    let compose = build_compose(
        &res.project_name,
        &res.resolved,
        &shared_env,
        options.router_port,
    );
    let yaml = serde_yaml::to_string(&compose).context("Failed to serialize compose file")?;

    if options.stdout {
        print!("{yaml}");
        return Ok(());
    }

    let out_path = options
        .output
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::Path::new(options.path).join("compose.yaml"));
    std::fs::write(&out_path, yaml)
        .with_context(|| format!("Failed to write {}", out_path.display()))?;
    info!("Wrote {}", out_path.display());
    info!(
        "Build images with `rise compose up`, then run this file with \
         `docker compose -f {} up`",
        out_path.display()
    );
    Ok(())
}

/// Options for `rise compose up`.
pub struct UpOptions<'a> {
    pub path: &'a str,
    pub project: Option<&'a str>,
    pub environment: Option<&'a str>,
    pub http_port: u16,
    pub router_port: u16,
    pub detach: bool,
    pub build_args: &'a build::BuildArgs,
}

/// Build the project locally and run it via `docker compose up`.
pub async fn up(http_client: &Client, config: &Config, options: UpOptions<'_>) -> Result<()> {
    let res = load_compose_project(options.path, options.project, options.http_port)?;
    reject_per_container_container_cli(&res.toml_config, None)?;
    let cli = resolve_container_cli(config, options.build_args, Some(&res.toml_config));
    let compose_project = compose_project_name(&res.project_name);

    build_all_local(
        config,
        options.path,
        options.build_args,
        &res.project_name,
        &res,
    )?;

    let shared_env =
        fetch_shared_env(http_client, config, &res.project_name, options.environment).await?;
    let compose = build_compose(
        &res.project_name,
        &res.resolved,
        &shared_env,
        options.router_port,
    );
    let yaml = serde_yaml::to_string(&compose).context("Failed to serialize compose file")?;

    // Ephemeral compose file — kept alive until after teardown, never persisted.
    let mut file = tempfile::Builder::new()
        .prefix("rise-compose-")
        .suffix(".yaml")
        .tempfile()
        .context("Failed to create temporary compose file")?;
    file.write_all(yaml.as_bytes())
        .context("Failed to write temporary compose file")?;
    let compose_path = file.path().to_path_buf();

    info!(
        "Starting {} container(s) with {}...",
        res.resolved.containers.len(),
        cli
    );
    let mut cmd = Command::new(&cli);
    cmd.arg("compose")
        .arg("-p")
        .arg(&compose_project)
        .arg("-f")
        .arg(&compose_path)
        .arg("up");
    if options.detach {
        cmd.arg("-d");
    }
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let status = cmd.status().context("Failed to run docker compose up")?;

    if options.detach {
        // Detached `up` returns immediately, so surface its failure here —
        // otherwise a bad compose file or missing daemon would be reported as
        // "Stack running" with a success exit code.
        if !status.success() {
            if let Some(code) = status.code() {
                bail!("docker compose up exited with status code: {}", code);
            }
            bail!("docker compose up was terminated by a signal");
        }
        info!(
            "Stack running. Stop it with `rise compose down`{}.",
            options
                .project
                .map(|p| format!(" --project {p}"))
                .unwrap_or_default()
        );
        return Ok(());
    }

    // Foreground exited (e.g. Ctrl-C) — tear the stack down so no stopped
    // containers or networks are left behind.
    info!("Tearing down stack...");
    let _ = Command::new(&cli)
        .arg("compose")
        .arg("-p")
        .arg(&compose_project)
        .arg("-f")
        .arg(&compose_path)
        .arg("down")
        .status();

    if !status.success() {
        if let Some(code) = status.code() {
            bail!("docker compose exited with status code: {}", code);
        }
        bail!("docker compose was terminated by a signal");
    }
    Ok(())
}

/// Options for `rise compose down`.
pub struct DownOptions<'a> {
    pub path: &'a str,
    pub project: Option<&'a str>,
    pub build_args: &'a build::BuildArgs,
}

/// Tear down the ephemeral stack started by `rise compose up`.
pub async fn down(config: &Config, options: DownOptions<'_>) -> Result<()> {
    // Tearing down an already-running stack only needs the project name, not a
    // fully valid `[containers]` config.
    let (toml_config, project_name) = resolve_project_name(options.path, options.project)?;
    let cli = resolve_container_cli(config, options.build_args, toml_config.as_ref());
    let compose_project = compose_project_name(&project_name);

    let status = Command::new(&cli)
        .arg("compose")
        .arg("-p")
        .arg(&compose_project)
        .arg("down")
        .status()
        .context("Failed to run docker compose down")?;

    if !status.success() {
        if let Some(code) = status.code() {
            bail!("docker compose down exited with status code: {}", code);
        }
        bail!("docker compose down was terminated by a signal");
    }
    info!("Stack '{}' torn down", compose_project);
    Ok(())
}

/// A `<cli> compose -p <project>` command with stdio inherited, ready for a
/// subcommand (`ps`, `logs`, …) to target a running stack by project name.
fn compose_for_project(cli: &str, project: &str) -> Command {
    let mut cmd = Command::new(cli);
    cmd.arg("compose")
        .arg("-p")
        .arg(project)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    cmd
}

/// Options for `rise compose ps`.
pub struct PsOptions<'a> {
    pub path: &'a str,
    pub project: Option<&'a str>,
    pub build_args: &'a build::BuildArgs,
}

/// List the containers of a running stack (`docker compose ps`).
pub async fn ps(config: &Config, options: PsOptions<'_>) -> Result<()> {
    // Inspecting a running stack only needs the project name.
    let (toml_config, project_name) = resolve_project_name(options.path, options.project)?;
    let cli = resolve_container_cli(config, options.build_args, toml_config.as_ref());
    let compose_project = compose_project_name(&project_name);

    let status = compose_for_project(&cli, &compose_project)
        .arg("ps")
        .status()
        .context("Failed to run docker compose ps")?;

    if !status.success() {
        if let Some(code) = status.code() {
            bail!("docker compose ps exited with status code: {}", code);
        }
        bail!("docker compose ps was terminated by a signal");
    }
    Ok(())
}

/// Options for `rise compose logs`.
pub struct LogsOptions<'a> {
    pub path: &'a str,
    pub project: Option<&'a str>,
    /// Stream new log output until interrupted.
    pub follow: bool,
    /// Number of lines to show from the end of each container's log.
    pub tail: Option<String>,
    /// Restrict output to these containers (compose service names). Empty = all.
    pub containers: &'a [String],
    pub build_args: &'a build::BuildArgs,
}

/// Show logs from a running stack (`docker compose logs`).
pub async fn logs(config: &Config, options: LogsOptions<'_>) -> Result<()> {
    // Showing logs from a running stack only needs the project name; don't
    // hard-fail just because `[containers]` was edited away after `up`.
    let (toml_config, project_name) = resolve_project_name(options.path, options.project)?;
    let cli = resolve_container_cli(config, options.build_args, toml_config.as_ref());
    let compose_project = compose_project_name(&project_name);

    // Best-effort `--container` validation: only check names against the
    // container list when the config still resolves to a multi-container
    // project. Otherwise pass the names through (the stack may already be
    // running with containers no longer in rise.toml).
    if let Some(resolved) = toml_config.as_ref().and_then(|c| c.resolve_deploy().ok()) {
        if !resolved.containers.is_empty() {
            for name in options.containers {
                if !resolved.containers.iter().any(|c| &c.name == name) {
                    let available = resolved
                        .containers
                        .iter()
                        .map(|c| c.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    bail!("container '{}' not found. Available: {}", name, available);
                }
            }
        }
    }

    let mut cmd = compose_for_project(&cli, &compose_project);
    cmd.arg("logs");
    if options.follow {
        cmd.arg("--follow");
    }
    if let Some(tail) = &options.tail {
        cmd.arg("--tail").arg(tail);
    }
    for name in options.containers {
        cmd.arg(name);
    }

    let status = cmd.status().context("Failed to run docker compose logs")?;

    if !status.success() {
        if let Some(code) = status.code() {
            bail!("docker compose logs exited with status code: {}", code);
        }
        bail!("docker compose logs was terminated by a signal");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rise_toml::{
        BuildConfig, ContainerConfig, ResolvedContainer, ResolvedRoute, DEFAULT_CONTAINER_NAME,
    };

    fn container(name: &str, port: Option<u16>) -> ResolvedContainer {
        ResolvedContainer {
            name: name.to_string(),
            image: None,
            build: None,
            port,
            replicas: None,
            cpu: None,
            memory: None,
            env: BTreeMap::new(),
            health_check: None,
        }
    }

    #[test]
    fn build_compose_wires_routes_hosts_and_router() {
        let resolved = ResolvedDeploy {
            containers: vec![
                container("web", Some(3000)),
                container("api", Some(8080)),
                container("redis", Some(6379)),
            ],
            routes: vec![
                ResolvedRoute {
                    path: "/".to_string(),
                    container: "web".to_string(),
                },
                ResolvedRoute {
                    path: "/api".to_string(),
                    container: "api".to_string(),
                },
            ],
        };
        let mut shared = BTreeMap::new();
        shared.insert("FOO".to_string(), "bar".to_string());

        let compose = build_compose("my-app", &resolved, &shared, 8080);

        // One service per container plus the router.
        assert_eq!(compose.services.len(), 4);
        assert!(compose.services.contains_key("rise-router"));

        let web = &compose.services["web"];
        assert_eq!(web.image, "rise-local-my-app-web");
        // Shared env + PORT + sibling hosts (incl. self), incl. the port-only redis.
        assert_eq!(web.environment["FOO"], "bar");
        assert_eq!(web.environment["PORT"], "3000");
        assert_eq!(web.environment["RISE_CONTAINER"], "web");
        assert_eq!(web.environment["RISE_CONTAINER_HOST__WEB"], "web:3000");
        assert_eq!(web.environment["RISE_CONTAINER_HOST__API"], "api:8080");
        assert_eq!(web.environment["RISE_CONTAINER_HOST__REDIS"], "redis:6379");
        // Routed: traefik labels present.
        assert!(web.labels.contains(&"traefik.enable=true".to_string()));
        assert!(web.labels.iter().any(|l| l.contains("PathPrefix(`/`)")));

        // Port-only, unrouted container: no traefik labels, no host publish.
        let redis = &compose.services["redis"];
        assert!(redis.labels.is_empty());
        assert!(redis.ports.is_empty());

        // Router publishes the host port and mounts the docker socket.
        let router = &compose.services["rise-router"];
        assert_eq!(router.image, "traefik:v3.7.4");
        assert_eq!(router.ports, vec!["8080:80".to_string()]);
        assert_eq!(router.environment["DOCKER_API_VERSION"], "1.44");
        assert!(router.volumes.iter().any(|v| v.contains("docker.sock")));
    }

    #[test]
    fn build_compose_omits_router_without_routes() {
        let resolved = ResolvedDeploy {
            containers: vec![container("worker", None), container("other", None)],
            routes: vec![],
        };
        let compose = build_compose("app", &resolved, &BTreeMap::new(), 8080);
        assert!(!compose.services.contains_key("rise-router"));
        assert_eq!(compose.services.len(), 2);
    }

    #[test]
    fn load_compose_project_synthesizes_single_container_app() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("rise.toml"),
            r#"
[project]
name = "solo"

[build]
backend = "docker"

[deploy]
replicas = 2
"#,
        )
        .unwrap();

        let loaded = load_compose_project(dir.path().to_str().unwrap(), None, 3000).unwrap();

        assert_eq!(loaded.project_name, "solo");
        assert_eq!(loaded.resolved.containers.len(), 1);
        let app = &loaded.resolved.containers[0];
        assert_eq!(app.name, DEFAULT_CONTAINER_NAME);
        assert_eq!(app.port, Some(3000));
        assert_eq!(app.replicas, Some(2));
        assert!(app.build.is_some());
        assert_eq!(loaded.resolved.routes.len(), 1);
        assert_eq!(loaded.resolved.routes[0].path, "/");
        assert_eq!(loaded.resolved.routes[0].container, DEFAULT_CONTAINER_NAME);

        let compose = build_compose(
            &loaded.project_name,
            &loaded.resolved,
            &BTreeMap::new(),
            8080,
        );
        assert!(compose.services.contains_key("app"));
        assert!(compose.services.contains_key("rise-router"));
        assert_eq!(compose.services["app"].environment["PORT"], "3000");
        assert_eq!(compose.services["app"].environment["RISE_CONTAINER"], "app");
        assert_eq!(
            compose.services["rise-router"].ports,
            vec!["8080:80".to_string()]
        );
    }

    #[test]
    fn router_name_sanitizes_paths() {
        // Readable prefix is retained; a short hash is appended for uniqueness.
        assert!(router_name("api", "/api").starts_with("api-api-"));
        assert!(router_name("web", "/").starts_with("web-root-"));
        assert!(router_name("api", "/v1/users").starts_with("api-v1-users-"));
    }

    #[test]
    fn router_name_disambiguates_colliding_paths() {
        // Paths that sanitize to the same readable suffix must still get
        // distinct router names, or their Traefik labels would clobber.
        let names = ["/api-v1", "/api/v1", "/api.v1", "/api_v1"].map(|p| router_name("api", p));
        let unique: std::collections::HashSet<&String> = names.iter().collect();
        assert_eq!(unique.len(), names.len());
        // Deterministic across calls.
        assert_eq!(router_name("api", "/api/v1"), router_name("api", "/api/v1"));
    }

    #[test]
    fn prebuilt_image_used_verbatim() {
        // A container with an explicit `image` should reference it verbatim,
        // not a generated `rise-local-*` tag.
        let mut redis = container("redis", Some(6379));
        redis.image = Some("redis:7-alpine".to_string());
        let resolved = ResolvedDeploy {
            containers: vec![container("web", Some(3000)), redis],
            routes: vec![],
        };
        let compose = build_compose("my-app", &resolved, &BTreeMap::new(), 8080);
        assert_eq!(compose.services["redis"].image, "redis:7-alpine");
        // The built container still gets the local tag.
        assert_eq!(compose.services["web"].image, "rise-local-my-app-web");
    }

    #[test]
    fn per_container_env_overrides_shared_env() {
        let mut web = container("web", Some(3000));
        web.env.insert("FOO".to_string(), "container".to_string());
        let resolved = ResolvedDeploy {
            containers: vec![web],
            routes: vec![],
        };
        let mut shared = BTreeMap::new();
        shared.insert("FOO".to_string(), "shared".to_string());
        let compose = build_compose("app", &resolved, &shared, 8080);
        // Per-container `[containers.X.env]` wins over project-level shared env.
        assert_eq!(compose.services["web"].environment["FOO"], "container");
    }

    #[test]
    fn compose_project_name_sanitizes() {
        // Uppercase lowered; `/` and other non-alphanumerics → `-`.
        assert_eq!(compose_project_name("My/App"), "rise-my-app");
        assert_eq!(compose_project_name("foo.bar_baz"), "rise-foo-bar-baz");
        // Leading/trailing dashes trimmed from the sanitized segment.
        assert_eq!(compose_project_name("--weird--"), "rise-weird");
        assert_eq!(compose_project_name("a"), "rise-a");
    }

    #[test]
    fn project_env_overrides_injected_host_var() {
        let resolved = ResolvedDeploy {
            containers: vec![container("web", Some(3000)), container("api", Some(8080))],
            routes: vec![],
        };
        let mut shared = BTreeMap::new();
        // A project-level var colliding with an injected host var must win,
        // matching the reconciler (which skips the injected var in that case).
        shared.insert(
            "RISE_CONTAINER_HOST__API".to_string(),
            "api.external:9999".to_string(),
        );
        let compose = build_compose("app", &resolved, &shared, 8080);
        assert_eq!(
            compose.services["web"].environment["RISE_CONTAINER_HOST__API"],
            "api.external:9999"
        );
    }

    #[test]
    fn local_dev_rejects_per_container_container_cli_for_compose() {
        let mut config = ProjectBuildConfig::default();
        config.containers.insert(
            "api".to_string(),
            ContainerConfig {
                build: Some(BuildConfig {
                    container_cli: Some("podman".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );

        let err = reject_per_container_container_cli(&config, None)
            .unwrap_err()
            .to_string();

        assert!(err.contains("per-container build.container_cli is not supported"));
        assert!(err.contains("[containers.api.build]"));
    }

    #[test]
    fn local_dev_rejects_per_container_container_cli_for_selected_run_target() {
        let mut config = ProjectBuildConfig::default();
        config.containers.insert(
            "api".to_string(),
            ContainerConfig {
                build: Some(BuildConfig {
                    container_cli: Some("podman".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        config.containers.insert(
            "web".to_string(),
            ContainerConfig {
                build: Some(BuildConfig::default()),
                ..Default::default()
            },
        );

        assert!(reject_per_container_container_cli(&config, Some("web")).is_ok());
        let err = reject_per_container_container_cli(&config, Some("api"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("[containers.api.build]"));
    }

    #[test]
    fn local_dev_allows_top_level_container_cli() {
        let mut config = ProjectBuildConfig {
            build: Some(BuildConfig {
                container_cli: Some("podman".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        config.containers.insert(
            "api".to_string(),
            ContainerConfig {
                build: Some(BuildConfig::default()),
                ..Default::default()
            },
        );

        reject_per_container_container_cli(&config, None).unwrap();
    }
}
