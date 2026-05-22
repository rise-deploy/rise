use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct ExchangeTokenResponse {
    token: String,
}

/// Standard in-pod path of the workload-identity bootstrap credential.
const IDENTITY_CREDENTIAL_FILE: &str = "/var/run/secrets/rise/identity/credential";

/// Resolve the bootstrap credential from `--credential`, falling back to the
/// standard credential file mounted into every Rise deployment.
fn resolve_credential(explicit: Option<&str>) -> Result<String> {
    if let Some(c) = explicit {
        return Ok(c.to_string());
    }
    let credential = std::fs::read_to_string(IDENTITY_CREDENTIAL_FILE).with_context(|| {
        format!(
            "No workload identity credential found. Pass --credential, or run this \
             inside a Rise deployment where '{}' is mounted.",
            IDENTITY_CREDENTIAL_FILE
        )
    })?;
    Ok(credential.trim().to_string())
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
