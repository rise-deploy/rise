use crate::config::{normalize_backend_url, Config};
use crate::login::token_utils::{format_token_expiration, log_token_debug};
use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Serialize)]
struct AuthorizeRequest {
    flow: String,
    /// Shown on Rise's confirmation page, so the user can recognize the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    client_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AuthorizeResponse {
    #[serde(default)]
    device_code: Option<String>,
    #[serde(default)]
    user_code: Option<String>,
    #[serde(default)]
    verification_uri: Option<String>,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    interval: Option<u64>,
}

fn default_expires_in() -> u64 {
    600 // 10 minutes
}

fn default_interval() -> u64 {
    5 // 5 seconds
}

/// How much a `slow_down` answer adds to the polling interval (RFC 8628 §3.5).
const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);

/// This machine's hostname, if it can be determined.
fn client_name() -> Option<String> {
    let output = std::process::Command::new("hostname").output().ok()?;
    let name = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (output.status.success() && !name.is_empty()).then_some(name)
}

/// Handle device authorization flow via backend
///
/// Rise is the device authorization server: the user confirms the code on the
/// Rise web UI (signing in there if needed), and this poll then receives a
/// Rise session. Useful where the CLI cannot open a browser or receive the
/// browser flow's localhost callback (SSH sessions, containers).
pub async fn handle_device_flow(
    http_client: &Client,
    backend_url: &str,
    config: &mut Config,
    backend_url_to_save: Option<&str>,
) -> Result<()> {
    let backend_url = normalize_backend_url(backend_url);

    // Step 1: Initialize device flow via backend
    println!("Initializing device authorization flow...");

    let authorize_url = format!("{}/api/v1/auth/authorize", backend_url);
    let authorize_request = AuthorizeRequest {
        flow: "device".to_string(),
        client_name: client_name(),
    };

    let response = http_client
        .post(&authorize_url)
        .json(&authorize_request)
        .send()
        .await
        .context("Failed to initialize device flow")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!(
            "Device flow initialization failed (status {}): {}",
            status,
            error_text
        );
    }

    let device_response: AuthorizeResponse = response
        .json()
        .await
        .context("Failed to parse device flow response")?;

    let device_code = device_response
        .device_code
        .ok_or_else(|| anyhow::anyhow!("No device_code in response"))?;
    let user_code = device_response
        .user_code
        .ok_or_else(|| anyhow::anyhow!("No user_code in response"))?;
    let verification_uri = device_response
        .verification_uri
        .ok_or_else(|| anyhow::anyhow!("No verification_uri in response"))?;
    let expires_in = device_response.expires_in.unwrap_or(default_expires_in());
    let interval = device_response.interval.unwrap_or(default_interval());

    // Step 2: Display user code and open browser
    let verification_url = device_response
        .verification_uri_complete
        .as_ref()
        .unwrap_or(&verification_uri);

    println!("\nTo log in, open this URL and confirm the code:");
    println!("  {}", verification_url);
    println!("\n  Code: {}\n", user_code);
    println!("Only approve if the page shows this same code.");

    if let Err(e) = webbrowser::open(verification_url) {
        println!("Failed to open browser automatically: {}", e);
    }

    // Step 3: Poll backend for authorization via device code exchange
    println!("\nWaiting for authentication...");

    #[derive(Serialize)]
    struct DeviceExchangeRequest {
        device_code: String,
    }

    #[derive(Deserialize)]
    struct DeviceExchangeResponse {
        token: Option<String>,
        #[serde(default)]
        error: Option<String>,
        #[serde(default)]
        error_description: Option<String>,
    }

    let exchange_url = format!("{}/api/v1/auth/device/exchange", backend_url);
    let mut poll_interval = Duration::from_secs(interval);
    let timeout = Duration::from_secs(expires_in);
    let start_time = std::time::Instant::now();

    loop {
        if start_time.elapsed() > timeout {
            anyhow::bail!("Authentication timeout - please try again");
        }

        tokio::time::sleep(poll_interval).await;

        let exchange_request = DeviceExchangeRequest {
            device_code: device_code.clone(),
        };

        let response = http_client
            .post(&exchange_url)
            .json(&exchange_request)
            .send()
            .await
            .context("Failed to poll for device authorization")?;

        let status = response.status();

        if status.is_success() {
            // Successfully got the token
            let exchange_response: DeviceExchangeResponse = response
                .json()
                .await
                .context("Failed to parse device exchange response")?;

            if let Some(token) = exchange_response.token {
                // Store the backend URL if provided
                if let Some(url) = backend_url_to_save {
                    config
                        .set_backend_url(url.to_string())
                        .context("Failed to save backend URL")?;
                }

                // Store the token
                log_token_debug(&token, "device flow response");
                config
                    .set_token(token.clone())
                    .context("Failed to save authentication token")?;

                println!("\n✓ Login successful!");
                println!("  Profile: {}", Config::active_profile_label()?);
                println!("  Token saved to: {}", Config::config_path()?.display());

                // Display token expiration
                match format_token_expiration(&token) {
                    Ok(expiration) => println!("  Token expires: {}", expiration),
                    Err(e) => {
                        // Don't fail the login if we can't parse expiration
                        tracing::debug!("Failed to parse token expiration: {}", e);
                    }
                }

                return Ok(());
            } else if let Some(error) = exchange_response.error {
                let description = exchange_response.error_description.unwrap_or_default();
                match error.as_str() {
                    // Keep polling; `server_error` is a transient backend failure.
                    "authorization_pending" | "slow_down" | "server_error" => {
                        if error == "slow_down" {
                            poll_interval += SLOW_DOWN_STEP;
                        }
                        print!(".");
                        use std::io::Write;
                        std::io::stdout().flush()?;
                    }
                    "access_denied" => {
                        anyhow::bail!("Login was denied: {}", description)
                    }
                    "expired_token" => anyhow::bail!(
                        "The login code expired or was already used. Run `rise login --device` again."
                    ),
                    _ => anyhow::bail!("Device authorization failed: {} - {}", error, description),
                }
            }
        } else {
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            anyhow::bail!(
                "Device token request failed with status {}: {}",
                status,
                error_text
            );
        }
    }
}
