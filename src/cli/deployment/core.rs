use anyhow::{bail, Context, Result};
use comfy_table::{
    modifiers::UTF8_ROUND_CORNERS, presets::UTF8_FULL, Attribute, Cell, Color, Table,
};
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::build::{self, BuildOptions};
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
}

/// Fetch registry push credentials scoped to a specific deployment.
async fn fetch_deployment_registry_credentials(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    project_name: &str,
    deployment_id: &str,
) -> Result<RegistryCredentials> {
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

    Ok(resp.credentials)
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

    if let Some(ref url) = resolved_job_url {
        info!("Deployment job URL: {}", url);
    }
    if let Some(ref url) = resolved_pull_request_url {
        info!("Deployment pull request URL: {}", url);
    }

    // Client-controlled push: if rise.toml declares a [registry] block and the
    // caller hasn't asked for a rollback or supplied --image, build + push the
    // image locally then defer to Rise's pre-built-image API path. Rise stays
    // registry-agnostic; the source repo's rise.toml is the only thing that
    // knows the target path. Auth must already be present in the container CLI
    // config (ambient — typically a Vault-minted JFrog token from CI or laptop).
    let client_pushed = client_controlled_push_if_configured(config, &deploy_opts)?;
    if let Some(ref pushed) = client_pushed {
        info!(
            "Built and pushed image to {} (digest {})",
            pushed.image_tag, pushed.image_digest
        );
    }
    let effective_image: Option<&str> = client_pushed
        .as_ref()
        .map(|p| p.image_tag.as_str())
        .or(deploy_opts.image);
    let client_supplied_digest: Option<&str> =
        client_pushed.as_ref().map(|p| p.image_digest.as_str());

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
        effective_image,
        client_supplied_digest,
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
        deploy_opts.replicas,
        deploy_opts.cpu.as_deref(),
        deploy_opts.memory.as_deref(),
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

    if let Some(source_image) = effective_image {
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
            let credentials = fetch_deployment_registry_credentials(
                http_client,
                backend_url,
                &token,
                deploy_opts.project_name,
                &deployment_info.deployment_id,
            )
            .await?;

            login_to_registry(
                http_client,
                backend_url,
                &token,
                &container_cli,
                &credentials,
                deploy_opts.project_name,
                &deployment_info.deployment_id,
            )
            .await?;

            // Pull the source image for the configured platform
            let platform = deploy_opts
                .build_args
                .platform
                .as_deref()
                .unwrap_or(build::DEFAULT_PLATFORM);
            if let Err(e) = build::docker_pull(&container_cli, source_image, platform) {
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
        // Build from source path: Execute build and push
        let options = BuildOptions::from_build_args(
            config,
            deployment_info.image_tag.clone(),
            deploy_opts.path.to_string(),
            deploy_opts.build_args,
            deploy_opts.toml_config,
        );

        // Step 2: Fetch deployment-scoped registry credentials and login
        let credentials = fetch_deployment_registry_credentials(
            http_client,
            backend_url,
            &token,
            deploy_opts.project_name,
            &deployment_info.deployment_id,
        )
        .await?;

        login_to_registry(
            http_client,
            backend_url,
            &token,
            options.container_cli.command(),
            &credentials,
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

/// Auto-detect CI source URL from well-known CI environment variables.
///
/// Checks common CI platforms in order:
/// - GitLab CI: `CI_JOB_URL`
/// - GitHub Actions: `GITHUB_SERVER_URL` + `GITHUB_REPOSITORY` + `GITHUB_RUN_ID`
///
/// Returns the first detected URL, or None if no CI environment is detected.
fn detect_ci_job_url() -> Option<String> {
    // GitLab CI
    if let Ok(url) = std::env::var("CI_JOB_URL") {
        if !url.is_empty() {
            return Some(url);
        }
    }

    // GitHub Actions
    if let (Ok(server), Ok(repo), Ok(run_id)) = (
        std::env::var("GITHUB_SERVER_URL"),
        std::env::var("GITHUB_REPOSITORY"),
        std::env::var("GITHUB_RUN_ID"),
    ) {
        if !server.is_empty() && !repo.is_empty() && !run_id.is_empty() {
            return Some(format!("{}/{}/actions/runs/{}", server, repo, run_id));
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

/// Outcome of a successful client-controlled build + push.
struct ClientPushed {
    /// Tag-form image reference: `{image_base}/{project}:{deployment_id}`.
    image_tag: String,
    /// `sha256:...` digest captured from the local image after push. Forwarded
    /// to Rise so it doesn't need its own registry credentials to resolve.
    image_digest: String,
}

/// Build + push the project image to the registry configured in rise.toml's
/// `[registry]` block. Returns `Ok(None)` when the client-controlled path
/// doesn't apply: no rise.toml, no `[registry]`, `--image` was supplied, or
/// this is a rollback (`--from-deployment`).
///
/// Auth comes from whatever lives in `~/.docker/config.json` (or the podman
/// equivalent) — typically a Vault-minted JFrog token. No `docker login` here.
fn client_controlled_push_if_configured(
    config: &Config,
    deploy_opts: &DeploymentOptions<'_>,
) -> Result<Option<ClientPushed>> {
    if deploy_opts.image.is_some() || deploy_opts.from_deployment.is_some() {
        return Ok(None);
    }
    let Some(toml) = deploy_opts.toml_config.as_ref() else {
        return Ok(None);
    };
    let env_registry = deploy_opts
        .environment
        .and_then(|name| toml.environments.get(name))
        .and_then(|env| env.registry.as_ref());
    let Some(registry) = env_registry.or(toml.registry.as_ref()) else {
        return Ok(None);
    };

    let deployment_id = crate::deployment_id::generate_deployment_id();
    let image_tag = format!(
        "{}/{}:{}",
        registry.image_base.trim_end_matches('/'),
        deploy_opts.project_name,
        deployment_id
    );

    info!("Client-controlled push: building {image_tag}");

    let options = BuildOptions::from_build_args(
        config,
        image_tag.clone(),
        deploy_opts.path.to_string(),
        deploy_opts.build_args,
        deploy_opts.toml_config.clone(),
    );
    let container_cli = options.container_cli.command().to_string();

    // If the operator has configured Vault-backed JFrog minting
    // (`rise vault configure --address ... --artifactory-token-path ...`),
    // freshen `docker login` for the push host before handing off to buildkit.
    // Ambient docker auth still works when vault config is absent (CI flow,
    // or a laptop where the operator manages docker login by hand).
    ensure_jfrog_docker_login_if_configured(config, registry, &container_cli)?;

    let options = options.with_push(true);

    build::build_image(options).context("Client-controlled build/push failed")?;

    let image_digest = build::inspect_push_digest(&container_cli, &image_tag)
        .context("Failed to inspect pushed image for digest")?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Pushed image {image_tag} has no RepoDigests — push may not have completed"
            )
        })?;

    Ok(Some(ClientPushed {
        image_tag,
        image_digest,
    }))
}

/// Auto-mint a fresh JFrog token via Vault and run `docker login` against the
/// push host. No-op when Vault isn't configured — the caller falls back to
/// ambient docker auth.
///
/// Triggers when **both** `vault_address` and `vault_artifactory_token_path`
/// are set in `~/.config/rise/config.json` (or via env). Either alone is
/// treated as "not configured" — half-set is almost certainly a config error
/// we'd rather surface as "skipped" than as a half-broken mint attempt.
///
/// Steps when triggered:
/// 1. Ensure a valid Vault session — trigger `vault login -method=oidc` if not.
/// 2. Look up the user's email from the Vault token.
/// 3. Substitute `{email}` in the configured artifactory token path and read
///    the JFrog access token.
/// 4. `docker login <host>` with username=email, password=token. Host is
///    derived from `image_base`'s first slash-separated segment.
fn ensure_jfrog_docker_login_if_configured(
    config: &Config,
    registry: &crate::rise_toml::RegistryConfig,
    container_cli: &str,
) -> Result<()> {
    let (Some(addr), Some(token_path_template)) = (
        config.get_vault_address(),
        config.get_vault_artifactory_token_path(),
    ) else {
        return Ok(());
    };

    let auth_path = config.get_vault_auth_path();
    let auth_role = config.get_vault_auth_role();

    crate::vault::ensure_session(&addr, &auth_path, &auth_role)?;
    let email = crate::vault::identity(&addr, &auth_path)?;

    let token_path = crate::vault::substitute_path(&token_path_template, &email);
    info!(
        "Minting JFrog token via Vault (path={}, user={})",
        token_path, email
    );
    let token = crate::vault::read_artifactory_token(&addr, &token_path)?;

    let host = host_from_image_base(&registry.image_base).context(
        "Could not determine push host from rise.toml [registry].image_base (no '/' in value)",
    )?;

    info!("Logging in to {host} as {email}");
    build::docker_login(container_cli, &host, &email, &token)
        .with_context(|| format!("docker login to {host} failed after Vault mint"))?;
    Ok(())
}

/// Return the first slash-separated segment of `image_base` (the registry
/// host). `jfrog.example.com/repo/path` → `jfrog.example.com`.
fn host_from_image_base(image_base: &str) -> Option<String> {
    image_base
        .trim_end_matches('/')
        .split_once('/')
        .map(|(host, _)| host.to_string())
        .or_else(|| {
            // No slash at all — image_base might be just a host with no repo path.
            let trimmed = image_base.trim_end_matches('/');
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        })
}

#[allow(clippy::too_many_arguments)]
async fn call_create_deployment_api(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    project_name: &str,
    image: Option<&str>,
    image_digest: Option<&str>,
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
    replicas: Option<u32>,
    cpu: Option<&str>,
    memory: Option<&str>,
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

    // Add caller-supplied digest if provided (client-controlled push flow)
    if let Some(digest) = image_digest {
        payload["image_digest"] = serde_json::json!(digest);
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

    let mut query_params = vec![];
    let tail_param;
    let since_param;

    if params.follow {
        query_params.push("follow=true");
    }
    if let Some(t) = params.tail {
        tail_param = format!("tail={}", t);
        query_params.push(&tail_param);
    }
    if params.timestamps {
        query_params.push("timestamps=true");
    }
    if let Some(s) = params.since {
        // Parse duration like "5m", "1h" into seconds
        let seconds = parse_duration_to_seconds(s)?;
        since_param = format!("since={}", seconds);
        query_params.push(&since_param);
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

    // Stream response bytes
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();

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
                        let text = String::from_utf8_lossy(&chunk);
                        buffer.push_str(&text);

                        // Process complete lines from buffer
                        while let Some(newline_pos) = buffer.find('\n') {
                            let line = buffer.drain(..=newline_pos).collect::<String>();
                            let line = line.trim_end();

                            // Parse SSE format: lines starting with "data: "
                            if let Some(data) = line.strip_prefix("data: ") {
                                // Only print non-empty data lines
                                if !data.is_empty() {
                                    println!("{}", data);
                                }
                            } else if !line.is_empty() && !line.starts_with(':') {
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

    // Print any remaining buffered content
    if !buffer.is_empty() {
        let line = buffer.trim();
        if let Some(data) = line.strip_prefix("data: ") {
            // Only print non-empty data
            if !data.is_empty() {
                println!("{}", data);
            }
        } else if !line.is_empty() && !line.starts_with(':') {
            println!("{}", line);
        }
    }

    Ok(())
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
    buffer: String,
}

impl LogStream {
    /// Receive the next log line from the stream.
    /// Returns `None` when the stream ends.
    pub async fn recv(&mut self) -> Option<Result<String>> {
        use futures::StreamExt;

        loop {
            // Try to extract a complete line from the buffer
            if let Some(newline_pos) = self.buffer.find('\n') {
                let line: String = self.buffer.drain(..=newline_pos).collect();
                let line = line.trim_end();

                // Parse SSE format: "data: ..." lines contain log content
                if let Some(data) = line.strip_prefix("data: ") {
                    if !data.is_empty() {
                        return Some(Ok(data.to_string()));
                    }
                    continue;
                } else if line.is_empty() || line.starts_with(':') {
                    // SSE comment or empty line, skip
                    continue;
                } else {
                    return Some(Ok(line.to_string()));
                }
            }

            // Need more data from the stream
            match self.stream.next().await {
                Some(Ok(chunk)) => {
                    let text = String::from_utf8_lossy(&chunk);
                    self.buffer.push_str(&text);
                }
                Some(Err(e)) => {
                    return Some(Err(anyhow::anyhow!("Log stream error: {}", e)));
                }
                None => {
                    // Stream ended - process any remaining buffer content
                    if !self.buffer.is_empty() {
                        let remaining = std::mem::take(&mut self.buffer);
                        let line = remaining.trim();
                        if let Some(data) = line.strip_prefix("data: ") {
                            if !data.is_empty() {
                                return Some(Ok(data.to_string()));
                            }
                        } else if !line.is_empty() && !line.starts_with(':') {
                            return Some(Ok(line.to_string()));
                        }
                    }
                    return None;
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
        buffer: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::host_from_image_base;

    #[test]
    fn host_from_image_base_extracts_first_segment() {
        assert_eq!(
            host_from_image_base("jfrog.example.com/org-docker-playground/team-apps").as_deref(),
            Some("jfrog.example.com"),
        );
    }

    #[test]
    fn host_from_image_base_with_port() {
        assert_eq!(
            host_from_image_base("registry.example.com:5000/team/app").as_deref(),
            Some("registry.example.com:5000"),
        );
    }

    #[test]
    fn host_from_image_base_no_slash_returns_whole_string() {
        assert_eq!(
            host_from_image_base("jfrog.example.com").as_deref(),
            Some("jfrog.example.com"),
        );
    }

    #[test]
    fn host_from_image_base_trims_trailing_slash() {
        assert_eq!(
            host_from_image_base("jfrog.example.com/").as_deref(),
            Some("jfrog.example.com"),
        );
    }

    #[test]
    fn host_from_image_base_empty_returns_none() {
        assert_eq!(host_from_image_base(""), None);
    }
}
