use crate::config::Config;
use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
struct EncryptRequest {
    plaintext: String,
}

#[derive(Debug, Deserialize)]
struct EncryptResponse {
    encrypted: String,
}

/// Encrypt a plaintext secret for use in extension specs
pub async fn encrypt_command(config: &Config, plaintext: Option<String>) -> Result<()> {
    // Read from stdin if no argument provided
    let plaintext = match plaintext {
        Some(p) => p,
        None => {
            use std::io::{IsTerminal, Read};

            // If stdin is a TTY, print helpful message
            if std::io::stdin().is_terminal() {
                eprintln!("Enter secret to encrypt (press Ctrl+D when done):");
            }

            let mut buffer = String::new();
            std::io::stdin()
                .read_to_string(&mut buffer)
                .map_err(|e| anyhow::anyhow!("Failed to read from stdin: {}", e))?;
            buffer.trim().to_string()
        }
    };

    if plaintext.is_empty() {
        bail!("Plaintext cannot be empty");
    }

    let http_client = Client::new();
    let backend_url = config.get_backend_url();
    // No project context: `rise encrypt` targets the global encrypt endpoint,
    // so an external token (if any) stays on the legacy path.
    let token = crate::token_source::resolve_token_with_retry(&http_client, config, None).await?;
    let url = format!("{}/api/v1/encrypt", backend_url);

    let response = http_client
        .post(&url)
        .header("Authorization", format!("Bearer {}", token))
        .json(&EncryptRequest { plaintext })
        .send()
        .await
        .context("Failed to call encrypt endpoint")?;

    let status = response.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        bail!("Rate limit exceeded. Please try again later (100 requests per hour).");
    }

    if !status.is_success() {
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        bail!("Failed to encrypt (status {}): {}", status, error_text);
    }

    let encrypt_response: EncryptResponse = response
        .json()
        .await
        .context("Failed to parse encrypt response")?;

    println!("{}", encrypt_response.encrypted);
    Ok(())
}
