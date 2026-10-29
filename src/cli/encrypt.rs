use crate::cli::auth_hint::AuthHintRequest;
use crate::config::Config;
use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
struct EncryptRequest {
    plaintext: String,
    project: String,
    extension_type: String,
    extension: String,
    slot: String,
    binding: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EncryptResponse {
    encrypted: String,
}

#[derive(Debug, Clone, clap::Args)]
pub struct EncryptScope {
    /// Existing project containing the target extension
    #[arg(long)]
    pub project: String,
    /// Extension type, including when the extension has not yet been created
    #[arg(long)]
    pub extension_type: String,
    /// Target extension name
    #[arg(long)]
    pub extension: String,
    /// Credential JSON pointer, for example /spec/client_secret_encrypted
    #[arg(long)]
    pub slot: String,
    /// Opaque consumer binding
    #[arg(long, conflicts_with = "oauth_spec")]
    pub binding: Option<String>,
    /// OAuth configuration JSON; derives its client and effective endpoint binding
    #[arg(long, conflicts_with = "binding")]
    pub oauth_spec: Option<std::path::PathBuf>,
}

/// Encrypt a plaintext secret for one resource and credential slot.
pub async fn encrypt_command(
    config: &Config,
    plaintext: Option<String>,
    scope: &EncryptScope,
) -> Result<()> {
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
    let token = crate::token_source::resolve_token_with_retry(&http_client, config).await?;
    let url = format!("{}/api/v1/encrypt", backend_url);

    if scope.oauth_spec.is_some()
        && (scope.extension_type != "oauth" || scope.slot != "/spec/client_secret_encrypted")
    {
        bail!("--oauth-spec requires --extension-type oauth --slot /spec/client_secret_encrypted");
    }

    let binding = if let Some(path) = &scope.oauth_spec {
        let spec: serde_json::Value = serde_json::from_slice(
            &std::fs::read(path).context("Failed to read OAuth configuration")?,
        )
        .context("Invalid OAuth JSON")?;
        let response: serde_json::Value = http_client
            .post(format!(
                "{}/api/v1/projects/{}/extensions/{}/oauth/credential-binding",
                backend_url,
                urlencoding::encode(&scope.project),
                urlencoding::encode(&scope.extension)
            ))
            .header("Authorization", format!("Bearer {}", token))
            .json(&spec)
            .send_with_auth_hint(&backend_url)
            .await
            .context("Failed to resolve OAuth credential binding")?
            .error_for_status()?
            .json()
            .await?;
        Some(
            response
                .get("binding")
                .and_then(serde_json::Value::as_str)
                .context("Invalid OAuth credential binding response")?
                .to_string(),
        )
    } else {
        scope.binding.clone()
    };

    let response = http_client
        .post(&url)
        .header("Authorization", format!("Bearer {}", token))
        .json(&EncryptRequest {
            plaintext,
            project: scope.project.clone(),
            extension_type: scope.extension_type.clone(),
            extension: scope.extension.clone(),
            slot: scope.slot.clone(),
            binding,
        })
        .send_with_auth_hint(&backend_url)
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
