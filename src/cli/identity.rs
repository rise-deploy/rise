use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct ExchangeTokenResponse {
    token: String,
}

/// Resolve the bootstrap credential from `--credential`, the
/// `RISE_IDENTITY_CREDENTIAL` env var, or the file at
/// `RISE_IDENTITY_CREDENTIAL_FILE` (in that order).
fn resolve_credential(explicit: Option<&str>) -> Result<String> {
    if let Some(c) = explicit {
        return Ok(c.to_string());
    }
    if let Ok(c) = std::env::var("RISE_IDENTITY_CREDENTIAL") {
        if !c.is_empty() {
            return Ok(c);
        }
    }
    if let Ok(path) = std::env::var("RISE_IDENTITY_CREDENTIAL_FILE") {
        let c = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read credential file '{}'", path))?;
        return Ok(c.trim().to_string());
    }
    anyhow::bail!(
        "No workload identity credential found. Pass --credential, or set \
         RISE_IDENTITY_CREDENTIAL or RISE_IDENTITY_CREDENTIAL_FILE."
    )
}

/// Request a workload identity token for `audience` and print it to stdout.
///
/// Uses the bootstrap credential (not a user session) and the `RISE_ISSUER`
/// env var injected into every Rise deployment.
pub async fn token_command(
    http_client: &Client,
    audience: &str,
    credential: Option<&str>,
) -> Result<()> {
    let credential = resolve_credential(credential)?;

    let issuer = std::env::var("RISE_ISSUER")
        .context("RISE_ISSUER is not set; this command runs inside a Rise deployment")?;
    let url = format!("{}/api/v1/identity/token", issuer.trim_end_matches('/'));

    let response = http_client
        .post(&url)
        .header("Authorization", format!("Bearer {}", credential))
        .json(&serde_json::json!({ "audience": audience }))
        .send()
        .await
        .context("Failed to send token exchange request")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!("Token exchange failed (status {}): {}", status, body);
    }

    let token: ExchangeTokenResponse = response
        .json()
        .await
        .context("Failed to parse token exchange response")?;

    println!("{}", token.token);
    Ok(())
}
