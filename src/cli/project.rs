use crate::api::project::{
    CreateProjectResponse, MeResponse, OwnerInfo, Project, ProjectErrorResponse, ProjectStatus,
    UpdateProjectResponse,
};
use crate::config::Config;
use anyhow::{Context, Result};
use comfy_table::{modifiers::UTF8_ROUND_CORNERS, presets::UTF8_FULL, Attribute, Cell, Table};
use reqwest::Client;
use serde::Serialize;

// Helper function to get current user info
async fn get_current_user(
    http_client: &Client,
    backend_url: &str,
    token: &str,
) -> Result<MeResponse> {
    let url = format!("{}/api/v1/users/me", backend_url);

    let response = http_client
        .get(&url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await
        .context("Failed to get current user")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!(
            "Failed to get current user (status {}): {}",
            status,
            error_text
        );
    }

    let me_response: MeResponse = response
        .json()
        .await
        .context("Failed to parse me response")?;

    Ok(me_response)
}

// Parse owner string (format: "user:email" or "team:name")
fn parse_owner(owner: &str) -> Result<(String, String)> {
    let parts: Vec<&str> = owner.splitn(2, ':').collect();
    if parts.len() != 2 {
        anyhow::bail!("Invalid owner format. Use 'user:email' or 'team:name'");
    }

    let owner_type = parts[0].to_lowercase();
    let owner_value = parts[1].to_string();

    if owner_type != "user" && owner_type != "team" {
        anyhow::bail!("Owner type must be 'user' or 'team'");
    }

    Ok((owner_type, owner_value))
}

// Create a new project
#[allow(clippy::too_many_arguments)]
pub async fn create_project(
    http_client: &Client,
    backend_url: &str,
    config: &Config,
    name: &Option<String>,
    access_class: &str,
    owner: Option<String>,
    source_url: Option<String>,
    path: &str,
    no_rise_toml: bool,
) -> Result<()> {
    use crate::build::config::{
        load_full_project_config, write_project_config, ProjectBuildConfig, ProjectConfig,
    };
    use std::collections::BTreeMap;

    let token = crate::token_source::resolve_token_with_retry(http_client, config, None).await?;

    // Check if rise.toml already exists
    let existing_config = load_full_project_config(path)?;
    let rise_toml_exists = existing_config.is_some();

    // Resolve project name
    let project_name = if let Some(name_str) = name {
        // Name provided as argument
        if let Some(ref full_config) = existing_config {
            if let Some(ref project_config) = full_config.project {
                if project_config.name != *name_str {
                    eprintln!(
                        "Note: rise.toml contains project name '{}', but creating project '{}'. rise.toml was not updated.",
                        project_config.name, name_str
                    );
                }
            }
        }
        name_str.clone()
    } else {
        // No name argument — try to read from existing rise.toml
        let full_config = existing_config.as_ref().ok_or_else(|| {
            anyhow::anyhow!("Project name is required. Provide it as an argument or create a rise.toml with a [project] section.")
        })?;

        let project_config = full_config
            .project
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No [project] section found in rise.toml. Either add a [project] section or provide the project name as an argument."))?;

        project_config.name.clone()
    };

    // Always create project on backend
    let (owner_type, owner_id) = if let Some(owner_str) = owner {
        parse_owner(&owner_str)?
    } else {
        // Default to current user
        let current_user = get_current_user(http_client, backend_url, &token).await?;
        ("user".to_string(), current_user.id)
    };

    #[derive(Serialize)]
    #[serde(rename_all = "snake_case")]
    enum OwnerType {
        User(String),
        Team(String),
    }

    let owner_payload = if owner_type == "user" {
        OwnerType::User(owner_id)
    } else {
        OwnerType::Team(owner_id)
    };

    #[derive(Serialize)]
    struct CreateRequest {
        name: String,
        access_class: String,
        owner: OwnerType,
        #[serde(skip_serializing_if = "Option::is_none")]
        source_url: Option<String>,
    }

    let request = CreateRequest {
        name: project_name.clone(),
        access_class: access_class.to_string(),
        owner: owner_payload,
        source_url: source_url.clone(),
    };

    let url = format!("{}/api/v1/projects", backend_url);
    let response = http_client
        .post(&url)
        .header("Authorization", format!("Bearer {}", token))
        .json(&request)
        .send()
        .await
        .context("Failed to send create project request")?;

    if response.status().is_success() {
        let create_response: CreateProjectResponse = response
            .json()
            .await
            .context("Failed to parse create project response")?;

        println!(
            "✓ Project '{}' created successfully!",
            create_response.project.name
        );
        println!("  ID: {}", create_response.project.id);
        println!("  Status: {}", create_response.project.status);
    } else {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!(
            "Failed to create project (status {}): {}",
            status,
            error_text
        );
    }

    // Write rise.toml only if it doesn't already exist and --no-rise-toml was not set
    if !rise_toml_exists && !no_rise_toml {
        let project_config = ProjectConfig {
            name: project_name.clone(),
            env: BTreeMap::new(),
        };

        let config_to_write = ProjectBuildConfig {
            version: Some(1),
            project: Some(project_config),
            ..Default::default()
        };

        write_project_config(path, &config_to_write)?;
        println!("✓ Created rise.toml at {}/rise.toml", path);
    }

    Ok(())
}

// List all projects
pub async fn list_projects(http_client: &Client, backend_url: &str, config: &Config) -> Result<()> {
    let token = crate::token_source::resolve_token_with_retry(http_client, config, None).await?;

    let url = format!("{}/api/v1/projects", backend_url);
    let response = http_client
        .get(&url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await
        .context("Failed to send list projects request")?;

    if response.status().is_success() {
        let projects: Vec<Project> = response
            .json()
            .await
            .context("Failed to parse list projects response")?;

        if projects.is_empty() {
            println!("No projects found.");
        } else {
            let mut table = Table::new();
            table
                .load_preset(UTF8_FULL)
                .apply_modifier(UTF8_ROUND_CORNERS)
                .set_header(vec![
                    Cell::new("NAME").add_attribute(Attribute::Bold),
                    Cell::new("STATUS").add_attribute(Attribute::Bold),
                    Cell::new("ACCESS CLASS").add_attribute(Attribute::Bold),
                    Cell::new("OWNER").add_attribute(Attribute::Bold),
                    Cell::new("ACTIVE DEPLOYMENT").add_attribute(Attribute::Bold),
                    Cell::new("URL").add_attribute(Attribute::Bold),
                ]);

            for project in projects {
                let url = project.primary_url.as_deref().unwrap_or("(not deployed)");

                // Format active deployment status
                let active_deployment = project
                    .active_deployment_status
                    .as_deref()
                    .unwrap_or("-")
                    .to_string();

                // Format owner
                let owner = match &project.owner {
                    Some(OwnerInfo::User(u)) => format!("user:{}", u.email),
                    Some(OwnerInfo::Team(t)) => format!("team:{}", t.name),
                    None => "-".to_string(),
                };

                table.add_row(vec![
                    Cell::new(&project.name),
                    Cell::new(format!("{}", project.status)),
                    Cell::new(&project.access_class),
                    Cell::new(&owner),
                    Cell::new(&active_deployment),
                    Cell::new(url),
                ]);
            }

            println!("{}", table);
        }
    } else {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!(
            "Failed to list projects (status {}): {}",
            status,
            error_text
        );
    }

    Ok(())
}

// Show project details
pub async fn show_project(
    http_client: &Client,
    backend_url: &str,
    config: &Config,
    project_identifier: &str,
) -> Result<()> {
    let token = crate::token_source::resolve_token_with_retry(http_client, config, None).await?;

    let url = format!("{}/api/v1/projects/{}", backend_url, project_identifier);
    let response = http_client
        .get(&url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await
        .context("Failed to send get project request")?;

    if response.status().is_success() {
        let project: Project = response
            .json()
            .await
            .context("Failed to parse get project response")?;

        println!("Project: {}", project.name);
        println!("ID: {}", project.id);
        println!("Status: {}", project.status);
        println!("Access Class: {}", project.access_class);
        if let Some(url) = project.primary_url {
            println!("Primary URL: {}", url);
        } else {
            println!("Primary URL: (not deployed)");
        }
        if let Some(ref url) = project.source_url {
            println!("Source URL: {}", url);
        }
        if !project.custom_domain_urls.is_empty() {
            println!("Custom Domains:");
            for domain_url in &project.custom_domain_urls {
                println!("  - {}", domain_url);
            }
        }

        println!("\nOwner:");
        if let Some(owner) = project.owner {
            match owner {
                OwnerInfo::User(user) => {
                    println!("  Type: User");
                    println!("  Email: {}", user.email);
                }
                OwnerInfo::Team(team) => {
                    println!("  Type: Team");
                    println!("  Name: {}", team.name);
                }
            }
        } else {
            println!("  (none)");
        }

        // Display deployment groups if any exist
        if let Some(groups) = &project.deployment_groups {
            if !groups.is_empty() {
                println!("\nDeployment Groups:");
                for group in groups {
                    println!("  - {}", group);
                }
            }
        }

        // Display finalizers if any exist
        if !project.finalizers.is_empty() {
            println!("\nFinalizers:");
            for finalizer in &project.finalizers {
                println!("  - {}", finalizer);
            }
        } else if project.status == ProjectStatus::Deleting {
            println!("\nFinalizers: (none - ready for deletion)");
        }

        // Display deployment defaults and constraints
        if let Some(ref defaults) = project.deployment_defaults {
            println!("\nDeployment Defaults:");
            println!(
                "  Replicas: {}, CPU: {}, Memory: {}",
                defaults.replicas, defaults.cpu, defaults.memory
            );
        }
    } else if response.status() == reqwest::StatusCode::NOT_FOUND {
        // Handle 404 with potential fuzzy match suggestions
        let error: ProjectErrorResponse = response
            .json()
            .await
            .context("Failed to parse error response")?;

        eprintln!("{}", error.error);
        if let Some(suggestions) = error.suggestions {
            eprintln!("\nDid you mean one of these?");
            for suggestion in suggestions {
                eprintln!("  - {}", suggestion);
            }
        }
        std::process::exit(1);
    } else {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!("Failed to get project (status {}): {}", status, error_text);
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn update_project(
    http_client: &Client,
    backend_url: &str,
    config: &Config,
    project_identifier: &str,
    name: Option<String>,
    access_class: Option<String>,
    owner: Option<String>,
    source_url: Option<Option<String>>,
) -> Result<()> {
    let token = crate::token_source::resolve_token_with_retry(http_client, config, None).await?;

    #[derive(Serialize)]
    #[serde(rename_all = "snake_case")]
    enum OwnerType {
        User(String),
        Team(String),
    }

    let owner_payload = if let Some(owner_str) = owner {
        let (owner_type, owner_id) = parse_owner(&owner_str)?;
        Some(if owner_type == "user" {
            OwnerType::User(owner_id)
        } else {
            OwnerType::Team(owner_id)
        })
    } else {
        None
    };

    #[derive(Serialize)]
    struct UpdateRequest {
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        access_class: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        owner: Option<OwnerType>,
        #[serde(skip_serializing_if = "Option::is_none")]
        source_url: Option<Option<String>>,
    }

    let request = UpdateRequest {
        name: name.clone(),
        access_class: access_class.clone(),
        owner: owner_payload,
        source_url: source_url.clone(),
    };

    let url = format!("{}/api/v1/projects/{}", backend_url, project_identifier);
    let response = http_client
        .put(&url)
        .header("Authorization", format!("Bearer {}", token))
        .json(&request)
        .send()
        .await
        .context("Failed to send update project request")?;

    if response.status().is_success() {
        let update_response: UpdateProjectResponse = response
            .json()
            .await
            .context("Failed to parse update project response")?;

        println!(
            "✓ Project '{}' updated successfully!",
            update_response.project.name
        );
        println!("  Status: {}", update_response.project.status);
    } else if response.status() == reqwest::StatusCode::NOT_FOUND {
        let error: ProjectErrorResponse = response
            .json()
            .await
            .context("Failed to parse error response")?;

        eprintln!("{}", error.error);
        if let Some(suggestions) = error.suggestions {
            eprintln!("\nDid you mean one of these?");
            for suggestion in suggestions {
                eprintln!("  - {}", suggestion);
            }
        }
        std::process::exit(1);
    } else {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!(
            "Failed to update project (status {}): {}",
            status,
            error_text
        );
    }

    Ok(())
}

// Delete a project
pub async fn delete_project(
    http_client: &Client,
    backend_url: &str,
    config: &Config,
    project_identifier: &str,
) -> Result<()> {
    let token = crate::token_source::resolve_token_with_retry(http_client, config, None).await?;

    let url = format!("{}/api/v1/projects/{}", backend_url, project_identifier);
    let response = http_client
        .delete(&url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await
        .context("Failed to send delete project request")?;

    if response.status() == reqwest::StatusCode::ACCEPTED {
        println!("✓ Project is being deleted (deployments are being cleaned up)");
    } else if response.status() == reqwest::StatusCode::NOT_FOUND {
        let error: ProjectErrorResponse = response
            .json()
            .await
            .context("Failed to parse error response")?;

        eprintln!("{}", error.error);
        if let Some(suggestions) = error.suggestions {
            eprintln!("\nDid you mean one of these?");
            for suggestion in suggestions {
                eprintln!("  - {}", suggestion);
            }
        }
        std::process::exit(1);
    } else {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!(
            "Failed to delete project (status {}): {}",
            status,
            error_text
        );
    }

    Ok(())
}

// Parse app user identifier string (format: "user:email" or "team:name")
fn parse_app_user_identifier(identifier: &str) -> Result<(String, String)> {
    let parts: Vec<&str> = identifier.splitn(2, ':').collect();
    if parts.len() != 2 {
        anyhow::bail!("Invalid identifier format. Use 'user:email' or 'team:name'");
    }

    let identifier_type = parts[0].to_lowercase();
    let identifier_value = parts[1].to_string();

    if identifier_type != "user" && identifier_type != "team" {
        anyhow::bail!("Identifier type must be 'user' or 'team'");
    }

    Ok((identifier_type, identifier_value))
}

/// Add a user or team as an app user (view-only access to deployed app)
pub async fn add_app_user(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    project: &str,
    identifier: &str,
) -> Result<()> {
    use crate::api::project::{UpdateProjectRequest, UpdateProjectResponse};

    let (identifier_type, identifier_value) = parse_app_user_identifier(identifier)?;

    // Step 1: Fetch current project state
    let url = format!("{}/api/v1/projects/{}", backend_url, project);
    let response = http_client
        .get(&url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await
        .context("Failed to fetch project")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!(
            "Failed to fetch project (status {}): {}",
            status,
            error_text
        );
    }

    let current_project: Project = response
        .json()
        .await
        .context("Failed to parse project response")?;

    // Step 2: Build updated lists
    let mut updated_users: Vec<String> = current_project
        .app_users
        .iter()
        .map(|u| u.email.clone())
        .collect();
    let mut updated_teams: Vec<String> = current_project
        .app_teams
        .iter()
        .map(|t| t.name.clone())
        .collect();

    if identifier_type == "user" {
        if updated_users.contains(&identifier_value) {
            anyhow::bail!(
                "User '{}' is already an app user for project '{}'",
                identifier_value,
                project
            );
        }
        updated_users.push(identifier_value.clone());
    } else {
        if updated_teams.contains(&identifier_value) {
            anyhow::bail!(
                "Team '{}' is already an app team for project '{}'",
                identifier_value,
                project
            );
        }
        updated_teams.push(identifier_value.clone());
    }

    // Step 3: Submit updated project via PUT
    let request = UpdateProjectRequest {
        name: None,
        access_class: None,
        owner: None,
        app_users: Some(updated_users),
        app_teams: Some(updated_teams),
        source_url: None,
    };

    let url = format!("{}/api/v1/projects/{}", backend_url, project);
    let response = http_client
        .put(&url)
        .header("Authorization", format!("Bearer {}", token))
        .json(&request)
        .send()
        .await
        .context("Failed to update project")?;

    if response.status().is_success() {
        let _update_response: UpdateProjectResponse = response
            .json()
            .await
            .context("Failed to parse update response")?;
        println!(
            "✓ Added {}:{} as app user for project '{}'",
            identifier_type, identifier_value, project
        );
    } else {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!("Failed to add app user (status {}): {}", status, error_text);
    }

    Ok(())
}

/// Remove a user or team from app users
pub async fn remove_app_user(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    project: &str,
    identifier: &str,
) -> Result<()> {
    use crate::api::project::{UpdateProjectRequest, UpdateProjectResponse};

    let (identifier_type, identifier_value) = parse_app_user_identifier(identifier)?;

    // Step 1: Fetch current project state
    let url = format!("{}/api/v1/projects/{}", backend_url, project);
    let response = http_client
        .get(&url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await
        .context("Failed to fetch project")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!(
            "Failed to fetch project (status {}): {}",
            status,
            error_text
        );
    }

    let current_project: Project = response
        .json()
        .await
        .context("Failed to parse project response")?;

    // Step 2: Build updated lists
    let mut updated_users: Vec<String> = current_project
        .app_users
        .iter()
        .map(|u| u.email.clone())
        .collect();
    let mut updated_teams: Vec<String> = current_project
        .app_teams
        .iter()
        .map(|t| t.name.clone())
        .collect();

    let mut found = false;
    if identifier_type == "user" {
        if let Some(pos) = updated_users.iter().position(|u| u == &identifier_value) {
            updated_users.remove(pos);
            found = true;
        }
    } else if let Some(pos) = updated_teams.iter().position(|t| t == &identifier_value) {
        updated_teams.remove(pos);
        found = true;
    }

    if !found {
        anyhow::bail!(
            "App user {}:{} not found for project '{}'",
            identifier_type,
            identifier_value,
            project
        );
    }

    // Step 3: Submit updated project via PUT
    let request = UpdateProjectRequest {
        name: None,
        access_class: None,
        owner: None,
        app_users: Some(updated_users),
        app_teams: Some(updated_teams),
        source_url: None,
    };

    let url = format!("{}/api/v1/projects/{}", backend_url, project);
    let response = http_client
        .put(&url)
        .header("Authorization", format!("Bearer {}", token))
        .json(&request)
        .send()
        .await
        .context("Failed to update project")?;

    if response.status().is_success() {
        let _update_response: UpdateProjectResponse = response
            .json()
            .await
            .context("Failed to parse update response")?;
        println!(
            "✓ Removed {}:{} from app users for project '{}'",
            identifier_type, identifier_value, project
        );
    } else {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!(
            "Failed to remove app user (status {}): {}",
            status,
            error_text
        );
    }

    Ok(())
}

/// List all app users and teams for a project
pub async fn list_app_users(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    project: &str,
) -> Result<()> {
    let url = format!("{}/api/v1/projects/{}", backend_url, project);
    let response = http_client
        .get(&url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await
        .context("Failed to fetch project")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!(
            "Failed to fetch project (status {}): {}",
            status,
            error_text
        );
    }

    let project_info: Project = response
        .json()
        .await
        .context("Failed to parse project response")?;

    if project_info.app_users.is_empty() && project_info.app_teams.is_empty() {
        println!("No app users or teams configured for project '{}'", project);
        println!("\nApp users can access the deployed application at the ingress level");
        println!("but have no project management permissions.");
        return Ok(());
    }

    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_header(vec![
            Cell::new("TYPE").add_attribute(Attribute::Bold),
            Cell::new("IDENTIFIER").add_attribute(Attribute::Bold),
        ]);

    for user in project_info.app_users {
        table.add_row(vec![Cell::new("user"), Cell::new(&user.email)]);
    }

    for team in project_info.app_teams {
        table.add_row(vec![Cell::new("team"), Cell::new(&team.name)]);
    }

    println!("{}", table);
    println!("\nProject: {}", project);
    println!("Note: App users can access the deployed application but cannot manage the project");

    Ok(())
}
