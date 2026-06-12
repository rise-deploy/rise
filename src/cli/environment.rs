use anyhow::{Context, Result};
use comfy_table::{modifiers::UTF8_ROUND_CORNERS, presets::UTF8_FULL, Attribute, Cell, Table};
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
pub(crate) struct EnvironmentResponse {
    pub(crate) name: String,
    primary_deployment_group: Option<String>,
    is_production: bool,
    color: String,
    #[serde(default)]
    deployment_constraints: Option<EnvironmentDeploymentConstraints>,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Serialize)]
struct CreateEnvironmentRequest {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    primary_deployment_group: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    is_production: Option<bool>,
    color: String,
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
}

pub async fn handle_environment_command(
    http_client: &Client,
    backend_url: &str,
    config: &Config,
    cmd: &crate::EnvironmentCommands,
) -> Result<()> {
    // Every subcommand targets a project; resolve it up front so the token
    // exchange can be scoped to it.
    let (project, path) = match cmd {
        crate::EnvironmentCommands::Create { project, path, .. }
        | crate::EnvironmentCommands::List { project, path }
        | crate::EnvironmentCommands::Show { project, path, .. }
        | crate::EnvironmentCommands::Update { project, path, .. }
        | crate::EnvironmentCommands::Delete { project, path, .. } => (project, path),
    };
    let project_name = crate::resolve_project_name(project.clone(), path)?;
    let token =
        crate::token_source::resolve_token_with_retry(http_client, config, Some(&project_name))
            .await?;

    match cmd {
        crate::EnvironmentCommands::Create {
            name,
            group,
            production,
            color,
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
) -> Result<()> {
    let url = format!("{}/api/v1/projects/{}/environments", backend_url, project);

    let payload = CreateEnvironmentRequest {
        name: name.to_string(),
        primary_deployment_group: group.map(|g| g.to_string()),
        is_production: if is_production { Some(true) } else { None },
        color: color.to_string(),
    };

    let response = http_client
        .post(&url)
        .bearer_auth(token)
        .json(&payload)
        .send()
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

    Ok(())
}

async fn list_environments(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    project: &str,
) -> Result<()> {
    let url = format!("{}/api/v1/projects/{}/environments", backend_url, project);

    let response = http_client
        .get(&url)
        .bearer_auth(token)
        .send()
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

    let envs: Vec<EnvironmentResponse> = response
        .json()
        .await
        .context("Failed to parse environments response")?;

    if envs.is_empty() {
        println!("No environments configured for project '{}'", project);
        return Ok(());
    }

    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_header(vec![
            Cell::new("NAME").add_attribute(Attribute::Bold),
            Cell::new("PRIMARY GROUP").add_attribute(Attribute::Bold),
            Cell::new("PRODUCTION").add_attribute(Attribute::Bold),
            Cell::new("COLOR").add_attribute(Attribute::Bold),
        ]);

    for env in envs {
        table.add_row(vec![
            Cell::new(&env.name),
            Cell::new(env.primary_deployment_group.as_deref().unwrap_or("-")),
            Cell::new(if env.is_production { "yes" } else { "-" }),
            Cell::new(&env.color),
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
        .send()
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

    let payload = UpdateEnvironmentRequest {
        name: rename.map(|n| n.to_string()),
        primary_deployment_group,
        is_production,
        color: color.map(|c| c.to_string()),
    };

    let response = http_client
        .patch(&url)
        .bearer_auth(token)
        .json(&payload)
        .send()
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
        .send()
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
