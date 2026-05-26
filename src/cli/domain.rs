use anyhow::{Context, Result};
use comfy_table::{modifiers::UTF8_ROUND_CORNERS, presets::UTF8_FULL, Cell, Table};
use reqwest::Client;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct CustomDomainResponse {
    id: String,
    domain: String,
    #[serde(default)]
    environment: String,
    #[serde(default)]
    is_primary: bool,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Deserialize)]
struct CustomDomainsResponse {
    domains: Vec<CustomDomainResponse>,
}

#[derive(Debug, Serialize)]
struct AddCustomDomainRequest {
    domain: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    environment: Option<String>,
}

/// Add a custom domain to a project
pub async fn add_domain(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    project: &str,
    domain: &str,
    environment: Option<&str>,
) -> Result<()> {
    let url = format!("{}/api/v1/projects/{}/domains", backend_url, project);

    let payload = AddCustomDomainRequest {
        domain: domain.to_string(),
        environment: environment.map(str::to_string),
    };

    let response = http_client
        .post(&url)
        .header("Authorization", format!("Bearer {}", token))
        .json(&payload)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!(
            "Failed to add custom domain (status {}): {}",
            status,
            error_text
        );
    }

    match environment {
        Some(env) => println!(
            "✓ Added custom domain '{}' to project '{}' (environment: {})",
            domain, project, env
        ),
        None => println!(
            "✓ Added custom domain '{}' to project '{}'",
            domain, project
        ),
    }

    Ok(())
}

/// List custom domains for a project
pub async fn list_domains(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    project: &str,
) -> Result<()> {
    let url = format!("{}/api/v1/projects/{}/domains", backend_url, project);

    let response = http_client
        .get(&url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!(
            "Failed to list custom domains (status {}): {}",
            status,
            error_text
        );
    }

    let domains_response: CustomDomainsResponse = response
        .json()
        .await
        .context("Failed to parse domains response")?;

    if domains_response.domains.is_empty() {
        println!("No custom domains configured for project '{}'", project);
        return Ok(());
    }

    // Create a table to display the domains
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_header(vec![
            Cell::new("DOMAIN"),
            Cell::new("ENVIRONMENT"),
            Cell::new("PRIMARY"),
            Cell::new("CREATED AT"),
        ]);

    for domain in &domains_response.domains {
        table.add_row(vec![
            Cell::new(&domain.domain),
            Cell::new(&domain.environment),
            Cell::new(if domain.is_primary { "★" } else { "" }),
            Cell::new(&domain.created_at),
        ]);
    }

    println!("{}", table);

    Ok(())
}

/// Remove a custom domain from a project
pub async fn remove_domain(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    project: &str,
    domain: &str,
) -> Result<()> {
    let url = format!(
        "{}/api/v1/projects/{}/domains/{}",
        backend_url, project, domain
    );

    let response = http_client
        .delete(&url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!(
            "Failed to remove custom domain (status {}): {}",
            status,
            error_text
        );
    }

    println!(
        "✓ Removed custom domain '{}' from project '{}'",
        domain, project
    );

    Ok(())
}
