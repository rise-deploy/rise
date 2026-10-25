use crate::cli::auth_hint::AuthHintRequest;
use anyhow::{Context, Result};
use comfy_table::{presets::UTF8_FULL, Attribute, Cell, Table};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::config::Config;

/// Per-environment deployment constraints
#[derive(Debug, Deserialize)]
struct EnvironmentDeploymentConstraints {
    min_replicas: Option<u32>,
    max_replicas: Option<u32>,
    min_cpu: Option<String>,
    max_cpu: Option<String>,
    min_memory: Option<String>,
    max_memory: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RollbackGuardConfig {
    enabled: bool,
    unhealthy_timeout_seconds: u32,
}

#[derive(Debug, Deserialize)]
pub(crate) struct EnvironmentResponse {
    pub(crate) name: String,
    primary_deployment_group: Option<String>,
    pub(crate) is_production: bool,
    color: String,
    #[serde(default)]
    deployment_constraints: Option<EnvironmentDeploymentConstraints>,
    #[serde(default)]
    max_deployment_expiration: Option<String>,
    /// The only environment deployments may be promoted from, if any.
    #[serde(default)]
    promotion_source_environment: Option<String>,
    /// Absent on servers that predate the Rollback Guard.
    #[serde(default)]
    rollback_guard: Option<RollbackGuardConfig>,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Serialize)]
struct RollbackGuardUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unhealthy_timeout_seconds: Option<u32>,
}

#[derive(Debug, Serialize)]
struct CreateEnvironmentRequest {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    primary_deployment_group: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    is_production: Option<bool>,
    color: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_deployment_expiration: Option<String>,
}

#[derive(Debug, Serialize)]
struct UpdateEnvironmentRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    primary_deployment_group: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    is_production: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_deployment_expiration: Option<Option<String>>,
    /// `Some(None)` clears the promotion source.
    #[serde(skip_serializing_if = "Option::is_none")]
    promotion_source_environment: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rollback_guard: Option<RollbackGuardUpdate>,
}

/// The promotion and Rollback Guard flags of `rise environment update`.
#[derive(Debug, Default)]
pub(crate) struct PromotionPolicyFlags<'a> {
    /// An empty string clears it.
    pub promote_from: Option<&'a str>,
    pub rollback_guard: Option<bool>,
    /// A duration, e.g. `5m`.
    pub rollback_guard_timeout: Option<&'a str>,
}

impl PromotionPolicyFlags<'_> {
    fn promotion_source(&self) -> Option<Option<String>> {
        self.promote_from
            .map(|e| Some(e.trim().to_string()).filter(|e| !e.is_empty()))
    }

    fn rollback_guard_update(&self) -> Result<Option<RollbackGuardUpdate>> {
        let timeout = self
            .rollback_guard_timeout
            .map(|raw| {
                let seconds = crate::cli::deployment::parse_duration_to_seconds(raw)
                    .with_context(|| format!("Invalid --rollback-guard-timeout '{}'", raw))?;
                u32::try_from(seconds)
                    .ok()
                    .filter(|s| *s > 0)
                    .with_context(|| format!("--rollback-guard-timeout '{}' is out of range", raw))
            })
            .transpose()?;
        Ok(
            (self.rollback_guard.is_some() || timeout.is_some()).then_some(RollbackGuardUpdate {
                enabled: self.rollback_guard,
                unhealthy_timeout_seconds: timeout,
            }),
        )
    }
}

/// Whether `env`, as returned by an update, reflects the requested promotion
/// policy. A server without promotion policy ignores the fields and still
/// answers 200.
fn policy_applied(env: &EnvironmentResponse, request: &UpdateEnvironmentRequest) -> bool {
    let source_applied = request
        .promotion_source_environment
        .as_ref()
        .is_none_or(|source| env.promotion_source_environment == *source);
    let guard_applied = request.rollback_guard.as_ref().is_none_or(|update| {
        env.rollback_guard.as_ref().is_some_and(|guard| {
            update.enabled.is_none_or(|e| e == guard.enabled)
                && update
                    .unhealthy_timeout_seconds
                    .is_none_or(|t| t == guard.unhealthy_timeout_seconds)
        })
    });
    source_applied && guard_applied
}

/// `300` → `5m`, `7200` → `2h`, `90` → `90s`.
fn format_seconds(seconds: u32) -> String {
    if seconds > 0 && seconds.is_multiple_of(3600) {
        format!("{}h", seconds / 3600)
    } else if seconds > 0 && seconds.is_multiple_of(60) {
        format!("{}m", seconds / 60)
    } else {
        format!("{}s", seconds)
    }
}

fn rollback_guard_summary(guard: Option<&RollbackGuardConfig>) -> String {
    match guard {
        Some(g) if g.enabled => format!(
            "on (after {} unhealthy)",
            format_seconds(g.unhealthy_timeout_seconds)
        ),
        Some(_) => "off".to_string(),
        None => "-".to_string(),
    }
}

pub async fn handle_environment_command(
    http_client: &Client,
    backend_url: &str,
    config: &Config,
    cmd: &crate::EnvironmentCommands,
) -> Result<()> {
    // Every subcommand targets a project; resolve it once up front and reuse it.
    let (project, path) = match cmd {
        crate::EnvironmentCommands::Create { project, path, .. }
        | crate::EnvironmentCommands::List { project, path }
        | crate::EnvironmentCommands::Show { project, path, .. }
        | crate::EnvironmentCommands::Update { project, path, .. }
        | crate::EnvironmentCommands::Delete { project, path, .. } => (project, path),
    };
    let project_name = crate::resolve_project_name(project.clone(), path)?;
    let token = crate::token_source::resolve_token_with_retry(http_client, config).await?;

    match cmd {
        crate::EnvironmentCommands::Create {
            name,
            group,
            production,
            color,
            max_expiration,
            ..
        } => {
            create_environment(
                http_client,
                backend_url,
                &token,
                &project_name,
                name,
                group.as_deref(),
                *production,
                color,
                max_expiration.as_deref(),
            )
            .await
        }
        crate::EnvironmentCommands::List { .. } => {
            list_environments(http_client, backend_url, &token, &project_name).await
        }
        crate::EnvironmentCommands::Show { name, .. } => {
            show_environment(http_client, backend_url, &token, &project_name, name).await
        }
        crate::EnvironmentCommands::Update {
            name,
            rename,
            group,
            production,
            color,
            max_expiration,
            promote_from,
            rollback_guard,
            rollback_guard_timeout,
            ..
        } => {
            update_environment(
                http_client,
                backend_url,
                &token,
                &project_name,
                name,
                rename.as_deref(),
                group.as_deref(),
                *production,
                color.as_deref(),
                max_expiration.as_deref(),
                PromotionPolicyFlags {
                    promote_from: promote_from.as_deref(),
                    rollback_guard: *rollback_guard,
                    rollback_guard_timeout: rollback_guard_timeout.as_deref(),
                },
            )
            .await
        }
        crate::EnvironmentCommands::Delete { name, .. } => {
            delete_environment(http_client, backend_url, &token, &project_name, name).await
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn create_environment(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    project: &str,
    name: &str,
    group: Option<&str>,
    is_production: bool,
    color: &str,
    max_expiration: Option<&str>,
) -> Result<()> {
    let url = format!("{}/api/v1/projects/{}/environments", backend_url, project);

    let payload = CreateEnvironmentRequest {
        name: name.to_string(),
        primary_deployment_group: group.map(|g| g.to_string()),
        is_production: if is_production { Some(true) } else { None },
        color: color.to_string(),
        max_deployment_expiration: max_expiration.map(|d| d.to_string()),
    };

    let response = http_client
        .post(&url)
        .bearer_auth(token)
        .json(&payload)
        .send_with_auth_hint(backend_url)
        .await
        .context("Failed to create environment")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!("Failed to create environment ({}): {}", status, error_text);
    }

    let env: EnvironmentResponse = response
        .json()
        .await
        .context("Failed to parse environment response")?;

    println!(
        "Created environment '{}' for project '{}'",
        env.name, project
    );
    if let Some(ref g) = env.primary_deployment_group {
        println!("  Primary group: {}", g);
    }
    if env.is_production {
        println!("  Production: yes");
    }
    println!("  Color: {}", env.color);
    println!(
        "  Max expiration: {}",
        env.max_deployment_expiration.as_deref().unwrap_or("none")
    );

    Ok(())
}

/// Fetch every environment of a project.
pub(crate) async fn fetch_environments(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    project: &str,
) -> Result<Vec<EnvironmentResponse>> {
    let url = format!("{}/api/v1/projects/{}/environments", backend_url, project);

    let response = http_client
        .get(&url)
        .bearer_auth(token)
        .send_with_auth_hint(backend_url)
        .await
        .context("Failed to list environments")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!("Failed to list environments ({}): {}", status, error_text);
    }

    response
        .json()
        .await
        .context("Failed to parse environments response")
}

async fn list_environments(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    project: &str,
) -> Result<()> {
    let envs = fetch_environments(http_client, backend_url, token, project).await?;

    if envs.is_empty() {
        println!("No environments configured for project '{}'", project);
        return Ok(());
    }

    let mut table = Table::new();
    table
        .load_style(UTF8_FULL.with_rounded_corners())
        .set_header(vec![
            Cell::new("NAME").add_attribute(Attribute::Bold),
            Cell::new("PRIMARY GROUP").add_attribute(Attribute::Bold),
            Cell::new("PRODUCTION").add_attribute(Attribute::Bold),
            Cell::new("COLOR").add_attribute(Attribute::Bold),
            Cell::new("MAX EXPIRATION").add_attribute(Attribute::Bold),
            Cell::new("PROMOTES FROM").add_attribute(Attribute::Bold),
            Cell::new("ROLLBACK GUARD").add_attribute(Attribute::Bold),
        ]);

    for env in envs {
        table.add_row(vec![
            Cell::new(&env.name),
            Cell::new(env.primary_deployment_group.as_deref().unwrap_or("-")),
            Cell::new(if env.is_production { "yes" } else { "-" }),
            Cell::new(&env.color),
            Cell::new(env.max_deployment_expiration.as_deref().unwrap_or("-")),
            Cell::new(env.promotion_source_environment.as_deref().unwrap_or("-")),
            Cell::new(rollback_guard_summary(env.rollback_guard.as_ref())),
        ]);
    }

    println!("{}", table);

    Ok(())
}

async fn show_environment(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    project: &str,
    name: &str,
) -> Result<()> {
    let url = format!(
        "{}/api/v1/projects/{}/environments/{}",
        backend_url, project, name
    );

    let response = http_client
        .get(&url)
        .bearer_auth(token)
        .send_with_auth_hint(backend_url)
        .await
        .context("Failed to get environment")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!("Failed to get environment ({}): {}", status, error_text);
    }

    let env: EnvironmentResponse = response
        .json()
        .await
        .context("Failed to parse environment response")?;

    println!("Name:           {}", env.name);
    println!(
        "Primary group:  {}",
        env.primary_deployment_group.as_deref().unwrap_or("-")
    );
    println!(
        "Production:     {}",
        if env.is_production { "yes" } else { "no" }
    );
    println!("Color:          {}", env.color);
    println!(
        "Max expiration: {}",
        env.max_deployment_expiration.as_deref().unwrap_or("none")
    );
    println!(
        "Promotes from:  {}",
        env.promotion_source_environment.as_deref().unwrap_or("any")
    );
    println!(
        "Rollback Guard: {}",
        rollback_guard_summary(env.rollback_guard.as_ref())
    );
    if let Some(ref c) = env.deployment_constraints {
        println!("\nDeployment Constraints:");
        println!(
            "  Replicas: {}–{}",
            c.min_replicas.map(|v| v.to_string()).unwrap_or("-".into()),
            c.max_replicas.map(|v| v.to_string()).unwrap_or("-".into()),
        );
        println!(
            "  CPU: {}–{}",
            c.min_cpu.as_deref().unwrap_or("-"),
            c.max_cpu.as_deref().unwrap_or("-"),
        );
        println!(
            "  Memory: {}–{}",
            c.min_memory.as_deref().unwrap_or("-"),
            c.max_memory.as_deref().unwrap_or("-"),
        );
    }
    println!("Created:        {}", env.created_at);
    println!("Updated:        {}", env.updated_at);

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn update_environment(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    project: &str,
    name: &str,
    rename: Option<&str>,
    group: Option<&str>,
    is_production: Option<bool>,
    color: Option<&str>,
    max_expiration: Option<&str>,
    policy: PromotionPolicyFlags<'_>,
) -> Result<()> {
    let url = format!(
        "{}/api/v1/projects/{}/environments/{}",
        backend_url, project, name
    );

    let primary_deployment_group = group.map(|g| {
        if g.is_empty() {
            None
        } else {
            Some(g.to_string())
        }
    });
    let max_deployment_expiration = max_expiration.map(|d| {
        if d.is_empty() {
            None
        } else {
            Some(d.to_string())
        }
    });

    let payload = UpdateEnvironmentRequest {
        name: rename.map(|n| n.to_string()),
        primary_deployment_group,
        is_production,
        color: color.map(|c| c.to_string()),
        max_deployment_expiration,
        promotion_source_environment: policy.promotion_source(),
        rollback_guard: policy.rollback_guard_update()?,
    };

    let response = http_client
        .patch(&url)
        .bearer_auth(token)
        .json(&payload)
        .send_with_auth_hint(backend_url)
        .await
        .context("Failed to update environment")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!("Failed to update environment ({}): {}", status, error_text);
    }

    let env: EnvironmentResponse = response
        .json()
        .await
        .context("Failed to parse environment response")?;

    if !policy_applied(&env, &payload) {
        anyhow::bail!(
            "The server did not apply the promotion policy; it may not support \
             promotion sources or the Rollback Guard"
        );
    }

    println!(
        "Updated environment '{}' in project '{}'",
        env.name, project
    );

    Ok(())
}

async fn delete_environment(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    project: &str,
    name: &str,
) -> Result<()> {
    let url = format!(
        "{}/api/v1/projects/{}/environments/{}",
        backend_url, project, name
    );

    let response = http_client
        .delete(&url)
        .bearer_auth(token)
        .send_with_auth_hint(backend_url)
        .await
        .context("Failed to delete environment")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!("Failed to delete environment ({}): {}", status, error_text);
    }

    println!("Deleted environment '{}' from project '{}'", name, project);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(policy: PromotionPolicyFlags<'_>) -> UpdateEnvironmentRequest {
        UpdateEnvironmentRequest {
            name: None,
            primary_deployment_group: None,
            is_production: None,
            color: None,
            max_deployment_expiration: None,
            promotion_source_environment: policy.promotion_source(),
            rollback_guard: policy.rollback_guard_update().unwrap(),
        }
    }

    #[test]
    fn promotion_flags_map_to_the_update_request() {
        let cases = [
            (
                PromotionPolicyFlags {
                    promote_from: Some("staging"),
                    rollback_guard: Some(true),
                    rollback_guard_timeout: Some("5m"),
                },
                serde_json::json!({
                    "promotion_source_environment": "staging",
                    "rollback_guard": {"enabled": true, "unhealthy_timeout_seconds": 300}
                }),
            ),
            (
                PromotionPolicyFlags {
                    promote_from: Some(""),
                    ..Default::default()
                },
                serde_json::json!({"promotion_source_environment": null}),
            ),
            (
                PromotionPolicyFlags {
                    rollback_guard: Some(false),
                    ..Default::default()
                },
                serde_json::json!({"rollback_guard": {"enabled": false}}),
            ),
            (PromotionPolicyFlags::default(), serde_json::json!({})),
        ];
        for (flags, expected) in cases {
            assert_eq!(serde_json::to_value(request(flags)).unwrap(), expected);
        }
        let invalid = PromotionPolicyFlags {
            rollback_guard_timeout: Some("soon"),
            ..Default::default()
        };
        assert!(invalid.rollback_guard_update().is_err());
    }

    fn env_response(extra: serde_json::Value) -> EnvironmentResponse {
        let mut env = serde_json::json!({
            "name": "production",
            "primary_deployment_group": "default",
            "is_production": true,
            "color": "green",
            "created_at": "2026-10-25T00:00:00Z",
            "updated_at": "2026-10-25T00:00:00Z",
        });
        env.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(env).unwrap()
    }

    #[test]
    fn update_detects_a_server_that_ignored_the_policy() {
        let both = request(PromotionPolicyFlags {
            promote_from: Some("staging"),
            rollback_guard: Some(true),
            ..Default::default()
        });
        let applied = env_response(serde_json::json!({
            "promotion_source_environment": "staging",
            "rollback_guard": {"enabled": true, "unhealthy_timeout_seconds": 600},
        }));
        assert!(policy_applied(&applied, &both));
        assert_eq!(
            rollback_guard_summary(applied.rollback_guard.as_ref()),
            "on (after 10m unhealthy)"
        );

        let old_server = env_response(serde_json::json!({}));
        assert!(!policy_applied(&old_server, &both));
        let guard_only = request(PromotionPolicyFlags {
            rollback_guard: Some(true),
            ..Default::default()
        });
        assert!(!policy_applied(&old_server, &guard_only));
        let clear_source = request(PromotionPolicyFlags {
            promote_from: Some(""),
            ..Default::default()
        });
        assert!(policy_applied(&old_server, &clear_source));
        assert!(policy_applied(&old_server, &request(Default::default())));
    }

    #[test]
    fn rollback_guard_summary_reads_naturally() {
        let guard = |enabled, unhealthy_timeout_seconds| RollbackGuardConfig {
            enabled,
            unhealthy_timeout_seconds,
        };
        assert_eq!(
            rollback_guard_summary(Some(&guard(true, 300))),
            "on (after 5m unhealthy)"
        );
        assert_eq!(rollback_guard_summary(Some(&guard(false, 7200))), "off");
        assert_eq!(rollback_guard_summary(None), "-");
        assert_eq!(format_seconds(7200), "2h");
        assert_eq!(format_seconds(90), "90s");
    }
}
