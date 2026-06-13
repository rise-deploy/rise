use crate::config::Config;
use anyhow::{Context, Result};
use comfy_table::{modifiers::UTF8_ROUND_CORNERS, presets::UTF8_FULL, Attribute, Cell, Table};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Deserialize)]
struct Extension {
    extension: String,
    extension_type: String,
    spec: Value,
    status: Value,
    status_summary: String,
    created: String,
    updated: String,
}

#[derive(Debug, Serialize)]
struct CreateExtensionRequest {
    extension_type: String,
    spec: Value,
}

#[derive(Debug, Deserialize)]
struct CreateExtensionResponse {
    extension: Extension,
}

#[derive(Debug, Serialize)]
struct UpdateExtensionRequest {
    spec: Value,
}

#[derive(Debug, Deserialize)]
struct UpdateExtensionResponse {
    extension: Extension,
}

#[derive(Debug, Deserialize)]
struct ListExtensionsResponse {
    extensions: Vec<Extension>,
}

/// Create or update extension for a project
pub async fn create_extension(
    project: &str,
    extension: &str,
    extension_type: &str,
    spec: Value,
) -> Result<()> {
    let config = Config::load()?;
    let backend_url = config.get_backend_url();
    let http_client = Client::new();
    let token = crate::token_source::resolve_token_with_retry(&http_client, &config).await?;
    let url = format!(
        "{}/api/v1/projects/{}/extensions/{}",
        backend_url, project, extension
    );

    let response = http_client
        .post(&url)
        .header("Authorization", format!("Bearer {}", token))
        .json(&CreateExtensionRequest {
            extension_type: extension_type.to_string(),
            spec,
        })
        .send()
        .await
        .context("Failed to create extension")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!(
            "Failed to create extension (status {}): {}",
            status,
            error_text
        );
    }

    let create_response: CreateExtensionResponse = response
        .json()
        .await
        .context("Failed to parse create extension response")?;

    println!(
        "✓ Created extension '{}' for project '{}'",
        create_response.extension.extension, project
    );
    println!("\nSpec:");
    println!(
        "{}",
        serde_json::to_string_pretty(&create_response.extension.spec)?
    );

    Ok(())
}

/// Update extension for a project (full replace)
pub async fn update_extension(project: &str, extension: &str, spec: Value) -> Result<()> {
    let config = Config::load()?;
    let backend_url = config.get_backend_url();
    let http_client = Client::new();
    let token = crate::token_source::resolve_token_with_retry(&http_client, &config).await?;
    let url = format!(
        "{}/api/v1/projects/{}/extensions/{}",
        backend_url, project, extension
    );

    let response = http_client
        .put(&url)
        .header("Authorization", format!("Bearer {}", token))
        .json(&UpdateExtensionRequest { spec })
        .send()
        .await
        .context("Failed to update extension")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!(
            "Failed to update extension (status {}): {}",
            status,
            error_text
        );
    }

    let update_response: UpdateExtensionResponse = response
        .json()
        .await
        .context("Failed to parse update extension response")?;

    println!(
        "✓ Updated extension '{}' for project '{}'",
        update_response.extension.extension, project
    );
    println!("\nSpec:");
    println!(
        "{}",
        serde_json::to_string_pretty(&update_response.extension.spec)?
    );

    Ok(())
}

/// Patch extension for a project (partial update with null=unset)
pub async fn patch_extension(project: &str, extension: &str, spec: Value) -> Result<()> {
    let config = Config::load()?;
    let backend_url = config.get_backend_url();
    let http_client = Client::new();
    let token = crate::token_source::resolve_token_with_retry(&http_client, &config).await?;
    let url = format!(
        "{}/api/v1/projects/{}/extensions/{}",
        backend_url, project, extension
    );

    let response = http_client
        .patch(&url)
        .header("Authorization", format!("Bearer {}", token))
        .json(&UpdateExtensionRequest { spec })
        .send()
        .await
        .context("Failed to patch extension")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!(
            "Failed to patch extension (status {}): {}",
            status,
            error_text
        );
    }

    let update_response: UpdateExtensionResponse = response
        .json()
        .await
        .context("Failed to parse patch extension response")?;

    println!(
        "✓ Patched extension '{}' for project '{}'",
        update_response.extension.extension, project
    );
    println!("\nSpec:");
    println!(
        "{}",
        serde_json::to_string_pretty(&update_response.extension.spec)?
    );

    Ok(())
}

/// List all extensions for a project
pub async fn list_extensions(project: &str) -> Result<()> {
    let config = Config::load()?;
    let backend_url = config.get_backend_url();
    let http_client = Client::new();
    let token = crate::token_source::resolve_token_with_retry(&http_client, &config).await?;
    let url = format!("{}/api/v1/projects/{}/extensions", backend_url, project);

    let response = http_client
        .get(&url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await
        .context("Failed to list extensions")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!(
            "Failed to list extensions (status {}): {}",
            status,
            error_text
        );
    }

    let list_response: ListExtensionsResponse = response
        .json()
        .await
        .context("Failed to parse list extensions response")?;

    if list_response.extensions.is_empty() {
        println!("No extensions found for project '{}'", project);
        return Ok(());
    }

    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_header(vec![
            Cell::new("NAME").add_attribute(Attribute::Bold),
            Cell::new("TYPE").add_attribute(Attribute::Bold),
            Cell::new("STATUS").add_attribute(Attribute::Bold),
            Cell::new("CREATED").add_attribute(Attribute::Bold),
            Cell::new("UPDATED").add_attribute(Attribute::Bold),
        ]);

    for ext in list_response.extensions {
        table.add_row(vec![
            Cell::new(&ext.extension),
            Cell::new(&ext.extension_type),
            Cell::new(&ext.status_summary),
            Cell::new(&ext.created),
            Cell::new(&ext.updated),
        ]);
    }

    println!("{}", table);
    Ok(())
}

/// Show extension details for a project
pub async fn show_extension(project: &str, extension: &str) -> Result<()> {
    let config = Config::load()?;
    let backend_url = config.get_backend_url();
    let http_client = Client::new();
    let token = crate::token_source::resolve_token_with_retry(&http_client, &config).await?;
    let url = format!(
        "{}/api/v1/projects/{}/extensions/{}",
        backend_url, project, extension
    );

    let response = http_client
        .get(&url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await
        .context("Failed to get extension")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!(
            "Failed to get extension (status {}): {}",
            status,
            error_text
        );
    }

    let ext: Extension = response
        .json()
        .await
        .context("Failed to parse extension response")?;

    println!("Extension: {}", ext.extension);
    println!("Type: {}", ext.extension_type);
    println!("Created: {}", ext.created);
    println!("Updated: {}", ext.updated);
    println!("\nSpec:");
    println!("{}", serde_json::to_string_pretty(&ext.spec)?);
    println!("\nStatus:");
    println!("{}", serde_json::to_string_pretty(&ext.status)?);

    Ok(())
}

/// Delete extension from a project
pub async fn delete_extension(project: &str, extension: &str) -> Result<()> {
    let config = Config::load()?;
    let backend_url = config.get_backend_url();
    let http_client = Client::new();
    let token = crate::token_source::resolve_token_with_retry(&http_client, &config).await?;
    let url = format!(
        "{}/api/v1/projects/{}/extensions/{}",
        backend_url, project, extension
    );

    let response = http_client
        .delete(&url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await
        .context("Failed to delete extension")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!(
            "Failed to delete extension (status {}): {}",
            status,
            error_text
        );
    }

    println!(
        "✓ Extension '{}' marked for deletion from project '{}'",
        extension, project
    );
    println!("The extension will be cleaned up by the reconciliation loop.");

    Ok(())
}
