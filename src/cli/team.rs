use crate::config::Config;
use anyhow::{Context, Result};
use comfy_table::{modifiers::UTF8_ROUND_CORNERS, presets::UTF8_FULL, Attribute, Cell, Table};
use reqwest::Client;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
struct MeResponse {
    #[allow(dead_code)]
    id: String,
    email: String,
}

#[derive(Debug, Serialize)]
struct UsersLookupRequest {
    emails: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct UserInfo {
    id: String,
    email: String,
}

#[derive(Debug, Deserialize)]
struct UsersLookupResponse {
    users: Vec<UserInfo>,
}

// Helper function to lookup user IDs from emails
async fn lookup_users(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    emails: Vec<String>,
) -> Result<Vec<String>> {
    if emails.is_empty() {
        return Ok(Vec::new());
    }

    let url = format!("{}/api/v1/users/lookup", backend_url);
    let request = UsersLookupRequest {
        emails: emails.clone(),
    };

    let response = http_client
        .post(&url)
        .header("Authorization", format!("Bearer {}", token))
        .json(&request)
        .send()
        .await
        .context("Failed to lookup users")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!("Failed to lookup users (status {}): {}", status, error_text);
    }

    let lookup_response: UsersLookupResponse = response
        .json()
        .await
        .context("Failed to parse lookup response")?;

    Ok(lookup_response.users.into_iter().map(|u| u.id).collect())
}

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

#[derive(Debug, Deserialize, Serialize)]
struct Team {
    id: String,
    name: String,
    members: Vec<UserInfo>,
    owners: Vec<UserInfo>,
    #[serde(default)]
    idp_managed: bool,
}

#[derive(Debug, Deserialize)]
struct TeamErrorResponse {
    error: String,
    suggestions: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct CreateTeamResponse {
    team: Team,
}

#[derive(Debug, Deserialize)]
struct UpdateTeamResponse {
    team: Team,
}

// Create a new team
pub async fn create_team(
    http_client: &Client,
    backend_url: &str,
    config: &Config,
    name: &str,
    owners: Option<Vec<String>>,
    members: Vec<String>,
) -> Result<()> {
    let token = crate::token_source::resolve_token_with_retry(http_client, config, None).await?;

    // If no owners specified, use current user
    let owner_emails = if let Some(emails) = owners {
        emails
    } else {
        let current_user = get_current_user(http_client, backend_url, &token).await?;
        vec![current_user.email]
    };

    // Convert email addresses to user IDs
    let owner_ids = lookup_users(http_client, backend_url, &token, owner_emails).await?;
    let member_ids = lookup_users(http_client, backend_url, &token, members).await?;

    #[derive(Serialize)]
    struct CreateRequest {
        name: String,
        owners: Vec<String>,
        members: Vec<String>,
    }

    let request = CreateRequest {
        name: name.to_string(),
        owners: owner_ids,
        members: member_ids,
    };

    let url = format!("{}/api/v1/teams", backend_url);
    let response = http_client
        .post(&url)
        .header("Authorization", format!("Bearer {}", token))
        .json(&request)
        .send()
        .await
        .context("Failed to send create team request")?;

    if response.status().is_success() {
        let create_response: CreateTeamResponse = response
            .json()
            .await
            .context("Failed to parse create team response")?;

        println!(
            "✓ Team '{}' created successfully!",
            create_response.team.name
        );
        println!("  ID: {}", create_response.team.id);
        println!(
            "  Owners: {}",
            create_response
                .team
                .owners
                .iter()
                .map(|u| u.email.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        println!(
            "  Members: {}",
            create_response
                .team
                .members
                .iter()
                .map(|u| u.email.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    } else {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!("Failed to create team (status {}): {}", status, error_text);
    }

    Ok(())
}

// List all teams
pub async fn list_teams(http_client: &Client, backend_url: &str, config: &Config) -> Result<()> {
    let token = crate::token_source::resolve_token_with_retry(http_client, config, None).await?;

    let url = format!("{}/api/v1/teams", backend_url);
    let response = http_client
        .get(&url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await
        .context("Failed to send list teams request")?;

    if response.status().is_success() {
        let teams: Vec<Team> = response
            .json()
            .await
            .context("Failed to parse list teams response")?;

        if teams.is_empty() {
            println!("No teams found.");
        } else {
            let mut table = Table::new();
            table
                .load_preset(UTF8_FULL)
                .apply_modifier(UTF8_ROUND_CORNERS)
                .set_header(vec![
                    Cell::new("NAME").add_attribute(Attribute::Bold),
                    Cell::new("ID").add_attribute(Attribute::Bold),
                    Cell::new("OWNERS").add_attribute(Attribute::Bold),
                    Cell::new("MEMBERS").add_attribute(Attribute::Bold),
                ]);

            for team in teams {
                // Add (IdP) indicator for IdP-managed teams
                let team_name = if team.idp_managed {
                    format!("{} (IdP)", team.name)
                } else {
                    team.name.clone()
                };

                table.add_row(vec![
                    Cell::new(&team_name),
                    Cell::new(&team.id),
                    Cell::new(team.owners.len()),
                    Cell::new(team.members.len()),
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
        anyhow::bail!("Failed to list teams (status {}): {}", status, error_text);
    }

    Ok(())
}

// Show team details
pub async fn show_team(
    http_client: &Client,
    backend_url: &str,
    config: &Config,
    team_identifier: &str,
) -> Result<()> {
    let token = crate::token_source::resolve_token_with_retry(http_client, config, None).await?;

    let url = format!("{}/api/v1/teams/{}", backend_url, team_identifier);
    let response = http_client
        .get(&url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await
        .context("Failed to send get team request")?;

    if response.status().is_success() {
        let team: Team = response
            .json()
            .await
            .context("Failed to parse get team response")?;

        let team_name = if team.idp_managed {
            format!("{} (IdP-managed)", team.name)
        } else {
            team.name.clone()
        };
        println!("Team: {}", team_name);
        println!("ID: {}", team.id);

        // Display owners table
        println!("\nOwners:");
        if team.owners.is_empty() {
            println!("  (none)");
        } else {
            let mut owners_table = Table::new();
            owners_table
                .load_preset(UTF8_FULL)
                .apply_modifier(UTF8_ROUND_CORNERS)
                .set_header(vec![
                    Cell::new("EMAIL").add_attribute(Attribute::Bold),
                    Cell::new("ID").add_attribute(Attribute::Bold),
                ]);

            for owner in &team.owners {
                owners_table.add_row(vec![Cell::new(&owner.email), Cell::new(&owner.id)]);
            }

            println!("{}", owners_table);
        }

        // Display members table
        println!("\nMembers:");
        if team.members.is_empty() {
            println!("  (none)");
        } else {
            let mut members_table = Table::new();
            members_table
                .load_preset(UTF8_FULL)
                .apply_modifier(UTF8_ROUND_CORNERS)
                .set_header(vec![
                    Cell::new("EMAIL").add_attribute(Attribute::Bold),
                    Cell::new("ID").add_attribute(Attribute::Bold),
                ]);

            for member in &team.members {
                members_table.add_row(vec![Cell::new(&member.email), Cell::new(&member.id)]);
            }

            println!("{}", members_table);
        }
    } else if response.status() == reqwest::StatusCode::NOT_FOUND {
        // Handle 404 with potential fuzzy match suggestions
        let error: TeamErrorResponse = response
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
        anyhow::bail!("Failed to get team (status {}): {}", status, error_text);
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn update_team(
    http_client: &Client,
    backend_url: &str,
    config: &Config,
    team_identifier: &str,
    name: Option<String>,
    add_owners: Vec<String>,
    remove_owners: Vec<String>,
    add_members: Vec<String>,
    remove_members: Vec<String>,
) -> Result<()> {
    let token = crate::token_source::resolve_token_with_retry(http_client, config, None).await?;

    // Convert email addresses to user IDs
    let add_owner_ids = lookup_users(http_client, backend_url, &token, add_owners).await?;
    let remove_owner_ids = lookup_users(http_client, backend_url, &token, remove_owners).await?;
    let add_member_ids = lookup_users(http_client, backend_url, &token, add_members).await?;
    let remove_member_ids = lookup_users(http_client, backend_url, &token, remove_members).await?;

    // First, get the current team state (without expand for now)
    let get_url = format!("{}/api/v1/teams/{}", backend_url, team_identifier);
    let get_response = http_client
        .get(&get_url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await
        .context("Failed to get current team state")?;

    if !get_response.status().is_success() {
        if get_response.status() == reqwest::StatusCode::NOT_FOUND {
            let error: TeamErrorResponse = get_response
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
        }
        anyhow::bail!("Team not found");
    }

    let team: Team = get_response
        .json()
        .await
        .context("Failed to parse team response")?;

    // Work with owner/member IDs
    let mut owner_ids: Vec<String> = team.owners.iter().map(|u| u.id.clone()).collect();
    let mut member_ids: Vec<String> = team.members.iter().map(|u| u.id.clone()).collect();
    let mut updated_name = team.name.clone();

    // Apply changes
    for owner in add_owner_ids {
        if !owner_ids.contains(&owner) {
            owner_ids.push(owner);
        }
    }
    for owner in remove_owner_ids {
        owner_ids.retain(|o| o != &owner);
    }
    for member in add_member_ids {
        if !member_ids.contains(&member) {
            member_ids.push(member);
        }
    }
    for member in remove_member_ids {
        member_ids.retain(|m| m != &member);
    }

    // Update name if provided
    if let Some(new_name) = name {
        updated_name = new_name;
    }

    #[derive(Serialize)]
    struct UpdateRequest {
        name: Option<String>,
        owners: Option<Vec<String>>,
        members: Option<Vec<String>>,
    }

    let request = UpdateRequest {
        name: Some(updated_name),
        owners: Some(owner_ids),
        members: Some(member_ids),
    };

    let url = format!("{}/api/v1/teams/{}", backend_url, team.id);
    let response = http_client
        .put(&url)
        .header("Authorization", format!("Bearer {}", token))
        .json(&request)
        .send()
        .await
        .context("Failed to send update team request")?;

    if response.status().is_success() {
        let update_response: UpdateTeamResponse = response
            .json()
            .await
            .context("Failed to parse update team response")?;

        println!(
            "✓ Team '{}' updated successfully!",
            update_response.team.name
        );
        println!(
            "  Owners: {}",
            update_response
                .team
                .owners
                .iter()
                .map(|u| u.email.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        println!(
            "  Members: {}",
            update_response
                .team
                .members
                .iter()
                .map(|u| u.email.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    } else {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!("Failed to update team (status {}): {}", status, error_text);
    }

    Ok(())
}

// Delete a team
pub async fn delete_team(
    http_client: &Client,
    backend_url: &str,
    config: &Config,
    team_identifier: &str,
) -> Result<()> {
    let token = crate::token_source::resolve_token_with_retry(http_client, config, None).await?;

    let url = format!("{}/api/v1/teams/{}", backend_url, team_identifier);
    let response = http_client
        .delete(&url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await
        .context("Failed to send delete team request")?;

    if response.status().is_success() {
        println!("✓ Team deleted successfully!");
    } else if response.status() == reqwest::StatusCode::NOT_FOUND {
        let error: TeamErrorResponse = response
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
    } else if response.status() == reqwest::StatusCode::CONFLICT {
        let error: TeamErrorResponse = response
            .json()
            .await
            .context("Failed to parse error response")?;
        eprintln!("Error: {}", error.error);
        std::process::exit(1);
    } else {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!("Failed to delete team (status {}): {}", status, error_text);
    }

    Ok(())
}
