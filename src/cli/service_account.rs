use crate::config::Config;
use anyhow::{Context, Result};
use comfy_table::{modifiers::UTF8_ROUND_CORNERS, presets::UTF8_FULL, Attribute, Cell, Table};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Serialize)]
struct CreateServiceAccountRequest {
    issuer_url: String,
    claims: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct ServiceAccountResponse {
    id: String,
    email: String,
    project_name: String,
    issuer_url: String,
    claims: HashMap<String, String>,
    created_at: String,
}

#[derive(Debug, Deserialize)]
struct ListServiceAccountsResponse {
    service_accounts: Vec<ServiceAccountResponse>,
}

/// Create a new service account for a project
pub async fn create_service_account(
    http_client: &Client,
    backend_url: &str,
    config: &Config,
    project_name: &str,
    issuer_url: &str,
    claims: HashMap<String, String>,
) -> Result<()> {
    let token = crate::token_source::resolve_token_with_retry(http_client, config).await?;

    let url = format!(
        "{}/api/v1/projects/{}/service-accounts",
        backend_url, project_name
    );

    let request_body = CreateServiceAccountRequest {
        issuer_url: issuer_url.to_string(),
        claims,
    };

    let response = http_client
        .post(&url)
        .header("Authorization", format!("Bearer {}", token))
        .json(&request_body)
        .send()
        .await
        .context("Failed to send create service account request")?;

    if response.status().is_success() {
        let sa: ServiceAccountResponse = response
            .json()
            .await
            .context("Failed to parse create service account response")?;

        println!("Created service account:");
        println!("  ID:         {}", sa.id);
        println!("  Email:      {}", sa.email);
        println!("  Project:    {}", sa.project_name);
        println!("  Issuer URL: {}", sa.issuer_url);
        println!("  Claims:");
        for (key, value) in &sa.claims {
            println!("    {}: {}", key, value);
        }
        println!("  Created at: {}", sa.created_at);
    } else {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!(
            "Failed to create service account (status {}): {}",
            status,
            error_text
        );
    }

    Ok(())
}

/// List all service accounts for a project
pub async fn list_service_accounts(
    http_client: &Client,
    backend_url: &str,
    config: &Config,
    project_name: &str,
) -> Result<()> {
    let token = crate::token_source::resolve_token_with_retry(http_client, config).await?;

    let url = format!(
        "{}/api/v1/projects/{}/service-accounts",
        backend_url, project_name
    );

    let response = http_client
        .get(&url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await
        .context("Failed to send list service accounts request")?;

    if response.status().is_success() {
        let list_response: ListServiceAccountsResponse = response
            .json()
            .await
            .context("Failed to parse list service accounts response")?;

        let service_accounts = list_response.service_accounts;

        if service_accounts.is_empty() {
            println!("No service accounts found for project '{}'.", project_name);
        } else {
            let mut table = Table::new();
            table
                .load_preset(UTF8_FULL)
                .apply_modifier(UTF8_ROUND_CORNERS)
                .set_header(vec![
                    Cell::new("ID").add_attribute(Attribute::Bold),
                    Cell::new("EMAIL").add_attribute(Attribute::Bold),
                    Cell::new("ISSUER URL").add_attribute(Attribute::Bold),
                    Cell::new("CLAIMS").add_attribute(Attribute::Bold),
                ]);

            for sa in service_accounts {
                let claims_str = sa
                    .claims
                    .iter()
                    .map(|(k, v)| format!("{}={}", k, v))
                    .collect::<Vec<_>>()
                    .join(", ");

                table.add_row(vec![
                    Cell::new(&sa.id),
                    Cell::new(&sa.email),
                    Cell::new(&sa.issuer_url),
                    Cell::new(&claims_str),
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
            "Failed to list service accounts (status {}): {}",
            status,
            error_text
        );
    }

    Ok(())
}

/// Show details of a specific service account
pub async fn show_service_account(
    http_client: &Client,
    backend_url: &str,
    config: &Config,
    project_name: &str,
    service_account_id: &str,
) -> Result<()> {
    let token = crate::token_source::resolve_token_with_retry(http_client, config).await?;

    let url = format!(
        "{}/api/v1/projects/{}/service-accounts/{}",
        backend_url, project_name, service_account_id
    );

    let response = http_client
        .get(&url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await
        .context("Failed to send show service account request")?;

    if response.status().is_success() {
        let sa: ServiceAccountResponse = response
            .json()
            .await
            .context("Failed to parse show service account response")?;

        println!("Service Account Details:");
        println!("  ID:         {}", sa.id);
        println!("  Email:      {}", sa.email);
        println!("  Project:    {}", sa.project_name);
        println!("  Issuer URL: {}", sa.issuer_url);
        println!("  Claims:");
        for (key, value) in &sa.claims {
            println!("    {}: {}", key, value);
        }
        println!("  Created at: {}", sa.created_at);
    } else {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!(
            "Failed to show service account (status {}): {}",
            status,
            error_text
        );
    }

    Ok(())
}

/// Delete a service account
pub async fn delete_service_account(
    http_client: &Client,
    backend_url: &str,
    config: &Config,
    project_name: &str,
    service_account_id: &str,
) -> Result<()> {
    let token = crate::token_source::resolve_token_with_retry(http_client, config).await?;

    let url = format!(
        "{}/api/v1/projects/{}/service-accounts/{}",
        backend_url, project_name, service_account_id
    );

    let response = http_client
        .delete(&url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await
        .context("Failed to send delete service account request")?;

    if response.status().is_success() {
        println!(
            "Service account '{}' deleted successfully from project '{}'.",
            service_account_id, project_name
        );
    } else {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!(
            "Failed to delete service account (status {}): {}",
            status,
            error_text
        );
    }

    Ok(())
}
