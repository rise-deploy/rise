use crate::config::{normalize_backend_url, Config};
use crate::login::token_utils::{format_token_expiration, log_token_debug};
use anyhow::{Context, Result};
use axum::{extract::Query, response::IntoResponse, routing::get, Router};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;
use tracing;

/// Generate PKCE code_verifier and code_challenge
fn generate_pkce_challenge() -> (String, String) {
    // Generate random code_verifier (43-128 characters)
    let random_bytes: Vec<u8> = (0..32).map(|_| rand::random::<u8>()).collect();
    let code_verifier = URL_SAFE_NO_PAD.encode(&random_bytes);

    // Calculate code_challenge = BASE64URL(SHA256(code_verifier))
    let mut hasher = Sha256::new();
    hasher.update(code_verifier.as_bytes());
    let hash = hasher.finalize();
    let code_challenge = URL_SAFE_NO_PAD.encode(hash);

    (code_verifier, code_challenge)
}

#[derive(Debug, Deserialize)]
struct CallbackParams {
    code: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// Start local HTTP server to receive OAuth callback
async fn start_callback_server(
    backend_url: &str,
) -> Result<(String, tokio::sync::oneshot::Receiver<Result<String>>)> {
    use std::sync::Arc;

    // Try multiple ports in case one is in use
    let ports = vec![8765, 8766, 8767];
    let mut last_error = None;
    let backend_url = backend_url.to_string();

    for port in ports {
        let redirect_uri = format!("http://localhost:{}/callback", port);
        let (tx, rx) = oneshot::channel();
        let tx = Arc::new(tokio::sync::Mutex::new(Some(tx)));

        let app = Router::new().route(
            "/callback",
            get({
                let tx = Arc::clone(&tx);
                let backend_url = backend_url.clone();
                move |Query(params): Query<CallbackParams>| async move {
                    use axum::response::Redirect;

                    let (result, response) = if let Some(code) = params.code {
                        // Success - redirect to backend success page
                        let success_url =
                            format!("{}/api/v1/auth/cli-success?success=true", backend_url);
                        (Ok(code), Redirect::to(&success_url).into_response())
                    } else if let Some(error) = params.error {
                        // Error - redirect to backend error page
                        let error_msg = format!(
                            "{} - {}",
                            error,
                            params.error_description.unwrap_or_default()
                        );
                        let error_url = format!(
                            "{}/api/v1/auth/cli-success?success=false&error={}",
                            backend_url,
                            urlencoding::encode(&error_msg)
                        );
                        (
                            Err(anyhow::anyhow!("OAuth error: {}", error_msg)),
                            Redirect::to(&error_url).into_response(),
                        )
                    } else {
                        // No code or error - redirect to backend error page
                        let error_url = format!(
                            "{}/api/v1/auth/cli-success?success=false&error={}",
                            backend_url,
                            urlencoding::encode("No code or error in callback")
                        );
                        (
                            Err(anyhow::anyhow!("No code or error in callback")),
                            Redirect::to(&error_url).into_response(),
                        )
                    };

                    // Send result through channel
                    if let Some(sender) = tx.lock().await.take() {
                        let _ = sender.send(result);
                    }

                    response
                }
            }),
        );

        // Try to bind to this port
        let addr = format!("localhost:{}", port);
        match tokio::net::TcpListener::bind(&addr).await {
            Ok(listener) => {
                // Successfully bound, start the server in the background
                tokio::spawn(async move {
                    let _ = axum::serve(listener, app).await;
                });
                return Ok((redirect_uri, rx));
            }
            Err(e) => {
                last_error = Some(e);
            }
        }
    }

    Err(anyhow::anyhow!(
        "Failed to bind to any port (tried 8765-8767): {}",
        last_error.unwrap()
    ))
}

#[derive(Debug, Serialize)]
struct AuthorizeRequest {
    flow: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    redirect_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code_challenge: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code_challenge_method: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AuthorizeResponse {
    authorization_url: Option<String>,
}

#[derive(Debug, Serialize)]
struct CodeExchangeRequest {
    code: String,
    code_verifier: String,
    redirect_uri: String,
}

#[derive(Debug, Deserialize)]
struct CodeExchangeResponse {
    token: String,
}

/// OpenID Connect Discovery document (subset of fields we need)
#[derive(Debug, Deserialize)]
struct OpenIdDiscovery {
    authorization_endpoint: String,
    token_endpoint: String,
}

/// Discover OpenID endpoints from the server's .well-known/openid-configuration
async fn discover_endpoints(http_client: &Client, backend_url: &str) -> Result<OpenIdDiscovery> {
    let discovery_url = format!("{}/.well-known/openid-configuration", backend_url);

    let response = http_client
        .get(&discovery_url)
        .send()
        .await
        .context("Failed to fetch OpenID discovery document")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!(
            "Failed to fetch OpenID discovery (status {}): {}",
            status,
            error_text
        );
    }

    let discovery: OpenIdDiscovery = response
        .json()
        .await
        .context("Failed to parse OpenID discovery document")?;

    Ok(discovery)
}

/// Handle OAuth2 authorization code flow with PKCE
pub async fn handle_authorization_code_flow(
    http_client: &Client,
    backend_url: &str,
    config: &mut Config,
    backend_url_to_save: Option<&str>,
) -> Result<()> {
    let backend_url = normalize_backend_url(backend_url);

    // Step 1: Discover OpenID endpoints
    tracing::debug!("Discovering authentication endpoints...");
    let discovery = discover_endpoints(http_client, &backend_url)
        .await
        .context("Failed to discover authentication endpoints")?;

    // Step 2: Generate PKCE codes
    let (code_verifier, code_challenge) = generate_pkce_challenge();

    // Step 3: Start local callback server
    let (redirect_uri, code_receiver) = start_callback_server(&backend_url)
        .await
        .context("Failed to start local callback server")?;

    // Step 4: Request authorization URL from backend
    println!("Requesting authorization URL from backend...");

    let authorize_request = AuthorizeRequest {
        flow: "code".to_string(),
        redirect_uri: Some(redirect_uri.clone()),
        code_challenge: Some(code_challenge.clone()),
        code_challenge_method: Some("S256".to_string()),
    };

    let response = http_client
        .post(&discovery.authorization_endpoint)
        .json(&authorize_request)
        .send()
        .await
        .context("Failed to request authorization URL from backend")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!(
            "Failed to get authorization URL (status {}): {}",
            status,
            error_text
        );
    }

    let authorize_response: AuthorizeResponse = response
        .json()
        .await
        .context("Failed to parse authorization URL response")?;

    let auth_url = authorize_response
        .authorization_url
        .ok_or_else(|| anyhow::anyhow!("No authorization URL in response"))?;

    // Step 4: Open browser
    println!("Opening browser to authenticate...");
    println!("If the browser doesn't open, visit: {}", auth_url);

    if let Err(e) = webbrowser::open(auth_url.as_str()) {
        println!("Failed to open browser automatically: {}", e);
    }

    // Step 5: Wait for callback
    println!("\nWaiting for authentication...");

    let code = tokio::time::timeout(
        std::time::Duration::from_secs(300), // 5 minute timeout
        code_receiver,
    )
    .await
    .context("Timeout waiting for authentication")??
    .context("Failed to receive authorization code")?;

    println!("✓ Received authorization code");

    // Step 6: Exchange code with backend
    println!("Exchanging authorization code for token...");

    let exchange_request = CodeExchangeRequest {
        code,
        code_verifier,
        redirect_uri,
    };

    let response = http_client
        .post(&discovery.token_endpoint)
        .json(&exchange_request)
        .send()
        .await
        .context("Failed to exchange code with backend")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());

        // Special handling for platform access denial
        if status == reqwest::StatusCode::FORBIDDEN {
            eprintln!("\n{}", "=".repeat(70));
            eprintln!("Platform Access Denied");
            eprintln!("{}", "=".repeat(70));
            eprintln!("\n{}\n", error_text);
            eprintln!("You authenticated successfully, but your account does not have");
            eprintln!("permission to use the Rise platform (CLI/API/Dashboard).");
            eprintln!("\nYour account is configured for application access only.");
            eprintln!("\nIf you believe this is an error, please contact your administrator.");
            eprintln!("{}\n", "=".repeat(70));
            std::process::exit(1);
        }

        anyhow::bail!("Code exchange failed (status {}): {}", status, error_text);
    }

    let exchange_response: CodeExchangeResponse = response
        .json()
        .await
        .context("Failed to parse code exchange response")?;

    // Store the backend URL if provided
    if let Some(url) = backend_url_to_save {
        config
            .set_backend_url(url.to_string())
            .context("Failed to save backend URL")?;
    }

    // Store the token
    log_token_debug(&exchange_response.token, "OAuth login response");
    config
        .set_token(exchange_response.token.clone())
        .context("Failed to save authentication token")?;

    println!("✓ Login successful!");
    println!("  Profile: {}", Config::active_profile_label()?);
    println!("  Token saved to: {}", Config::config_path()?.display());

    // Display token expiration
    match format_token_expiration(&exchange_response.token) {
        Ok(expiration) => println!("  Token expires: {}", expiration),
        Err(e) => {
            // Don't fail the login if we can't parse expiration
            tracing::debug!("Failed to parse token expiration: {}", e);
        }
    }

    Ok(())
}
