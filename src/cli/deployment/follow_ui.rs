use anyhow::{bail, Result};
use reqwest::Client;
use serde::Deserialize;
use std::io::{self, IsTerminal, Write as _};
use std::time::{Duration, Instant};
use tracing::{debug, info};

use crate::api::models::{Deployment, DeploymentStatus};
use crate::config::Config;

use super::core::{fetch_deployment, open_log_stream, parse_duration, LogStreamError};
use crate::token_source::token_with_retry;

// Project info for fetching project URL
#[derive(Deserialize)]
struct ProjectInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    primary_url: Option<String>,
}

// Legacy Docker controller metadata structures (for backward compatibility with old deployments)
#[derive(Deserialize, Debug, Clone, PartialEq)]
struct DockerMetadata {
    #[serde(default)]
    reconcile_phase: ReconcilePhase,
    container_id: Option<String>,
    container_name: Option<String>,
    assigned_port: Option<u16>,
}

#[derive(Deserialize, Debug, Clone, PartialEq, Default)]
enum ReconcilePhase {
    #[default]
    NotStarted,
    CreatingContainer,
    StartingContainer,
    WaitingForHealth,
    Completed,
}

// ANSI escape codes for terminal manipulation
mod ansi {
    pub const CLEAR_LINE: &str = "\x1B[2K";
    pub const HIDE_CURSOR: &str = "\x1B[?25l";
    pub const SHOW_CURSOR: &str = "\x1B[?25h";
    pub const RESET: &str = "\x1B[0m";

    /// Move cursor up n lines
    pub fn move_up(n: usize) -> String {
        format!("\x1B[{}A", n)
    }

    /// Move cursor to beginning of line
    pub const CURSOR_TO_START: &str = "\r";
}

// Spinner animation frames
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// State tracking between polls
struct FollowState {
    last_status: DeploymentStatus,
    last_controller_phase: Option<ReconcilePhase>,
    last_error: Option<String>,
    last_url: Option<String>,
    last_metadata: serde_json::Value,
    spinner_frame: usize,
    is_first_poll: bool,
}

impl FollowState {
    fn new() -> Self {
        Self {
            last_status: DeploymentStatus::Pending,
            last_controller_phase: None,
            last_error: None,
            last_url: None,
            last_metadata: serde_json::Value::Null,
            spinner_frame: 0,
            is_first_poll: true,
        }
    }

    fn should_log_state_change(
        &self,
        deployment: &Deployment,
        controller_phase: &Option<ReconcilePhase>,
    ) -> bool {
        self.is_first_poll
            || self.last_status != deployment.status
            || self.last_controller_phase != *controller_phase
    }

    fn update(&mut self, deployment: &Deployment, controller_phase: Option<ReconcilePhase>) {
        self.last_status = deployment.status.clone();
        self.last_controller_phase = controller_phase;
        self.last_error = deployment.error_message.clone();
        self.last_url = deployment.primary_url.clone();
        self.last_metadata = deployment.controller_metadata.clone();
        self.is_first_poll = false;
    }
}

/// Live status section that gets replaced on each poll
struct LiveStatusSection {
    pub last_line_count: usize,
}

impl LiveStatusSection {
    fn new() -> Self {
        Self { last_line_count: 0 }
    }

    fn clear_previous(&self) {
        if self.last_line_count > 0 {
            // Move cursor up and clear each line
            for _ in 0..self.last_line_count {
                print!(
                    "{}{}{}",
                    ansi::move_up(1),
                    ansi::CURSOR_TO_START,
                    ansi::CLEAR_LINE
                );
            }
            print!("{}", ansi::CURSOR_TO_START);
            io::stdout().flush().unwrap();
        }
    }

    fn render(
        &mut self,
        deployment: &Deployment,
        state: &FollowState,
        controller_phase: &Option<ReconcilePhase>,
    ) -> String {
        // Clear previous output
        self.clear_previous();

        let mut output = String::new();
        let mut line_count = 0;

        // Status line with icon and color
        let icon = status_icon(&deployment.status);
        let color = status_color(&deployment.status);
        let spinner = if is_in_progress(&deployment.status) {
            format!("{} ", spinner_frame(state.spinner_frame))
        } else {
            String::new()
        };

        // Show deployment status + controller phase if available
        let status_text = if let Some(phase) = controller_phase {
            format!("{} ({})", deployment.status, format_controller_phase(phase))
        } else {
            format!("{}", deployment.status)
        };

        output.push_str(&format!(
            "{}{} Status:    {}{}{}\n",
            spinner,
            icon,
            color,
            status_text,
            ansi::RESET
        ));
        line_count += 1;

        // URL if available
        if let Some(ref url) = deployment.primary_url {
            output.push_str(&format!("   URL:       {}\n", url));
            line_count += 1;
        }

        // Error message if present
        if let Some(ref error) = deployment.error_message {
            output.push_str(&format!(
                "   {}Error:{} {}\n",
                "\x1B[31m",
                ansi::RESET,
                error
            ));
            line_count += 1;
        }

        // Controller metadata summary (container ID if available)
        if let Some(container_id) = extract_container_id(&deployment.controller_metadata) {
            output.push_str(&format!("   Container: {}\n", container_id));
            line_count += 1;
        }

        self.last_line_count = line_count;
        output
    }
}

/// Get status color ANSI code
fn status_color(status: &DeploymentStatus) -> &'static str {
    match status {
        DeploymentStatus::Healthy => "\x1B[32m",   // Green
        DeploymentStatus::Failed => "\x1B[31m",    // Red
        DeploymentStatus::Deploying => "\x1B[33m", // Yellow
        DeploymentStatus::Building => "\x1B[36m",  // Cyan
        DeploymentStatus::Pushing => "\x1B[36m",   // Cyan
        DeploymentStatus::Unhealthy => "\x1B[31m", // Red
        DeploymentStatus::Cancelled => "\x1B[90m", // Gray
        DeploymentStatus::Stopped => "\x1B[90m",   // Gray
        _ => "\x1B[37m",                           // White
    }
}

/// Get status icon
fn status_icon(status: &DeploymentStatus) -> &'static str {
    match status {
        DeploymentStatus::Healthy => "✓",
        DeploymentStatus::Failed => "✗",
        DeploymentStatus::Deploying => "⚙",
        DeploymentStatus::Building => "🔨",
        DeploymentStatus::Pushing => "⬆",
        DeploymentStatus::Pushed => "✓",
        DeploymentStatus::Unhealthy => "⚠",
        DeploymentStatus::Cancelled => "⊘",
        DeploymentStatus::Cancelling => "⊘",
        DeploymentStatus::Terminating => "⊘",
        DeploymentStatus::Stopped => "■",
        DeploymentStatus::Superseded => "↻",
        DeploymentStatus::Expired => "⏱",
        DeploymentStatus::Pending => "○",
    }
}

/// Get spinner frame
fn spinner_frame(frame_num: usize) -> &'static str {
    SPINNER_FRAMES[frame_num % SPINNER_FRAMES.len()]
}

/// Check if status is in-progress (should show spinner)
fn is_in_progress(status: &DeploymentStatus) -> bool {
    matches!(
        status,
        DeploymentStatus::Pending
            | DeploymentStatus::Building
            | DeploymentStatus::Pushing
            | DeploymentStatus::Pushed
            | DeploymentStatus::Deploying
            | DeploymentStatus::Cancelling
            | DeploymentStatus::Terminating
    )
}

/// Check if status is terminal
fn is_terminal_state(status: &DeploymentStatus) -> bool {
    matches!(
        status,
        DeploymentStatus::Healthy
            | DeploymentStatus::Failed
            | DeploymentStatus::Cancelled
            | DeploymentStatus::Stopped
            | DeploymentStatus::Superseded
            | DeploymentStatus::Expired
    )
}

/// Parse controller metadata to extract deployment phase info (handles legacy Docker deployments)
fn parse_controller_metadata(metadata: &serde_json::Value) -> Option<DockerMetadata> {
    if metadata.is_null() || metadata == &serde_json::json!({}) {
        return None;
    }

    // Try to parse as Docker metadata (for legacy deployments)
    // For Kubernetes deployments, this will return None, which is fine
    serde_json::from_value::<DockerMetadata>(metadata.clone()).ok()
}

/// Extract container ID from metadata for display
fn extract_container_id(metadata: &serde_json::Value) -> Option<String> {
    parse_controller_metadata(metadata)
        .and_then(|m| m.container_id.map(|id| id[..12.min(id.len())].to_string()))
}

/// Format controller phase for display
fn format_controller_phase(phase: &ReconcilePhase) -> String {
    match phase {
        ReconcilePhase::NotStarted => "not started".to_string(),
        ReconcilePhase::CreatingContainer => "creating container".to_string(),
        ReconcilePhase::StartingContainer => "starting container".to_string(),
        ReconcilePhase::WaitingForHealth => "waiting for health".to_string(),
        ReconcilePhase::Completed => "running".to_string(),
    }
}

/// Log state change to tracing (appears in history)
fn log_state_change(
    project: &str,
    deployment_id: &str,
    status: &DeploymentStatus,
    controller_phase: &Option<ReconcilePhase>,
) {
    let status_text = if let Some(phase) = controller_phase {
        format!("{} ({})", status, format_controller_phase(phase))
    } else {
        format!("{}", status)
    };

    info!("Deployment {}:{} → {}", project, deployment_id, status_text);
}

/// Check if stdout is a TTY
fn is_tty() -> bool {
    io::stdout().is_terminal()
}

/// Print deployment snapshot (for non-follow mode)
pub fn print_deployment_snapshot(deployment: &Deployment) {
    // Parse controller metadata
    let controller_phase =
        parse_controller_metadata(&deployment.controller_metadata).map(|m| m.reconcile_phase);

    // Status line with icon and color
    let icon = status_icon(&deployment.status);
    let color = status_color(&deployment.status);

    // Show deployment status + controller phase if available
    let status_text = if let Some(phase) = controller_phase {
        format!(
            "{} ({})",
            deployment.status,
            format_controller_phase(&phase)
        )
    } else {
        format!("{}", deployment.status)
    };

    println!(
        "{} Status:         {}{}{}",
        icon,
        color,
        status_text,
        ansi::RESET
    );

    // Deployment ID
    println!("   Deployment ID:  {}", deployment.deployment_id);

    // Deployment group (if not default)
    if deployment.deployment_group != "default" {
        println!("   Group:          {}", deployment.deployment_group);
    }

    // Created by
    println!("   Created by:     {}", deployment.created_by_email);

    // Created/Updated timestamps
    println!("   Created:        {}", deployment.created);
    if deployment.updated != deployment.created {
        println!("   Updated:        {}", deployment.updated);
    }

    // Expires at (if set)
    if let Some(ref expires) = deployment.expires_at {
        println!("   Expires at:     {}", expires);
    }

    // Image and digest (if available)
    if let Some(ref image) = deployment.image {
        println!("   Image:          {}", image);
    }
    if let Some(ref digest) = deployment.image_digest {
        println!("   Image digest:   {}", digest);
    }

    // Resources
    println!(
        "   Resources:      {} replicas, cpu={}, memory={}",
        deployment.replicas, deployment.cpu, deployment.memory
    );

    // URL if available
    if let Some(ref url) = deployment.primary_url {
        println!("   URL:            {}", url);
    }

    // Controller metadata summary (container ID if available)
    if let Some(container_id) = extract_container_id(&deployment.controller_metadata) {
        println!("   Container:      {}", container_id);
    }

    // Error message if present
    if let Some(ref error) = deployment.error_message {
        println!("   \x1B[31mError:{} {}", ansi::RESET, error);
    }
}

/// Fetch project info to get project URL
async fn fetch_project_info(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    project: &str,
) -> Result<ProjectInfo> {
    let url = format!("{}/api/v1/projects/{}", backend_url, project);

    let response = http_client.get(&url).bearer_auth(token).send().await?;

    if !response.status().is_success() {
        bail!("Failed to fetch project info");
    }

    let project_info: ProjectInfo = response.json().await?;
    Ok(project_info)
}

/// Check if the deployment status indicates logs should be available for streaming.
fn should_stream_logs(status: &DeploymentStatus) -> bool {
    matches!(
        status,
        DeploymentStatus::Deploying | DeploymentStatus::Unhealthy
    )
}

/// Stream logs from a deployment while monitoring its status.
///
/// Opens an SSE log stream and polls deployment status every 3 seconds.
/// Returns the final deployment when a terminal state is reached.
async fn stream_logs_with_status_polling(
    http_client: &Client,
    backend_url: &str,
    provider: &crate::token_source::TokenProvider,
    project: &str,
    deployment_id: &str,
    timeout: Duration,
    start_time: Instant,
) -> Result<Deployment> {
    let mut log_stream = None;
    let mut retry_count: usize = 0;
    const MAX_RETRIES: usize = 10;
    const RETRY_DELAY: Duration = Duration::from_secs(2);

    let mut status_interval = tokio::time::interval(Duration::from_secs(3));
    status_interval.tick().await; // consume first immediate tick

    // Try initial connection. Resolve a fresh token per request so a long poll
    // (up to the deploy timeout) doesn't outlast a short-lived CI token (#352).
    let token = token_with_retry(provider).await?;
    match open_log_stream(
        http_client,
        backend_url,
        &token,
        project,
        deployment_id,
        100,
    )
    .await
    {
        Ok(s) => log_stream = Some(s),
        Err(LogStreamError::NotReady) => {
            debug!("Initial log stream connection deferred: deployment logs are not ready yet");
        }
        Err(LogStreamError::Gone) => {
            let token = token_with_retry(provider).await?;
            return fetch_deployment(http_client, backend_url, &token, project, deployment_id)
                .await;
        }
        Err(e) => {
            debug!("Initial log stream connection failed: {:?}", e);
        }
    }

    loop {
        if start_time.elapsed() >= timeout {
            bail!(
                "Timeout waiting for deployment to complete after {:?}",
                timeout
            );
        }

        if let Some(ref mut stream) = log_stream {
            tokio::select! {
                biased; // prefer draining log lines over status checks
                line = stream.recv() => {
                    match line {
                        Some(Ok((text, level))) => super::core::print_log_line(&text, &level),
                        Some(Err(e)) => {
                            debug!("Log stream error: {:?}", e);
                            log_stream = None;
                        }
                        None => {
                            debug!("Log stream ended");
                            log_stream = None;
                        }
                    }
                }
                _ = status_interval.tick() => {
                    let token = token_with_retry(provider).await?;
                    let deployment = fetch_deployment(
                        http_client, backend_url, &token, project, deployment_id,
                    ).await?;
                    if is_terminal_state(&deployment.status) {
                        drain_log_stream(stream).await;
                        return Ok(deployment);
                    }
                }
            }
        } else {
            // No active log stream - try to reconnect or poll status
            if retry_count >= MAX_RETRIES {
                debug!("Max log stream retries exceeded, falling back to status-only polling");
                return status_only_polling(
                    http_client,
                    backend_url,
                    provider,
                    project,
                    deployment_id,
                    timeout,
                    start_time,
                )
                .await;
            }

            tokio::select! {
                _ = tokio::time::sleep(RETRY_DELAY) => {
                    let token = token_with_retry(provider).await?;
                    match open_log_stream(
                        http_client, backend_url, &token, project, deployment_id, 100,
                    ).await {
                        Ok(s) => {
                            log_stream = Some(s);
                            retry_count = 0;
                        }
                        Err(LogStreamError::NotReady) => {
                            debug!("Log stream not ready yet; will retry");
                        }
                        Err(LogStreamError::Gone) => {
                            let token = token_with_retry(provider).await?;
                            return fetch_deployment(
                                http_client, backend_url, &token, project, deployment_id,
                            ).await;
                        }
                        Err(e) => {
                            retry_count += 1;
                            debug!("Log stream reconnect failed (attempt {}): {:?}", retry_count, e);
                        }
                    }
                }
                _ = status_interval.tick() => {
                    let token = token_with_retry(provider).await?;
                    let deployment = fetch_deployment(
                        http_client, backend_url, &token, project, deployment_id,
                    ).await?;
                    if is_terminal_state(&deployment.status) {
                        return Ok(deployment);
                    }
                }
            }
        }
    }
}

/// Drain remaining log lines from the log stream, waiting up to 2 seconds.
async fn drain_log_stream(stream: &mut super::core::LogStream) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        tokio::select! {
            line = stream.recv() => {
                match line {
                    Some(Ok((text, level))) => super::core::print_log_line(&text, &level),
                    _ => break,
                }
            }
            _ = tokio::time::sleep_until(deadline) => break,
        }
    }
}

/// Fall back to status-only polling when log streaming is unavailable.
async fn status_only_polling(
    http_client: &Client,
    backend_url: &str,
    provider: &crate::token_source::TokenProvider,
    project: &str,
    deployment_id: &str,
    timeout: Duration,
    start_time: Instant,
) -> Result<Deployment> {
    loop {
        // Resolve a fresh token per poll so a long wait doesn't outlast a
        // short-lived CI token (#352).
        let token = token_with_retry(provider).await?;
        let deployment =
            fetch_deployment(http_client, backend_url, &token, project, deployment_id).await?;
        if is_terminal_state(&deployment.status) {
            return Ok(deployment);
        }
        if start_time.elapsed() >= timeout {
            bail!(
                "Timeout waiting for deployment to complete after {:?}",
                timeout
            );
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// Main follow function with enhanced UX
pub async fn follow_deployment_with_ui(
    http_client: &Client,
    backend_url: &str,
    config: &Config,
    project: &str,
    deployment_id: &str,
    timeout_str: &str,
) -> Result<Deployment> {
    // The follow/poll loop can run for up to ~10 minutes, so resolve a fresh
    // token before each request rather than capturing one up front: in CI the
    // token may be a short-lived OIDC token (see #352). The provider re-mints
    // lazily within a 60s skew of expiry.
    let provider = crate::token_source::resolve_token_provider(http_client, config)?;

    let timeout = parse_duration(timeout_str)?;
    let start_time = Instant::now();

    // Check if we're in a TTY - if not, fall back to simple mode
    if !is_tty() {
        return follow_deployment_simple(
            http_client,
            backend_url,
            &provider,
            project,
            deployment_id,
            timeout,
        )
        .await;
    }

    let mut state = FollowState::new();
    let mut live_section = LiveStatusSection::new();

    // Hide cursor for cleaner output
    print!("{}", ansi::HIDE_CURSOR);
    io::stdout().flush().unwrap();

    // Phase 1: Status polling with spinner UI
    // Poll until deployment reaches Deploying state (logs available) or a terminal state.
    let phase1_result: Result<Deployment> = async {
        loop {
            let token = token_with_retry(&provider).await?;
            let deployment =
                fetch_deployment(http_client, backend_url, &token, project, deployment_id).await?;

            let controller_phase = parse_controller_metadata(&deployment.controller_metadata)
                .map(|m| m.reconcile_phase);

            if state.should_log_state_change(&deployment, &controller_phase) {
                live_section.clear_previous();
                log_state_change(
                    project,
                    deployment_id,
                    &deployment.status,
                    &controller_phase,
                );
                live_section.last_line_count = 0;
            } else {
                let output = live_section.render(&deployment, &state, &controller_phase);
                print!("{}", output);
                io::stdout().flush().unwrap();
            }

            state.update(&deployment, controller_phase);
            state.spinner_frame = (state.spinner_frame + 1) % SPINNER_FRAMES.len();

            // Terminal state reached before Deploying - skip to Phase 3
            if is_terminal_state(&deployment.status) {
                return Ok(deployment);
            }

            // Deploying (or later) - transition to Phase 2 for log streaming
            if should_stream_logs(&deployment.status) {
                return Ok(deployment);
            }

            if start_time.elapsed() >= timeout {
                bail!(
                    "Timeout waiting for deployment to complete after {:?}",
                    timeout
                );
            }

            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
    .await;

    let deployment = match phase1_result {
        Ok(d) => d,
        Err(e) => {
            print!("{}", ansi::SHOW_CURSOR);
            io::stdout().flush().unwrap();
            return Err(e);
        }
    };

    // Phase 2: Log streaming + status monitoring (only if not already terminal)
    let final_deployment = if !is_terminal_state(&deployment.status) {
        // Clear spinner UI and restore cursor
        live_section.clear_previous();
        print!("{}", ansi::SHOW_CURSOR);
        io::stdout().flush().unwrap();

        println!("--- Logs ---");

        stream_logs_with_status_polling(
            http_client,
            backend_url,
            &provider,
            project,
            deployment_id,
            timeout,
            start_time,
        )
        .await?
    } else {
        print!("{}", ansi::SHOW_CURSOR);
        io::stdout().flush().unwrap();
        deployment
    };

    // Phase 3: Print project URL if deployment became active (Healthy in default group)
    if final_deployment.status == DeploymentStatus::Healthy
        && final_deployment.deployment_group == "default"
    {
        let token = token_with_retry(&provider).await?;
        if let Ok(project_info) =
            fetch_project_info(http_client, backend_url, &token, project).await
        {
            if let Some(url) = project_info.primary_url {
                println!();
                println!("Project URL: {}", url);
            }
        }
    }

    Ok(final_deployment)
}

/// Simple fallback for non-TTY environments (pipes, redirects)
async fn follow_deployment_simple(
    http_client: &Client,
    backend_url: &str,
    provider: &crate::token_source::TokenProvider,
    project: &str,
    deployment_id: &str,
    timeout: Duration,
) -> Result<Deployment> {
    let start_time = Instant::now();
    let mut state = FollowState::new();

    // Phase 1: Status polling (print state changes as text lines)
    let deployment = loop {
        let token = token_with_retry(provider).await?;
        let deployment =
            fetch_deployment(http_client, backend_url, &token, project, deployment_id).await?;

        let controller_phase =
            parse_controller_metadata(&deployment.controller_metadata).map(|m| m.reconcile_phase);

        if state.should_log_state_change(&deployment, &controller_phase) {
            log_state_change(
                project,
                deployment_id,
                &deployment.status,
                &controller_phase,
            );
        }

        state.update(&deployment, controller_phase);

        if is_terminal_state(&deployment.status) {
            break deployment;
        }

        if should_stream_logs(&deployment.status) {
            break deployment;
        }

        if start_time.elapsed() >= timeout {
            bail!("Timeout waiting for deployment");
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
    };

    // Phase 2: Log streaming + status monitoring (only if not terminal)
    let final_deployment = if !is_terminal_state(&deployment.status) {
        println!("--- Logs ---");

        stream_logs_with_status_polling(
            http_client,
            backend_url,
            provider,
            project,
            deployment_id,
            timeout,
            start_time,
        )
        .await?
    } else {
        deployment
    };

    // Phase 3: Print project URL if deployment became active (Healthy in default group)
    if final_deployment.status == DeploymentStatus::Healthy
        && final_deployment.deployment_group == "default"
    {
        let token = token_with_retry(provider).await?;
        if let Ok(project_info) =
            fetch_project_info(http_client, backend_url, &token, project).await
        {
            if let Some(url) = project_info.primary_url {
                println!();
                println!("Project URL: {}", url);
            }
        }
    }

    Ok(final_deployment)
}
