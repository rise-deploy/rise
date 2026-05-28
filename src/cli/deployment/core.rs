use anyhow::{bail, Context, Result};
use comfy_table::{
    modifiers::UTF8_ROUND_CORNERS, presets::UTF8_FULL, Attribute, Cell, Color, Table,
};
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::build::{self, BuildOptions, PlatformSource};
use crate::config::Config;

// Re-export models from API module (always available)
pub use crate::api::models::{Deployment, DeploymentStatus};

/// Parse duration string (e.g., "5m", "30s", "1h")
pub(super) fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        bail!("Duration string is empty");
    }

    let (num_str, unit) = if let Some(num_str) = s.strip_suffix("ms") {
        (num_str, "ms")
    } else {
        let num_str = &s[..s.len() - 1];
        let unit = &s[s.len() - 1..];
        (num_str, unit)
    };

    let num: u64 = num_str.parse().context("Invalid duration number")?;

    let duration = match unit {
        "ms" => Duration::from_millis(num),
        "s" => Duration::from_secs(num),
        "m" => Duration::from_secs(num * 60),
        "h" => Duration::from_secs(num * 3600),
        _ => bail!("Invalid duration unit '{}'. Use ms, s, m, or h", unit),
    };

    Ok(duration)
}

/// Fetch deployment by project name and deployment_id
pub async fn fetch_deployment(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    project: &str,
    deployment_id: &str,
) -> Result<Deployment> {
    let url = format!(
        "{}/api/v1/projects/{}/deployments/{}",
        backend_url, project, deployment_id
    );

    let response = http_client
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .context("Failed to fetch deployment")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        bail!("Failed to fetch deployment ({}): {}", status, error_text);
    }

    let deployment: Deployment = response
        .json()
        .await
        .context("Failed to parse deployment response")?;

    Ok(deployment)
}

/// List deployments for a project
pub async fn list_deployments(
    http_client: &Client,
    backend_url: &str,
    config: &Config,
    project: &str,
    group: Option<&str>,
    limit: usize,
) -> Result<()> {
    let token = config
        .get_token()
        .ok_or_else(|| anyhow::anyhow!("Not logged in. Please run 'rise login' first."))?;

    if let Some(g) = group {
        info!(
            "Listing deployments for project '{}' in group '{}'",
            project, g
        );
    } else {
        info!("Listing deployments for project '{}'", project);
    }

    let mut url = format!("{}/api/v1/projects/{}/deployments", backend_url, project);

    // Add group query parameter if provided
    if let Some(g) = group {
        url = format!("{}?group={}", url, urlencoding::encode(g));
    }

    let response = http_client
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .context("Failed to list deployments")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        bail!("Failed to list deployments ({}): {}", status, error_text);
    }

    let mut deployments: Vec<Deployment> = response
        .json()
        .await
        .context("Failed to parse deployments")?;

    // Limit results
    deployments.truncate(limit);

    if deployments.is_empty() {
        println!("No deployments found for project '{}'", project);
        return Ok(());
    }

    // Group deployments by deployment_group to find active (Healthy) ones
    let mut active_per_group = std::collections::HashMap::new();
    for deployment in &deployments {
        if deployment.status == DeploymentStatus::Healthy {
            active_per_group.insert(
                deployment.deployment_group.clone(),
                deployment.deployment_id.clone(),
            );
        }
    }

    // Find the active deployment in the default group (this is the project's active deployment)
    let default_active = active_per_group.get("default");

    // Create table
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_header(vec![
            Cell::new("DEPLOYMENT").add_attribute(Attribute::Bold),
            Cell::new("STATUS").add_attribute(Attribute::Bold),
            Cell::new("CREATED BY").add_attribute(Attribute::Bold),
            Cell::new("IMAGE").add_attribute(Attribute::Bold),
            Cell::new("GROUP").add_attribute(Attribute::Bold),
            Cell::new("EXPIRY").add_attribute(Attribute::Bold),
            Cell::new("CREATED").add_attribute(Attribute::Bold),
            Cell::new("URL").add_attribute(Attribute::Bold),
            Cell::new("ERROR").add_attribute(Attribute::Bold),
        ]);

    for deployment in deployments {
        // Just use deployment_id in the table, project is already in context
        let deployment_display = deployment.deployment_id.clone();
        // Only show URL for Healthy deployments (inactive deployments can't be connected to)
        let url = if deployment.status == DeploymentStatus::Healthy {
            deployment.primary_url.as_deref().unwrap_or("-")
        } else {
            "-"
        };

        // Format created time (just show date and time, not full RFC3339)
        let created = if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&deployment.created) {
            dt.format("%Y-%m-%d %H:%M:%S").to_string()
        } else {
            deployment.created.clone()
        };

        // Format expiry time
        let expiry = if let Some(expires_at) = &deployment.expires_at {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(expires_at) {
                dt.format("%Y-%m-%d %H:%M:%S").to_string()
            } else {
                expires_at.to_string()
            }
        } else {
            "-".to_string()
        };

        // Determine if this deployment is active in its group
        let is_active_in_group =
            active_per_group.get(&deployment.deployment_group) == Some(&deployment.deployment_id);

        // Determine if this is the default group's active deployment (bold)
        let is_default_active = default_active == Some(&deployment.deployment_id);

        // Format image (show the image tag or "-" if not set)
        let image_display = deployment.image.as_deref().unwrap_or("-");

        // Create cells with appropriate styling
        let mut deployment_cell = Cell::new(&deployment_display);
        let mut status_cell = Cell::new(deployment.status.to_string());
        let mut created_by_cell = Cell::new(&deployment.created_by_email);
        let mut image_cell = Cell::new(image_display);
        let mut group_cell = Cell::new(&deployment.deployment_group);
        let mut expiry_cell = Cell::new(&expiry);
        let mut created_cell = Cell::new(&created);
        let mut url_cell = Cell::new(url);

        // Apply bold to the entire row if this is the default group's active deployment
        if is_default_active {
            deployment_cell = deployment_cell.add_attribute(Attribute::Bold);
            status_cell = status_cell.add_attribute(Attribute::Bold);
            created_by_cell = created_by_cell.add_attribute(Attribute::Bold);
            image_cell = image_cell.add_attribute(Attribute::Bold);
            group_cell = group_cell.add_attribute(Attribute::Bold);
            expiry_cell = expiry_cell.add_attribute(Attribute::Bold);
            created_cell = created_cell.add_attribute(Attribute::Bold);
            url_cell = url_cell.add_attribute(Attribute::Bold);
        }

        // Apply green color if this is active in its group
        if is_active_in_group {
            deployment_cell = deployment_cell.fg(Color::Green);
            status_cell = status_cell.fg(Color::Green);
        }

        // Create error cell with truncated message
        let error_cell = if let Some(ref error) = deployment.error_message {
            let truncated: String = if error.len() > 40 {
                format!("{}...", &error[..37])
            } else {
                error.clone()
            };
            Cell::new(truncated).fg(Color::Red)
        } else {
            Cell::new("-")
        };

        table.add_row(vec![
            deployment_cell,
            status_cell,
            created_by_cell,
            image_cell,
            group_cell,
            expiry_cell,
            created_cell,
            url_cell,
            error_cell,
        ]);
    }

    println!("{}", table);

    Ok(())
}

/// Show deployment details and optionally follow until terminal state
pub async fn show_deployment(
    http_client: &Client,
    backend_url: &str,
    config: &Config,
    project: &str,
    deployment_id: &str,
    follow: bool,
    timeout_str: &str,
) -> Result<()> {
    if follow {
        // Use new enhanced UI for follow mode
        let deployment = super::follow_ui::follow_deployment_with_ui(
            http_client,
            backend_url,
            config,
            project,
            deployment_id,
            timeout_str,
        )
        .await?;

        // Exit with error if deployment failed
        if deployment.status == DeploymentStatus::Failed {
            if let Some(error) = deployment.error_message {
                bail!("Deployment failed: {}", error);
            } else {
                bail!("Deployment failed");
            }
        }

        Ok(())
    } else {
        // One-shot display (no follow)
        let token = config
            .token
            .as_ref()
            .context("Not logged in. Please run 'rise login' first.")?;

        let deployment =
            fetch_deployment(http_client, backend_url, token, project, deployment_id).await?;

        // Use the same UI as follow mode
        super::follow_ui::print_deployment_snapshot(&deployment);

        // Exit with error if deployment failed
        if deployment.status == DeploymentStatus::Failed {
            if let Some(error) = deployment.error_message {
                bail!("Deployment failed: {}", error);
            } else {
                bail!("Deployment failed");
            }
        }

        Ok(())
    }
}

/// Rollback to a previous deployment
///
/// Creates a new deployment with the same image as the reference deployment

#[derive(Debug, Deserialize)]
struct StopDeploymentsResponse {
    stopped_count: usize,
    deployment_ids: Vec<String>,
}

/// Stop all deployments in a group for a project
pub async fn stop_deployments_by_group(
    http_client: &Client,
    backend_url: &str,
    config: &Config,
    project: &str,
    group: &str,
) -> Result<()> {
    let token = config
        .get_token()
        .ok_or_else(|| anyhow::anyhow!("Not logged in. Please run 'rise login' first."))?;

    info!(
        "Stopping deployments in group '{}' for project '{}'",
        group, project
    );

    let url = format!(
        "{}/api/v1/projects/{}/deployments/stop?group={}",
        backend_url,
        project,
        urlencoding::encode(group)
    );

    let response = http_client
        .post(&url)
        .bearer_auth(token)
        .send()
        .await
        .context("Failed to stop deployments")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        bail!("Failed to stop deployments ({}): {}", status, error_text);
    }

    let stop_response: StopDeploymentsResponse = response
        .json()
        .await
        .context("Failed to parse stop response")?;

    if stop_response.stopped_count == 0 {
        println!("No running deployments found in group '{}'", group);
    } else {
        println!(
            "✓ Stopped {} deployment(s) in group '{}':",
            stop_response.stopped_count, group
        );
        for deployment_id in &stop_response.deployment_ids {
            println!("  - {}", deployment_id);
        }
    }

    Ok(())
}

// ============================================================================
// Deployment Creation (merged from deploy.rs)
// ============================================================================

#[derive(Debug, Deserialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
enum RegistryAuthMethod {
    #[default]
    LoginCredentials,
    RegistryToken,
}

#[derive(Debug, Deserialize)]
struct RegistryCredentials {
    registry_url: String,
    username: String,
    password: String,
    #[allow(dead_code)]
    expires_in: Option<u64>,
    #[serde(default)]
    auth_method: RegistryAuthMethod,
}

#[derive(Debug, Deserialize)]
struct CreateDeploymentResponse {
    deployment_id: String,
    image_tag: String,
    /// Deprecated: credentials are now fetched from the deployment-scoped endpoint.
    /// Kept for deserialization compatibility with older backends.
    #[allow(dead_code)]
    credentials: RegistryCredentials,
}

#[derive(Debug, Deserialize)]
struct GetRegistryCredsResponse {
    credentials: RegistryCredentials,
    #[allow(dead_code)]
    repository: String,
    /// Backend-advertised container platform for the target cluster (e.g.
    /// "linux/amd64"). Absent when the backend doesn't pin a node arch — CLI
    /// then falls back to the host architecture.
    #[serde(default)]
    target_platform: Option<String>,
}

/// Registry credentials plus the backend-advertised target platform.
struct DeploymentRegistryInfo {
    credentials: RegistryCredentials,
    target_platform: Option<String>,
}

/// Log the resolved build platform and the source it was inferred from, so the
/// choice isn't silent when Rise infers it. Explicit user choices (CLI flag,
/// env var, rise.toml) stay quiet — the user already knows what they asked for.
fn log_platform_choice(platform: &str, source: PlatformSource, operation: &str) {
    use crate::build::PlatformSource::*;
    match source {
        CliFlag | EnvVar | RiseToml => {} // explicit user choice, stay silent
        BackendHint => info!("{operation} for {platform} (target cluster architecture)"),
        HostFallback => {
            info!("{operation} for {platform} (host architecture; pass --platform to override)")
        }
    }
}

/// Fetch registry push credentials scoped to a specific deployment.
async fn fetch_deployment_registry_credentials(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    project_name: &str,
    deployment_id: &str,
) -> Result<DeploymentRegistryInfo> {
    let url = format!(
        "{}/api/v1/projects/{}/deployments/{}/registry-credentials",
        backend_url, project_name, deployment_id
    );

    let response = http_client
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .context("Failed to fetch registry credentials")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        bail!(
            "Failed to fetch registry credentials ({}): {}",
            status,
            error_text
        );
    }

    let resp: GetRegistryCredsResponse = response
        .json()
        .await
        .context("Failed to parse registry credentials response")?;

    Ok(DeploymentRegistryInfo {
        credentials: resp.credentials,
        target_platform: resp.target_platform,
    })
}

/// A runtime environment variable override for a deployment
#[derive(Debug, Clone)]
pub struct EnvOverride {
    pub key: String,
    pub value: String,
    pub is_secret: bool,
    pub is_protected: bool,
    pub source: Option<String>,
    /// Target environment name. When set, the server only applies this override
    /// if the resolved deployment environment matches. `None` means global.
    pub for_environment: Option<String>,
}

/// Options for creating a deployment
pub struct DeploymentOptions<'a> {
    pub project_name: &'a str,
    pub path: &'a str,
    pub image: Option<&'a str>,
    pub group: Option<&'a str>,
    pub environment: Option<&'a str>,
    pub expires_in: Option<&'a str>,
    /// HTTP port the application listens on.
    /// If None, server will use project's PORT env var or default to 8080.
    pub http_port: Option<u16>,
    pub build_args: &'a build::BuildArgs,
    pub from_deployment: Option<&'a str>,
    pub use_source_env_vars: bool,
    /// When true with --image, pull the image locally and push to Rise registry.
    pub push_image: bool,
    /// Runtime environment variable overrides to apply to the deployment.
    pub env_overrides: Vec<EnvOverride>,
    /// URL to the CI pipeline/job that created this deployment.
    /// If None, auto-detection from CI environment variables is attempted.
    pub job_url: Option<String>,
    /// URL to the pull request/merge request associated with this deployment.
    /// If None, auto-detection from CI environment variables is attempted.
    pub pull_request_url: Option<String>,
    /// URL to the Git repository this deployment was created from.
    /// If None, auto-detection from CI environment variables (or the local
    /// `origin` remote) is attempted.
    pub git_repository_url: Option<String>,
    /// Pre-loaded project config from rise.toml to avoid re-loading during build.
    pub toml_config: Option<build::config::ProjectBuildConfig>,
    /// Number of replicas (resolved from CLI flag > rise.toml > server default)
    pub replicas: Option<u32>,
    /// CPU allocation (resolved from CLI flag > rise.toml > server default)
    pub cpu: Option<String>,
    /// Memory allocation (resolved from CLI flag > rise.toml > server default)
    pub memory: Option<String>,
}

pub async fn create_deployment(
    http_client: &Client,
    backend_url: &str,
    config: &Config,
    deploy_opts: DeploymentOptions<'_>,
) -> Result<()> {
    if let Some(from_deployment_id) = deploy_opts.from_deployment {
        info!(
            "Creating deployment for project '{}' from deployment '{}' with {} environment variables",
            deploy_opts.project_name,
            from_deployment_id,
            if deploy_opts.use_source_env_vars { "source" } else { "current project" }
        );
    } else if let Some(image_ref) = deploy_opts.image {
        info!(
            "Deploying project '{}' with pre-built image '{}'",
            deploy_opts.project_name, image_ref
        );
    } else {
        info!(
            "Deploying project '{}' from path '{}'",
            deploy_opts.project_name, deploy_opts.path
        );
    }

    // Get authentication token
    let token = config
        .get_token()
        .ok_or_else(|| anyhow::anyhow!("Not authenticated. Run 'rise login' first."))?;

    // Resolve job_url and pull_request_url: use explicit values, or auto-detect from CI environment
    let resolved_job_url = deploy_opts.job_url.clone().or_else(detect_ci_job_url);
    let resolved_pull_request_url = deploy_opts
        .pull_request_url
        .clone()
        .or_else(detect_ci_pull_request_url);
    // Resolve the Git repository: use the explicit value (normalized), or
    // auto-detect from the CI environment / local git remote.
    let resolved_git_repository_url = match deploy_opts.git_repository_url.as_deref() {
        Some(raw) => match normalize_git_url(raw) {
            Some(url) => Some(url),
            None => anyhow::bail!("--git-repository: could not parse {:?} as a Git URL", raw),
        },
        None => detect_git_repository_url(),
    };

    if let Some(ref url) = resolved_job_url {
        info!("Deployment job URL: {}", url);
    }
    if let Some(ref url) = resolved_pull_request_url {
        info!("Deployment pull request URL: {}", url);
    }
    if let Some(ref url) = resolved_git_repository_url {
        info!("Deployment git repository: {}", url);
    }

    // Step 1: Create deployment and get deployment ID + credentials
    info!(
        "Creating deployment for project '{}'",
        deploy_opts.project_name
    );
    let deployment_info = call_create_deployment_api(
        http_client,
        backend_url,
        &token,
        deploy_opts.project_name,
        deploy_opts.image,
        deploy_opts.group,
        deploy_opts.environment,
        deploy_opts.expires_in,
        deploy_opts.http_port,
        deploy_opts.from_deployment,
        deploy_opts.use_source_env_vars,
        deploy_opts.push_image,
        &deploy_opts.env_overrides,
        resolved_job_url.as_deref(),
        resolved_pull_request_url.as_deref(),
        resolved_git_repository_url.as_deref(),
        deploy_opts.replicas,
        deploy_opts.cpu.as_deref(),
        deploy_opts.memory.as_deref(),
        deploy_opts
            .toml_config
            .as_ref()
            .and_then(|c| c.identity.as_ref())
            .map(|i| &i.audiences),
    )
    .await?;

    info!("Deployment ID: {}", deployment_info.deployment_id);
    info!("Image tag: {}", deployment_info.image_tag);

    // Set up Ctrl+C handler to gracefully cancel deployment
    let deployment_id_clone = deployment_info.deployment_id.clone();
    let backend_url_clone = backend_url.to_string();
    let http_client_clone = http_client.clone();
    let token_clone = token.to_string();
    let project_name_clone = deploy_opts.project_name.to_string();

    tokio::spawn(async move {
        if let Ok(()) = tokio::signal::ctrl_c().await {
            eprintln!("\n⚠️  Caught Ctrl+C, cancelling deployment...");

            // Cancel the deployment
            if let Err(e) = cancel_deployment(
                &http_client_clone,
                &backend_url_clone,
                &token_clone,
                &project_name_clone,
                &deployment_id_clone,
            )
            .await
            {
                eprintln!("Failed to cancel deployment: {}", e);
            } else {
                eprintln!("✓ Deployment cancelled");
            }

            std::process::exit(130); // Standard exit code for SIGINT
        }
    });

    if let Some(source_image) = &deploy_opts.image {
        if deploy_opts.push_image {
            // Push-image path: pull image locally, tag it, and push to Rise registry
            info!(
                "Pulling and pushing image '{}' to Rise registry",
                source_image
            );

            // Determine container CLI from config/build args
            let container_cli = match &deploy_opts.build_args.container_cli {
                Some(cli) => cli.clone(),
                None => config.get_container_cli().command().to_string(),
            };

            // Fetch deployment-scoped registry credentials
            let registry_info = fetch_deployment_registry_credentials(
                http_client,
                backend_url,
                &token,
                deploy_opts.project_name,
                &deployment_info.deployment_id,
            )
            .await?;
            let credentials = &registry_info.credentials;

            login_to_registry(
                http_client,
                backend_url,
                &token,
                &container_cli,
                credentials,
                deploy_opts.project_name,
                &deployment_info.deployment_id,
            )
            .await?;

            // Pull the source image for the configured platform.
            let toml_platform = deploy_opts
                .toml_config
                .as_ref()
                .and_then(|c| c.build.as_ref())
                .and_then(|b| b.platform.as_deref());
            let env = build::env_var_non_empty("RISE_PLATFORM");
            let (platform, source) = build::resolve_platform(
                deploy_opts.build_args.platform.as_deref(),
                env.as_deref(),
                toml_platform,
                registry_info.target_platform.as_deref(),
            );
            log_platform_choice(&platform, source, "Pulling");
            if let Err(e) = build::docker_pull(&container_cli, source_image, &platform) {
                update_deployment_status(
                    http_client,
                    backend_url,
                    &token,
                    deploy_opts.project_name,
                    &deployment_info.deployment_id,
                    "Failed",
                    Some(&e.to_string()),
                )
                .await?;
                return Err(e);
            }

            // Tag it with the Rise registry image tag
            if let Err(e) =
                build::docker_tag(&container_cli, source_image, &deployment_info.image_tag)
            {
                update_deployment_status(
                    http_client,
                    backend_url,
                    &token,
                    deploy_opts.project_name,
                    &deployment_info.deployment_id,
                    "Failed",
                    Some(&e.to_string()),
                )
                .await?;
                return Err(e);
            }

            // Update status to Building (reusing existing state for push phase)
            update_deployment_status(
                http_client,
                backend_url,
                &token,
                deploy_opts.project_name,
                &deployment_info.deployment_id,
                "Building",
                None,
            )
            .await?;

            // Push to Rise registry
            if let Err(e) = build::docker_push(&container_cli, &deployment_info.image_tag) {
                update_deployment_status(
                    http_client,
                    backend_url,
                    &token,
                    deploy_opts.project_name,
                    &deployment_info.deployment_id,
                    "Failed",
                    Some(&e.to_string()),
                )
                .await?;
                return Err(e);
            }

            // Mark as pushed (controller will take over deployment)
            update_deployment_status(
                http_client,
                backend_url,
                &token,
                deploy_opts.project_name,
                &deployment_info.deployment_id,
                "Pushed",
                None,
            )
            .await?;

            info!(
                "✓ Successfully pushed {} to {}",
                source_image, deployment_info.image_tag
            );
        } else {
            // Pre-built image path: Skip build/push, backend already marked as Pushed
            info!("✓ Pre-built image deployment created");
        }
    } else if let Some(from_deployment) = &deploy_opts.from_deployment {
        // Redeploy from existing deployment: Skip build/push, backend already marked as Pushed
        info!(
            "✓ Deployment created from existing deployment '{}' with {} environment variables",
            from_deployment,
            if deploy_opts.use_source_env_vars {
                "source"
            } else {
                "current project"
            }
        );
    } else {
        // Build from source path: Execute build and push.
        //
        // Fetch credentials first so the backend's target_platform hint can
        // feed into BuildOptions' platform precedence.
        let registry_info = fetch_deployment_registry_credentials(
            http_client,
            backend_url,
            &token,
            deploy_opts.project_name,
            &deployment_info.deployment_id,
        )
        .await?;

        let options = BuildOptions::from_build_args(
            config,
            deployment_info.image_tag.clone(),
            deploy_opts.path.to_string(),
            deploy_opts.build_args,
            deploy_opts.toml_config,
            registry_info.target_platform.as_deref(),
        );
        log_platform_choice(&options.platform, options.platform_source, "Building");

        login_to_registry(
            http_client,
            backend_url,
            &token,
            options.container_cli.command(),
            &registry_info.credentials,
            deploy_opts.project_name,
            &deployment_info.deployment_id,
        )
        .await?;

        // Step 3: Update status to 'building'
        update_deployment_status(
            http_client,
            backend_url,
            &token,
            deploy_opts.project_name,
            &deployment_info.deployment_id,
            "Building",
            None,
        )
        .await?;

        // Step 4: Build and push image using build module
        let options = options.with_push(true);

        if let Err(e) = build::build_image(options) {
            update_deployment_status(
                http_client,
                backend_url,
                &token,
                deploy_opts.project_name,
                &deployment_info.deployment_id,
                "Failed",
                Some(&e.to_string()),
            )
            .await?;
            return Err(e);
        }

        // Step 5: Mark as pushed (controller will take over deployment)
        update_deployment_status(
            http_client,
            backend_url,
            &token,
            deploy_opts.project_name,
            &deployment_info.deployment_id,
            "Pushed",
            None,
        )
        .await?;

        info!(
            "✓ Successfully pushed {} to {}",
            deploy_opts.project_name, deployment_info.image_tag
        );
    }
    info!("  Deployment ID: {}", deployment_info.deployment_id);
    println!();

    // Step 7: Follow deployment until completion
    show_deployment(
        http_client,
        backend_url,
        config,
        deploy_opts.project_name,
        &deployment_info.deployment_id,
        true,  // follow
        "10m", // timeout
    )
    .await?;

    Ok(())
}
/// Login to the container registry, marking the deployment as Failed on error.
async fn login_to_registry(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    container_cli: &str,
    credentials: &RegistryCredentials,
    project_name: &str,
    deployment_id: &str,
) -> Result<()> {
    let result = match credentials.auth_method {
        RegistryAuthMethod::LoginCredentials => {
            if credentials.username.is_empty() {
                return Ok(()); // OCI client-auth: credentials managed by docker login
            }
            info!("Logging into registry");
            build::docker_login(
                container_cli,
                &credentials.registry_url,
                &credentials.username,
                &credentials.password,
            )
        }
        RegistryAuthMethod::RegistryToken => {
            // Extract the registry host from the full registry_url, stripping any scheme
            let registry_host = credentials
                .registry_url
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .split('/')
                .next()
                .unwrap_or(&credentials.registry_url);
            info!("Injecting registry token for {}", registry_host);
            build::inject_registry_auth(container_cli, registry_host, &credentials.password)
        }
    };

    if let Err(e) = result {
        update_deployment_status(
            http_client,
            backend_url,
            token,
            project_name,
            deployment_id,
            "Failed",
            Some(&e.to_string()),
        )
        .await?;
        return Err(e);
    }
    Ok(())
}

/// Read an environment variable, returning `None` when it is unset or empty.
fn env_non_empty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Auto-detect the CI pipeline/job URL from well-known CI environment variables.
///
/// Supported CI platforms:
/// - GitLab CI: `CI_JOB_URL`
/// - GitHub Actions: `GITHUB_SERVER_URL` + `GITHUB_REPOSITORY` + `GITHUB_RUN_ID`
/// - CircleCI: `CIRCLE_BUILD_URL`
/// - Buildkite: `BUILDKITE_BUILD_URL`
/// - Drone CI: `DRONE_BUILD_LINK`
/// - Jenkins: `BUILD_URL`
///
/// Returns the first detected URL, or None if no CI environment is detected.
fn detect_ci_job_url() -> Option<String> {
    // GitLab CI
    if let Some(url) = env_non_empty("CI_JOB_URL") {
        return Some(url);
    }

    // GitHub Actions — construct the workflow run URL
    if let (Some(server), Some(repo), Some(run_id)) = (
        env_non_empty("GITHUB_SERVER_URL"),
        env_non_empty("GITHUB_REPOSITORY"),
        env_non_empty("GITHUB_RUN_ID"),
    ) {
        let server = server.trim_end_matches('/');
        return Some(format!("{server}/{repo}/actions/runs/{run_id}"));
    }

    // Other CI platforms that expose the build/job URL directly
    for var in [
        "CIRCLE_BUILD_URL",    // CircleCI
        "BUILDKITE_BUILD_URL", // Buildkite
        "DRONE_BUILD_LINK",    // Drone CI
        "BUILD_URL",           // Jenkins
    ] {
        if let Some(url) = env_non_empty(var) {
            return Some(url);
        }
    }

    None
}

/// Auto-detect pull request/merge request URL from CI environment variables.
///
/// Supported CI environments:
/// - GitLab CI: `CI_MERGE_REQUEST_URL`
/// - GitHub Actions: Constructed from `GITHUB_SERVER_URL`, `GITHUB_REPOSITORY`, and
///   `GITHUB_REF` (when `GITHUB_EVENT_NAME` is `pull_request` or `pull_request_target`)
///
/// Returns the detected URL, or None if not in a PR/MR context.
fn detect_ci_pull_request_url() -> Option<String> {
    // GitLab CI
    if let Ok(url) = std::env::var("CI_MERGE_REQUEST_URL") {
        if !url.is_empty() {
            return Some(url);
        }
    }

    // GitHub Actions
    if let Ok(event) = std::env::var("GITHUB_EVENT_NAME") {
        if event == "pull_request" || event == "pull_request_target" {
            if let (Ok(server), Ok(repo), Ok(github_ref)) = (
                std::env::var("GITHUB_SERVER_URL"),
                std::env::var("GITHUB_REPOSITORY"),
                std::env::var("GITHUB_REF"),
            ) {
                // GITHUB_REF is in the form refs/pull/<number>/merge
                if let Some(pr_number) = github_ref
                    .strip_prefix("refs/pull/")
                    .and_then(|s| s.strip_suffix("/merge"))
                {
                    return Some(format!("{}/{}/pull/{}", server, repo, pr_number));
                }
            }
        }
    }

    None
}

/// Auto-detect the Git repository URL this deployment is created from.
///
/// Prefers well-known CI environment variables, and otherwise falls back to the
/// local `origin` Git remote. The detected URL is normalized to an HTTPS URL.
///
/// Returns None if no repository can be determined.
fn detect_git_repository_url() -> Option<String> {
    let raw = detect_ci_repository_url().or_else(detect_git_remote_url)?;
    normalize_git_url(&raw)
}

/// Detect the repository URL exposed by well-known CI platforms.
///
/// Supported CI platforms:
/// - GitHub Actions: `GITHUB_SERVER_URL` + `GITHUB_REPOSITORY`
/// - GitLab CI: `CI_PROJECT_URL`
/// - Bitbucket Pipelines: `BITBUCKET_GIT_HTTP_ORIGIN`
/// - CircleCI: `CIRCLE_REPOSITORY_URL`
/// - Azure Pipelines: `BUILD_REPOSITORY_URI`
/// - Buildkite: `BUILDKITE_REPO`
/// - Drone CI: `DRONE_GIT_HTTP_URL`
/// - Jenkins: `GIT_URL`
fn detect_ci_repository_url() -> Option<String> {
    // GitHub Actions — construct the repository URL
    if let (Some(server), Some(repo)) = (
        env_non_empty("GITHUB_SERVER_URL"),
        env_non_empty("GITHUB_REPOSITORY"),
    ) {
        return Some(format!("{}/{}", server.trim_end_matches('/'), repo));
    }

    // GitLab CI — CI_PROJECT_URL is the canonical web URL of the project
    if let Some(url) = env_non_empty("CI_PROJECT_URL") {
        return Some(url);
    }

    // Other CI platforms expose the clone/repository URL directly
    for var in [
        "BITBUCKET_GIT_HTTP_ORIGIN", // Bitbucket Pipelines
        "CIRCLE_REPOSITORY_URL",     // CircleCI
        "BUILD_REPOSITORY_URI",      // Azure Pipelines
        "BUILDKITE_REPO",            // Buildkite
        "DRONE_GIT_HTTP_URL",        // Drone CI
        // GIT_URL is checked last: it is a generic name used by Jenkins (git
        // plugin) but also set by many scripts and CI systems to non-repository
        // values (API endpoints, webhooks, etc.), making it the most
        // collision-prone variable in this list.
        "GIT_URL", // Jenkins (git plugin)
    ] {
        if let Some(url) = env_non_empty(var) {
            return Some(url);
        }
    }

    None
}

/// Read the URL of the local `origin` Git remote, if available.
///
/// Equivalent to the fetch URL reported by `git remote show origin`, but
/// resolved offline via `git remote get-url origin`.
fn detect_git_remote_url() -> Option<String> {
    use std::sync::mpsc;
    use std::time::Duration;

    // Run git in a separate thread so we can apply a timeout. `git remote
    // get-url` normally completes instantly (it reads `.git/config`), but a
    // misconfigured credential helper can cause it to block indefinitely.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = std::process::Command::new("git")
            .args(["remote", "get-url", "origin"])
            .output();
        let _ = tx.send(result);
    });

    let output = rx.recv_timeout(Duration::from_secs(5)).ok()?.ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if url.is_empty() {
        None
    } else {
        Some(url)
    }
}

/// Normalize a Git repository URL to a canonical `https://host/path` form.
///
/// Handles `git@host:owner/repo.git` (scp-like), `ssh://`, `git://`, and
/// `http(s)://` URLs. Any embedded credentials, port, and trailing `.git`
/// suffix are stripped. Returns `None` if the input cannot be parsed.
fn normalize_git_url(raw: &str) -> Option<String> {
    let raw = raw.trim().trim_end_matches('/');
    if raw.is_empty() {
        return None;
    }

    // Split into the authority (`[user@]host[:port]`) and path components.
    let (authority, path) = if let Some(idx) = raw.find("://") {
        // scheme://[user@]host[:port]/path
        raw[idx + 3..].split_once('/')?
    } else {
        // scp-like: [user@]host:path
        raw.split_once(':')?
    };

    // Drop any userinfo prefix and port suffix from the authority.
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, h)| h)
        .split(':')
        .next()?;

    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);

    if host.is_empty() || path.is_empty() {
        return None;
    }
    Some(format!("https://{host}/{path}"))
}

#[allow(clippy::too_many_arguments)]
async fn call_create_deployment_api(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    project_name: &str,
    image: Option<&str>,
    group: Option<&str>,
    environment: Option<&str>,
    expires_in: Option<&str>,
    http_port: Option<u16>,
    from_deployment: Option<&str>,
    use_source_env_vars: bool,
    push_image: bool,
    env_overrides: &[EnvOverride],
    job_url: Option<&str>,
    pull_request_url: Option<&str>,
    git_repository_url: Option<&str>,
    replicas: Option<u32>,
    cpu: Option<&str>,
    memory: Option<&str>,
    identity_audiences: Option<&std::collections::BTreeMap<String, String>>,
) -> Result<CreateDeploymentResponse> {
    let url = format!("{}/api/v1/deployments", backend_url);
    let mut payload = serde_json::json!({
        "project": project_name,
    });

    // Add http_port field if explicitly provided
    // If not provided, server will resolve from project's PORT env var or use default (8080)
    if let Some(port) = http_port {
        payload["http_port"] = serde_json::json!(port);
    }

    // Add image field if provided
    if let Some(image_ref) = image {
        payload["image"] = serde_json::json!(image_ref);
    }

    // Add group field if provided (defaults to "default" on backend)
    if let Some(group_name) = group {
        payload["group"] = serde_json::json!(group_name);
    }

    // Add environment field if provided
    if let Some(env_name) = environment {
        payload["environment"] = serde_json::json!(env_name);
    }

    // Add expires_in field if provided
    if let Some(expiration) = expires_in {
        payload["expires_in"] = serde_json::json!(expiration);
    }

    // Add from_deployment field if provided
    if let Some(source_deployment_id) = from_deployment {
        payload["from_deployment"] = serde_json::json!(source_deployment_id);
        payload["use_source_env_vars"] = serde_json::json!(use_source_env_vars);
    }

    // Add push_image field if set
    if push_image {
        payload["push_image"] = serde_json::json!(true);
    }

    // Add job_url if provided
    if let Some(url) = job_url {
        payload["job_url"] = serde_json::json!(url);
    }

    // Add pull_request_url if provided
    if let Some(url) = pull_request_url {
        payload["pull_request_url"] = serde_json::json!(url);
    }

    // Add git_repository_url if provided
    if let Some(url) = git_repository_url {
        payload["git_repository_url"] = serde_json::json!(url);
    }

    // Add resource fields if provided
    if let Some(r) = replicas {
        payload["replicas"] = serde_json::json!(r);
    }
    if let Some(c) = cpu {
        payload["cpu"] = serde_json::json!(c);
    }
    if let Some(m) = memory {
        payload["memory"] = serde_json::json!(m);
    }

    // Add identity audiences from [identity] in rise.toml if any
    if let Some(audiences) = identity_audiences {
        if !audiences.is_empty() {
            payload["identity_audiences"] = serde_json::json!(audiences);
        }
    }

    // Add env_overrides if any
    if !env_overrides.is_empty() {
        let overrides: Vec<serde_json::Value> = env_overrides
            .iter()
            .map(|o| {
                let mut v = serde_json::json!({
                    "key": o.key,
                    "value": o.value,
                    "is_secret": o.is_secret,
                    "is_protected": o.is_protected,
                });
                if let Some(ref source) = o.source {
                    v["source"] = serde_json::json!(source);
                }
                if let Some(ref env) = o.for_environment {
                    v["for_environment"] = serde_json::json!(env);
                }
                v
            })
            .collect();
        payload["env_overrides"] = serde_json::json!(overrides);
    }

    let response = http_client
        .post(&url)
        .header("Authorization", format!("Bearer {}", token))
        .json(&payload)
        .send()
        .await
        .context("Failed to create deployment")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        bail!("Failed to create deployment ({}): {}", status, error_text);
    }

    let deployment_info: CreateDeploymentResponse = response
        .json()
        .await
        .context("Failed to parse deployment response")?;

    Ok(deployment_info)
}

/// Cancel a deployment by marking it as Cancelling
async fn cancel_deployment(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    project_name: &str,
    deployment_id: &str,
) -> Result<()> {
    let url = format!(
        "{}/api/v1/projects/{}/deployments/{}/status",
        backend_url, project_name, deployment_id
    );

    let payload = serde_json::json!({
        "status": "Cancelling"
    });

    let response = http_client
        .patch(&url)
        .bearer_auth(token)
        .json(&payload)
        .send()
        .await
        .context("Failed to cancel deployment")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        bail!("Failed to cancel deployment ({}): {}", status, error_text);
    }

    Ok(())
}

async fn update_deployment_status(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    project_name: &str,
    deployment_id: &str,
    status: &str,
    error_message: Option<&str>,
) -> Result<()> {
    let url = format!(
        "{}/api/v1/projects/{}/deployments/{}/status",
        backend_url, project_name, deployment_id
    );
    let mut payload = serde_json::json!({
        "status": status,
    });

    if let Some(error) = error_message {
        payload["error_message"] = serde_json::json!(error);
    }

    debug!("Updating deployment {} status to {}", deployment_id, status);

    let response = http_client
        .patch(&url)
        .header("Authorization", format!("Bearer {}", token))
        .json(&payload)
        .send()
        .await;

    // Don't fail deployment if status update fails, just log it
    match response {
        Ok(resp) if resp.status().is_success() => Ok(()),
        Ok(resp) => {
            let status = resp.status();
            let error = resp.text().await.unwrap_or_else(|_| "Unknown".to_string());
            warn!("Failed to update deployment status: {} - {}", status, error);
            Ok(())
        }
        Err(e) => {
            warn!("Failed to update deployment status: {}", e);
            Ok(())
        }
    }
}

/// Parameters for get_logs function
pub struct GetLogsParams<'a> {
    pub project: &'a str,
    pub deployment_id: &'a str,
    pub follow: bool,
    pub tail: Option<usize>,
    pub timestamps: bool,
    pub since: Option<&'a str>,
    /// Restrict to these levels (repeated `?level=` on the wire). Empty =
    /// no filter. Accepted values come from `/api/v1/logs/capabilities`,
    /// but the CLI doesn't validate — the server returns no matches for
    /// unknown values.
    pub levels: &'a [String],
}

/// Get logs from a deployment
pub async fn get_logs(
    http_client: &reqwest::Client,
    backend_url: &str,
    token: &str,
    params: GetLogsParams<'_>,
) -> anyhow::Result<()> {
    use futures::StreamExt;

    // Build URL with query parameters
    let mut url = format!(
        "{}/api/v1/projects/{}/deployments/{}/logs",
        backend_url, params.project, params.deployment_id
    );

    let mut query_params: Vec<String> = vec![];
    if params.follow {
        query_params.push("follow=true".to_string());
    }
    if let Some(t) = params.tail {
        query_params.push(format!("tail={}", t));
    }
    if params.timestamps {
        query_params.push("timestamps=true".to_string());
    }
    if let Some(s) = params.since {
        let seconds = parse_duration_to_seconds(s)?;
        query_params.push(format!("since={}", seconds));
    }
    for level in params.levels {
        query_params.push(format!("level={}", urlencoding::encode(level)));
    }

    if !query_params.is_empty() {
        url.push('?');
        url.push_str(&query_params.join("&"));
    }

    debug!("Fetching logs from: {}", url);

    // Send request
    let response = http_client
        .get(&url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await?;

    // Check status
    if !response.status().is_success() {
        let status = response.status();
        let error = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown".to_string());
        return Err(anyhow::anyhow!(
            "Failed to get logs ({}): {}",
            status,
            error
        ));
    }

    // Setup Ctrl+C handler for graceful shutdown
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    // Stream response bytes. Buffer raw bytes so we don't corrupt UTF-8
    // codepoints split across chunk boundaries (see `ByteLineBuffer`).
    let mut stream = response.bytes_stream();
    let mut buffer = ByteLineBuffer::new();
    let mut event_type = String::from("message");

    loop {
        tokio::select! {
            // Handle Ctrl+C
            _ = &mut ctrl_c => {
                debug!("Received Ctrl+C, stopping log stream");
                break;
            }
            // Process stream chunks
            chunk_result = stream.next() => {
                match chunk_result {
                    Some(Ok(chunk)) => {
                        buffer.push_chunk(&chunk);

                        // Process complete lines from buffer
                        while let Some(line) = buffer.next_line() {
                            if let Some(event) = line.strip_prefix("event: ") {
                                event_type = event.to_string();
                                continue;
                            }

                            // Parse SSE format: lines starting with "data: "
                            if let Some(data) = line.strip_prefix("data: ") {
                                if !data.is_empty() {
                                    if event_type == "status" {
                                        print_log_status(data);
                                    } else if event_type == "log" || event_type == "message" {
                                        // `log` events are JSON-wrapped:
                                        // {"line": "...", "level": "..."}.
                                        // Other event types (`backlog_complete`)
                                        // are ignored — they're meta-events
                                        // the CLI has no use for.
                                        print_log_event(data);
                                    }
                                }
                            } else if line.is_empty() {
                                event_type = String::from("message");
                            } else if !line.starts_with(':') {
                                // SSE comments start with ':', skip them
                                // Print other non-empty lines (in case format changes)
                                println!("{}", line);
                            }
                        }
                    }
                    Some(Err(e)) => {
                        return Err(anyhow::anyhow!("Stream error: {}", e));
                    }
                    None => {
                        // Stream ended
                        debug!("Log stream ended");
                        break;
                    }
                }
            }
        }
    }

    // Print any remaining buffered content (final line without trailing '\n').
    if let Some(remainder) = buffer.take_remainder() {
        let line = remainder.trim();
        if let Some(data) = line.strip_prefix("data: ") {
            // Only print non-empty data
            if !data.is_empty() {
                if event_type == "status" {
                    print_log_status(data);
                } else if event_type == "log" || event_type == "message" {
                    print_log_event(data);
                }
            }
        } else if !line.is_empty() && !line.starts_with(':') {
            println!("{}", line);
        }
    }

    Ok(())
}

/// Extract `(line, level)` from a `log` SSE event payload.
///
/// The backend emits `data: {"line": "...", "level": "..."}`. If parsing
/// fails (unexpected shape, older server, plain text) fall back to using
/// the raw `data` as the line with an empty level so the user still sees
/// something useful.
fn extract_log_event(data: &str) -> (String, String) {
    let trimmed = data.trim_start();
    if !trimmed.starts_with('{') {
        return (data.to_string(), String::new());
    }
    match serde_json::from_str::<serde_json::Value>(data) {
        Ok(value) => {
            let line = value
                .get("line")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| data.to_string());
            let level = value
                .get("level")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            (line, level)
        }
        Err(_) => (data.to_string(), String::new()),
    }
}

/// Pull `(line, level)` from a `log` SSE event payload and print it,
/// coloured by level when stdout is a TTY and `NO_COLOR` isn't set.
fn print_log_event(data: &str) {
    let (line, level) = extract_log_event(data);
    print_log_line(&line, &level);
}

/// Print a log line, coloured by its level when colour output is enabled.
/// Used by both the one-shot `rise deployment logs` path and the
/// follow-UI's live stream so both render levels consistently.
pub(super) fn print_log_line(line: &str, level: &str) {
    if let Some(code) = level_ansi_color(level) {
        println!("{}{}{}", code, line, ANSI_RESET);
    } else {
        println!("{}", line);
    }
}

const ANSI_RESET: &str = "\x1b[0m";

/// Returns the ANSI start sequence for a given level string, or `None` if
/// colours should be skipped (non-TTY, `NO_COLOR` set, or unknown level
/// that maps to the terminal default).
///
/// The palette matches the frontend's `--r-log-chart-*` CSS variables
/// semantically: errors family in red, warn in amber/yellow, info in the
/// terminal default, debug/trace/unknown in dim grey.
fn level_ansi_color(level: &str) -> Option<&'static str> {
    use std::io::IsTerminal;
    if std::env::var_os("NO_COLOR").is_some() {
        return None;
    }
    if !std::io::stdout().is_terminal() {
        return None;
    }
    match level.trim().to_ascii_lowercase().as_str() {
        "error" => Some("\x1b[31m"),              // red
        "critical" | "fatal" => Some("\x1b[91m"), // bright red
        "warn" => Some("\x1b[33m"),               // yellow
        "debug" | "trace" => Some("\x1b[2m"),     // dim
        "unknown" => Some("\x1b[2m"),             // dim grey
        // info / notice / anything else: no colour (terminal default).
        _ => None,
    }
}

fn print_log_status(data: &str) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
        return;
    };

    let reason = value
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or("no_logs_found");
    let retention_hint = value.get("retention_hint").and_then(|v| v.as_str());

    match reason {
        "retention_expired_possible" => {
            if let Some(hint) = retention_hint {
                println!(
                    "No logs found. Runtime logs are retained for {}, so this deployment's logs may no longer be available.",
                    hint
                );
            } else {
                println!("No logs found. They may have expired based on the log backend retention policy.");
            }
        }
        "historical_backend_not_configured" => {
            println!("No active deployment pod was found and historical logs are not configured.")
        }
        "deployment_not_ready" => println!("Deployment logs are not ready yet."),
        "backend_unavailable" => println!("The log backend is unavailable."),
        _ => println!("No logs found."),
    }
}

/// Parse duration string (e.g., "5m", "1h", "30s") into seconds
fn parse_duration_to_seconds(duration: &str) -> anyhow::Result<i64> {
    let duration = duration.trim();

    if let Some(num_str) = duration.strip_suffix('s') {
        let num: i64 = num_str.parse()?;
        Ok(num)
    } else if let Some(num_str) = duration.strip_suffix('m') {
        let num: i64 = num_str.parse()?;
        Ok(num * 60)
    } else if let Some(num_str) = duration.strip_suffix('h') {
        let num: i64 = num_str.parse()?;
        Ok(num * 3600)
    } else if let Some(num_str) = duration.strip_suffix('d') {
        let num: i64 = num_str.parse()?;
        Ok(num * 86400)
    } else {
        Err(anyhow::anyhow!(
            "Invalid duration format '{}'. Use format like '5m', '1h', '30s', '7d'",
            duration
        ))
    }
}

// ============================================================================
// Log streaming helpers (used by follow_ui for inline log display)
// ============================================================================

/// Byte-level line buffer for SSE byte streams.
///
/// `reqwest::Response::chunk()` can split a UTF-8 codepoint across HTTP/TCP
/// chunk boundaries. Decoding each chunk with `from_utf8_lossy` would emit
/// `U+FFFD` replacement characters for the partial bytes on either side of
/// the boundary, silently corrupting any log line with non-ASCII content
/// (emoji, accented chars, CJK, …).
///
/// This buffer accumulates raw bytes and only decodes *complete* lines
/// (delimited by `\n`), leaving any trailing partial-line bytes — which
/// may include a partial codepoint at the end — buffered for the next
/// chunk. Decoding still uses `from_utf8_lossy` so genuinely-invalid byte
/// sequences within a complete line don't break the stream.
pub(super) struct ByteLineBuffer {
    buf: Vec<u8>,
}

impl ByteLineBuffer {
    pub(super) fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Append a chunk of raw bytes to the buffer.
    pub(super) fn push_chunk(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    /// Drain and return the next complete line from the buffer (without
    /// the trailing `\n`, and with any trailing `\r` trimmed). Returns
    /// `None` if the buffer doesn't yet contain a full line.
    pub(super) fn next_line(&mut self) -> Option<String> {
        let newline_pos = self.buf.iter().position(|&b| b == b'\n')?;
        let mut line_bytes: Vec<u8> = self.buf.drain(..=newline_pos).collect();
        // Drop the trailing '\n'
        line_bytes.pop();
        // Drop a trailing '\r' if present (CRLF tolerance).
        if line_bytes.last() == Some(&b'\r') {
            line_bytes.pop();
        }
        Some(decode_line(&line_bytes))
    }

    /// Drain the remainder of the buffer as a final partial line, if any.
    /// Use this once the underlying byte stream has ended.
    pub(super) fn take_remainder(&mut self) -> Option<String> {
        if self.buf.is_empty() {
            return None;
        }
        let mut bytes = std::mem::take(&mut self.buf);
        // Tolerate CRLF on the last line just in case.
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        Some(decode_line(&bytes))
    }
}

/// Decode a complete line. Prefers strict UTF-8 decoding; falls back to
/// `from_utf8_lossy` only when the line genuinely contains invalid byte
/// sequences (not just a codepoint split across chunks — that's handled
/// at the `ByteLineBuffer` layer).
fn decode_line(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// Error type for log stream connection attempts.
pub(super) enum LogStreamError {
    /// Server returned 503 - pod/logs not ready yet
    NotReady,
    /// Server returned 410 - deployment gone/terminated
    Gone,
    /// Other error
    Other(anyhow::Error),
}

impl std::fmt::Debug for LogStreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogStreamError::NotReady => write!(f, "NotReady"),
            LogStreamError::Gone => write!(f, "Gone"),
            LogStreamError::Other(e) => write!(f, "Other({:?})", e),
        }
    }
}

/// SSE log stream that yields parsed log lines.
///
/// Wraps an SSE byte stream from the deployment logs endpoint and provides
/// a simple async interface for receiving individual log lines.
pub(super) struct LogStream {
    stream: futures::stream::BoxStream<'static, Result<bytes::Bytes, reqwest::Error>>,
    buffer: ByteLineBuffer,
    event_type: String,
    /// Set once the underlying byte stream returns `None`, so we can yield
    /// any trailing partial-line bytes as a final line before reporting
    /// stream end on the next call.
    stream_done: bool,
}

impl LogStream {
    /// Receive the next log line from the stream plus its backend-classified
    /// level. Returns `None` when the stream ends. The level is the raw
    /// string the SSE payload carried (one of the values advertised by
    /// `/api/v1/logs/capabilities`), or an empty string if the payload
    /// didn't carry one.
    pub async fn recv(&mut self) -> Option<Result<(String, String)>> {
        use futures::StreamExt;

        loop {
            // Try to extract a complete line from the buffer
            if let Some(line) = self.buffer.next_line() {
                if let Some(event) = line.strip_prefix("event: ") {
                    self.event_type = event.to_string();
                    continue;
                } else if let Some(data) = line.strip_prefix("data: ") {
                    if !data.is_empty()
                        && (self.event_type == "log" || self.event_type == "message")
                    {
                        // `log` events are JSON-wrapped:
                        // {"line": "...", "level": "..."}. Yield both so
                        // the follow UI can colour the line by level.
                        // Other event types (status, backlog_complete) are
                        // ignored by the streaming follow UI.
                        return Some(Ok(extract_log_event(data)));
                    }
                    continue;
                } else if line.is_empty() || line.starts_with(':') {
                    // SSE comment or empty line, skip
                    if line.is_empty() {
                        self.event_type = String::from("message");
                    }
                    continue;
                } else {
                    return Some(Ok((line, String::new())));
                }
            }

            // If the upstream stream already ended, flush any trailing
            // partial-line bytes as a final line, then signal end-of-stream.
            if self.stream_done {
                if let Some(remainder) = self.buffer.take_remainder() {
                    let line = remainder.trim();
                    if let Some(data) = line.strip_prefix("data: ") {
                        if !data.is_empty()
                            && (self.event_type == "log" || self.event_type == "message")
                        {
                            return Some(Ok(extract_log_event(data)));
                        }
                    } else if !line.is_empty() && !line.starts_with(':') {
                        return Some(Ok((line.to_string(), String::new())));
                    }
                }
                return None;
            }

            // Need more data from the stream
            match self.stream.next().await {
                Some(Ok(chunk)) => {
                    self.buffer.push_chunk(&chunk);
                }
                Some(Err(e)) => {
                    return Some(Err(anyhow::anyhow!("Log stream error: {}", e)));
                }
                None => {
                    self.stream_done = true;
                    // Loop around to flush any remainder.
                }
            }
        }
    }
}

/// Open an SSE log stream for a deployment.
///
/// Connects to the deployment logs SSE endpoint with `follow=true` and the
/// specified `tail` line count.
pub(super) async fn open_log_stream(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    project: &str,
    deployment_id: &str,
    tail: usize,
) -> Result<LogStream, LogStreamError> {
    use futures::StreamExt;

    let url = format!(
        "{}/api/v1/projects/{}/deployments/{}/logs?follow=true&tail={}",
        backend_url, project, deployment_id, tail
    );

    let response = http_client
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| LogStreamError::Other(anyhow::anyhow!("Failed to connect: {}", e)))?;

    let status = response.status();
    if status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
        return Err(LogStreamError::NotReady);
    }
    if status == reqwest::StatusCode::GONE {
        return Err(LogStreamError::Gone);
    }
    if !status.is_success() {
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        return Err(LogStreamError::Other(anyhow::anyhow!(
            "Failed to open log stream ({}): {}",
            status,
            error_text
        )));
    }

    Ok(LogStream {
        stream: response.bytes_stream().boxed(),
        buffer: ByteLineBuffer::new(),
        event_type: String::from("message"),
        stream_done: false,
    })
}

#[cfg(test)]
mod tests {
    use super::{extract_log_event, normalize_git_url, ByteLineBuffer};

    /// Feeding a non-ASCII line one byte at a time — i.e. splitting *every*
    /// multi-byte codepoint mid-character — must still round-trip the line
    /// intact. With the old `String::from_utf8_lossy(&chunk)` per-chunk
    /// approach, each partial-codepoint chunk would produce a `U+FFFD`
    /// replacement character, corrupting the output.
    #[test]
    fn byte_line_buffer_round_trips_codepoints_split_across_chunks() {
        let line = "data: 你好🌍 héllo\n";
        let bytes = line.as_bytes();

        let mut buffer = ByteLineBuffer::new();
        // Feed one byte at a time — guarantees every multi-byte codepoint
        // is split across chunk boundaries.
        for &b in bytes {
            buffer.push_chunk(&[b]);
        }

        let got = buffer.next_line().expect("expected one full line");
        assert_eq!(got, "data: 你好🌍 héllo");
        assert!(
            !got.contains('\u{FFFD}'),
            "line must not contain replacement chars: {:?}",
            got
        );
        assert!(buffer.next_line().is_none());
    }

    /// Same shape, but split at deliberately awkward offsets: inside each
    /// of `你`, `🌍`, and `é`. Catches off-by-one mistakes in the buffer
    /// boundary handling.
    #[test]
    fn byte_line_buffer_handles_multiple_mid_codepoint_splits() {
        let line = "data: 你好🌍 héllo\n";
        let bytes = line.as_bytes();

        // Split into three chunks, each cut mid-codepoint.
        // `你` is 3 bytes starting at index 6: split after byte 7 (mid-`你`).
        // `🌍` is 4 bytes starting at index 12: split after byte 14 (mid-`🌍`).
        let cut1 = 7;
        let cut2 = 14;
        assert!(cut1 < cut2 && cut2 < bytes.len());

        let mut buffer = ByteLineBuffer::new();
        buffer.push_chunk(&bytes[..cut1]);
        // No line yet — first chunk ends mid-codepoint and has no '\n'.
        assert!(buffer.next_line().is_none());
        buffer.push_chunk(&bytes[cut1..cut2]);
        assert!(buffer.next_line().is_none());
        buffer.push_chunk(&bytes[cut2..]);

        let got = buffer.next_line().expect("expected one full line");
        assert_eq!(got, "data: 你好🌍 héllo");
        assert!(!got.contains('\u{FFFD}'));
    }

    /// Multiple lines in a single buffer must each be yielded once, in
    /// order, with CRLF tolerated.
    #[test]
    fn byte_line_buffer_yields_multiple_lines_in_order() {
        let mut buffer = ByteLineBuffer::new();
        buffer.push_chunk(b"event: log\r\ndata: hello\nworld\n");
        assert_eq!(buffer.next_line().as_deref(), Some("event: log"));
        assert_eq!(buffer.next_line().as_deref(), Some("data: hello"));
        assert_eq!(buffer.next_line().as_deref(), Some("world"));
        assert_eq!(buffer.next_line(), None);
    }

    /// A trailing partial line (no terminating '\n') must be retrievable
    /// via `take_remainder` once the underlying byte stream has ended.
    #[test]
    fn byte_line_buffer_take_remainder_returns_trailing_partial_line() {
        let mut buffer = ByteLineBuffer::new();
        buffer.push_chunk(b"data: partial");
        assert!(buffer.next_line().is_none());
        assert_eq!(buffer.take_remainder().as_deref(), Some("data: partial"));
        // Empty buffer afterwards.
        assert_eq!(buffer.take_remainder(), None);
    }

    #[test]
    fn extract_log_event_reads_line_and_level() {
        let payload = r#"{"line":"2026-05-27T22:29:06+00:00 {\"level\":\"warn\",\"msg\":\"HTTP/2 skipped\"}","level":"warn"}"#;
        let (line, level) = extract_log_event(payload);
        assert_eq!(level, "warn");
        assert!(line.contains("HTTP/2 skipped"));
    }

    #[test]
    fn extract_log_event_handles_level_first_field() {
        // serde_json::Value doesn't care about field order, but verify
        // explicitly since the wire format isn't field-order-stable.
        let payload = r#"{"level":"error","line":"boom"}"#;
        let (line, level) = extract_log_event(payload);
        assert_eq!(level, "error");
        assert_eq!(line, "boom");
    }

    #[test]
    fn extract_log_event_missing_level_yields_empty_string() {
        let payload = r#"{"line":"plain"}"#;
        let (line, level) = extract_log_event(payload);
        assert_eq!(line, "plain");
        assert_eq!(level, "");
    }

    #[test]
    fn normalizes_scp_like_url() {
        assert_eq!(
            normalize_git_url("git@github.com:owner/repo.git").as_deref(),
            Some("https://github.com/owner/repo")
        );
    }

    #[test]
    fn normalizes_ssh_url_with_port() {
        assert_eq!(
            normalize_git_url("ssh://git@github.com:22/owner/repo.git").as_deref(),
            Some("https://github.com/owner/repo")
        );
    }

    #[test]
    fn normalizes_git_protocol_url() {
        assert_eq!(
            normalize_git_url("git://gitlab.com/group/sub/repo.git").as_deref(),
            Some("https://gitlab.com/group/sub/repo")
        );
    }

    #[test]
    fn strips_credentials_and_git_suffix_from_https_url() {
        assert_eq!(
            normalize_git_url("https://user:token@github.com/owner/repo.git").as_deref(),
            Some("https://github.com/owner/repo")
        );
    }

    #[test]
    fn passes_through_clean_https_url() {
        assert_eq!(
            normalize_git_url("https://github.com/owner/repo").as_deref(),
            Some("https://github.com/owner/repo")
        );
    }

    #[test]
    fn rejects_unparseable_input() {
        assert_eq!(normalize_git_url(""), None);
        assert_eq!(normalize_git_url("not-a-url"), None);
        assert_eq!(normalize_git_url("/local/path/repo"), None);
    }
}
