use anyhow::{bail, Result};
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use std::io::{self, IsTerminal, Write as _};
use std::time::{Duration, Instant};
use tracing::debug;

use crate::api::models::{Deployment, DeploymentEvent, DeploymentStatus};
use crate::config::Config;

use super::core::{
    fetch_deployment, fetch_deployment_events, fetch_latest_deployment_events, open_log_stream,
    parse_duration, LogStreamError,
};
use crate::token_source::token_with_retry;

// Project info for fetching project URL
#[derive(Deserialize)]
struct ProjectInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    primary_url: Option<String>,
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
    spinner_frame: usize,
}

impl FollowState {
    fn new() -> Self {
        Self { spinner_frame: 0 }
    }
}

/// Keeps the deployment event log visible while the deploy command follows a
/// deployment. The initial read includes the complete history; later reads use
/// only the newest page and the event id set makes the output idempotent.
struct EventReporter {
    seen: HashSet<i64>,
    initialized: bool,
}

impl EventReporter {
    fn new() -> Self {
        Self {
            seen: HashSet::new(),
            initialized: false,
        }
    }

    async fn poll(
        &mut self,
        http_client: &Client,
        backend_url: &str,
        provider: &crate::token_source::TokenProvider,
        project: &str,
        deployment_id: &str,
    ) -> Vec<DeploymentEvent> {
        let token = match token_with_retry(provider).await {
            Ok(token) => token,
            Err(error) => {
                debug!(
                    "Could not resolve a token for deployment events: {:?}",
                    error
                );
                return Vec::new();
            }
        };

        let result = if self.initialized {
            fetch_latest_deployment_events(http_client, backend_url, &token, project, deployment_id)
                .await
        } else {
            fetch_deployment_events(http_client, backend_url, &token, project, deployment_id).await
        };

        let events = match result {
            Ok(events) => events,
            Err(error) => {
                debug!("Could not read deployment events: {:?}", error);
                return Vec::new();
            }
        };
        self.initialized = true;

        unseen_events(&mut self.seen, events)
    }
}

fn unseen_events(seen: &mut HashSet<i64>, events: Vec<DeploymentEvent>) -> Vec<DeploymentEvent> {
    let mut new_events: Vec<_> = events
        .into_iter()
        .filter(|event| seen.insert(event.id))
        .collect();
    // The API returns newest-first; command output follows the order in which
    // the server recorded events.
    new_events.reverse();
    new_events
}

fn print_deployment_events(events: &[DeploymentEvent]) {
    for event in events {
        let subject = event
            .subject
            .as_deref()
            .map(|value| format!(" {value}:"))
            .unwrap_or_default();
        let description = event_description(event);
        let attributes = if event.attributes.is_object()
            && event
                .attributes
                .as_object()
                .is_some_and(|attrs| !attrs.is_empty())
        {
            format!(
                " {}",
                serde_json::to_string(&event.attributes).unwrap_or_else(|_| "{}".to_string())
            )
        } else {
            String::new()
        };

        println!(
            "{} [{}] {}{} {}{}",
            event.occurred_at,
            event.severity.to_uppercase(),
            event.source,
            subject,
            description,
            attributes
        );
    }
}

fn event_description(event: &DeploymentEvent) -> String {
    if event.kind == "status_changed" {
        let from = event.attributes.get("from").and_then(event_scalar);
        let to = event.attributes.get("to").and_then(event_scalar);
        if let Some(to) = to {
            return match from {
                Some(from) => format!("status changed: {from} → {to}"),
                None => format!("status: {to}"),
            };
        }
    }

    event
        .message
        .clone()
        .unwrap_or_else(|| event.kind.replace('_', " "))
}

fn event_scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        _ => None,
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

    fn render(&mut self, deployment: &Deployment, state: &FollowState) -> String {
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

        let status_text = format!("{}", deployment.status);

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

/// Check if stdout is a TTY
fn is_tty() -> bool {
    io::stdout().is_terminal()
}

/// Print deployment snapshot (for non-follow mode)
pub fn print_deployment_snapshot(deployment: &Deployment) {
    // Status line with icon and color
    let icon = status_icon(&deployment.status);
    let color = status_color(&deployment.status);

    let status_text = format!("{}", deployment.status);

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
#[allow(clippy::too_many_arguments)]
async fn stream_logs_with_status_polling(
    http_client: &Client,
    backend_url: &str,
    provider: &crate::token_source::TokenProvider,
    project: &str,
    deployment_id: &str,
    timeout: Duration,
    start_time: Instant,
    event_reporter: &mut EventReporter,
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
                    let events = event_reporter
                        .poll(
                            http_client,
                            backend_url,
                            provider,
                            project,
                            deployment_id,
                        )
                        .await;
                    print_deployment_events(&events);
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
                    event_reporter,
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
                    let events = event_reporter
                        .poll(
                            http_client,
                            backend_url,
                            provider,
                            project,
                            deployment_id,
                        )
                        .await;
                    print_deployment_events(&events);
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
#[allow(clippy::too_many_arguments)]
async fn status_only_polling(
    http_client: &Client,
    backend_url: &str,
    provider: &crate::token_source::TokenProvider,
    project: &str,
    deployment_id: &str,
    timeout: Duration,
    start_time: Instant,
    event_reporter: &mut EventReporter,
) -> Result<Deployment> {
    loop {
        // Resolve a fresh token per poll so a long wait doesn't outlast a
        // short-lived CI token (#352).
        let token = token_with_retry(provider).await?;
        let deployment =
            fetch_deployment(http_client, backend_url, &token, project, deployment_id).await?;
        let events = event_reporter
            .poll(http_client, backend_url, provider, project, deployment_id)
            .await;
        print_deployment_events(&events);
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
    let mut event_reporter = EventReporter::new();

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

            let events = event_reporter
                .poll(http_client, backend_url, &provider, project, deployment_id)
                .await;
            if !events.is_empty() {
                live_section.clear_previous();
                live_section.last_line_count = 0;
                print_deployment_events(&events);
            }

            let output = live_section.render(&deployment, &state);
            print!("{}", output);
            io::stdout().flush().unwrap();

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
            &mut event_reporter,
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
    let mut event_reporter = EventReporter::new();

    // Phase 1: Status polling with the event log as the permanent output.
    let deployment = loop {
        let token = token_with_retry(provider).await?;
        let deployment =
            fetch_deployment(http_client, backend_url, &token, project, deployment_id).await?;

        let events = event_reporter
            .poll(http_client, backend_url, provider, project, deployment_id)
            .await;
        print_deployment_events(&events);

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
            &mut event_reporter,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: i64, kind: &str, message: Option<&str>, attributes: Value) -> DeploymentEvent {
        DeploymentEvent {
            id,
            occurred_at: "2026-09-01T09:11:03.969119Z".to_string(),
            kind: kind.to_string(),
            severity: "info".to_string(),
            source: "control-plane".to_string(),
            subject: None,
            message: message.map(str::to_string),
            attributes,
        }
    }

    #[test]
    fn unseen_events_are_returned_in_recorded_order_once() {
        let mut seen = HashSet::from([2]);
        let events = vec![
            event(3, "backend_event", Some("third"), Value::Null),
            event(2, "backend_event", Some("second"), Value::Null),
            event(1, "backend_event", Some("first"), Value::Null),
        ];

        let fresh = unseen_events(&mut seen, events);
        assert_eq!(
            fresh.iter().map(|event| event.id).collect::<Vec<_>>(),
            [1, 3]
        );
        assert!(unseen_events(&mut seen, fresh).is_empty());
    }

    #[test]
    fn status_events_get_a_readable_transition_description() {
        let status = event(
            1,
            "status_changed",
            None,
            serde_json::json!({ "from": "Pending", "to": "Building" }),
        );
        assert_eq!(
            event_description(&status),
            "status changed: Pending → Building"
        );
    }

    #[test]
    fn backend_events_keep_their_message_as_the_description() {
        let backend = event(
            1,
            "backend_event",
            Some("Resources were rounded up"),
            serde_json::json!({ "type": "resource_adjusted" }),
        );
        assert_eq!(event_description(&backend), "Resources were rounded up");
    }
}
