use crate::config::{normalize_backend_url, Config};
use crate::login::token_utils::{format_token_expiration, log_token_debug};
use anyhow::{Context, Result};
use axum::{extract::Query, response::IntoResponse, routing::get, Router};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::net::Ipv4Addr;
use std::time::Duration;
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

/// What the browser's redirect delivered: the authorization code (or the
/// IdP's error), and where to send the browser once the CLI knows how the
/// login ended.
struct Callback {
    code: Result<String>,
    reply: oneshot::Sender<String>,
}

/// How long the browser waits on the callback for the CLI to finish the login.
const CALLBACK_REPLY_TIMEOUT: Duration = Duration::from_secs(120);

/// The local server receiving the OAuth redirect.
struct CallbackServer {
    redirect_uri: String,
    callback: oneshot::Receiver<Callback>,
    shutdown: oneshot::Sender<()>,
    serving: tokio::task::JoinHandle<()>,
}

impl CallbackServer {
    /// Stop accepting connections and give the browser's response a moment
    /// to be written before the CLI exits.
    async fn finish(self) {
        let _ = self.shutdown.send(());
        let _ = tokio::time::timeout(Duration::from_secs(5), self.serving).await;
    }
}

/// The CLI success page, showing `error` when the login failed and the
/// granted `access` (see [`super::access::access_page_param`]) when known.
fn cli_result_url(backend_url: &str, outcome: std::result::Result<Option<&str>, &str>) -> String {
    let base = format!("{backend_url}/api/v1/auth/cli-success");
    match outcome {
        Ok(None) => format!("{base}?success=true"),
        Ok(Some(access)) => format!("{base}?success=true&access={}", urlencoding::encode(access)),
        Err(error) => format!("{base}?success=false&error={}", urlencoding::encode(error)),
    }
}

/// Start local HTTP server to receive OAuth callback
async fn start_callback_server(backend_url: &str) -> Result<CallbackServer> {
    start_callback_server_on_ports(backend_url, &[8765, 8766, 8767]).await
}

async fn start_callback_server_on_ports(
    backend_url: &str,
    ports: &[u16],
) -> Result<CallbackServer> {
    use std::sync::Arc;

    // Try multiple ports in case one is in use
    let mut last_error = None;
    let backend_url = backend_url.to_string();

    for &port in ports {
        let (tx, rx) = oneshot::channel();
        let tx = Arc::new(tokio::sync::Mutex::new(Some(tx)));

        let app = Router::new().route(
            "/callback",
            get({
                let tx = Arc::clone(&tx);
                let backend_url = backend_url.clone();
                move |Query(params): Query<CallbackParams>| async move {
                    use axum::response::Redirect;

                    let code = if let Some(code) = params.code {
                        Ok(code)
                    } else if let Some(error) = params.error {
                        Err(anyhow::anyhow!(
                            "OAuth error: {} - {}",
                            error,
                            params.error_description.unwrap_or_default()
                        ))
                    } else {
                        Err(anyhow::anyhow!("No code or error in callback"))
                    };

                    // Only the first callback counts; the browser is answered
                    // once the CLI has finished the login with it.
                    let Some(sender) = tx.lock().await.take() else {
                        return Redirect::to(&cli_result_url(
                            &backend_url,
                            Err("This login has already been completed"),
                        ))
                        .into_response();
                    };
                    let (reply, outcome) = oneshot::channel();
                    let _ = sender.send(Callback { code, reply });
                    let target = match tokio::time::timeout(CALLBACK_REPLY_TIMEOUT, outcome).await {
                        Ok(Ok(target)) => target,
                        _ => cli_result_url(
                            &backend_url,
                            Err("The CLI did not finish the login; check your terminal"),
                        ),
                    };
                    Redirect::to(&target).into_response()
                }
            }),
        );

        // The listener and redirect must use the same IP so a browser cannot
        // reach a different service through localhost's other address family.
        match tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
            Ok(listener) => {
                let redirect_uri = format!("http://{}/callback", listener.local_addr()?);
                let (shutdown, shutdown_rx) = oneshot::channel::<()>();
                // Successfully bound, start the server in the background
                let serving = tokio::spawn(async move {
                    let _ = axum::serve(listener, app)
                        .with_graceful_shutdown(async {
                            let _ = shutdown_rx.await;
                        })
                        .await;
                });
                return Ok(CallbackServer {
                    redirect_uri,
                    callback: rx,
                    shutdown,
                    serving,
                });
            }
            Err(e) => {
                last_error = Some(e);
            }
        }
    }

    Err(anyhow::anyhow!(
        "Failed to bind to any port (tried {:?}): {}",
        ports,
        last_error.context("No callback ports configured")?
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
    #[serde(skip_serializing_if = "Option::is_none")]
    access: Option<serde_json::Value>,
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
    access: Option<serde_json::Value>,
) -> Result<()> {
    let backend_url = normalize_backend_url(backend_url);
    super::access::warn_if_narrowing(&backend_url, access.as_ref());

    // Step 1: Discover OpenID endpoints
    tracing::debug!("Discovering authentication endpoints...");
    let discovery = discover_endpoints(http_client, &backend_url)
        .await
        .context("Failed to discover authentication endpoints")?;

    // Step 2: Generate PKCE codes
    let (code_verifier, code_challenge) = generate_pkce_challenge();

    // Step 3: Start local callback server
    let mut server = start_callback_server(&backend_url)
        .await
        .context("Failed to start local callback server")?;
    let redirect_uri = server.redirect_uri.clone();

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

    let callback = match tokio::time::timeout(Duration::from_secs(300), &mut server.callback).await
    {
        Ok(Ok(callback)) => callback,
        Ok(Err(_)) => {
            server.finish().await;
            anyhow::bail!("Failed to receive authorization code");
        }
        Err(_) => {
            server.finish().await;
            anyhow::bail!("Timeout waiting for authentication");
        }
    };

    let outcome = match callback.code {
        Ok(code) => {
            println!("✓ Received authorization code");
            println!("Exchanging authorization code for token...");
            exchange_and_save(
                http_client,
                &discovery.token_endpoint,
                &backend_url,
                config,
                CodeExchangeRequest {
                    code,
                    code_verifier,
                    redirect_uri,
                    access,
                },
            )
            .await
        }
        Err(e) => Err(LoginError::Other(e)),
    };

    // The browser waits on the callback until it learns how the login ended.
    let (page, result) = match outcome {
        Ok(token) => {
            let granted =
                super::access::fetch_granted_access(http_client, &backend_url, &token).await;
            let param = granted.as_ref().map(super::access::access_page_param);
            (
                cli_result_url(&backend_url, Ok(param.as_deref())),
                Ok((token, granted)),
            )
        }
        Err(e) => (cli_result_url(&backend_url, Err(&e.page_message())), Err(e)),
    };
    let _ = callback.reply.send(page);
    server.finish().await;

    let (token, granted) = match result {
        Ok(done) => done,
        Err(LoginError::PlatformAccessDenied(error_text)) => {
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
        Err(LoginError::Other(e)) => return Err(e),
    };

    println!("✓ Login successful!");
    println!("  Profile: {}", Config::active_profile_label()?);
    println!(
        "  Token saved to: {}",
        Config::credential_path(backend_url.as_ref())?.display()
    );

    // Display token expiration
    match format_token_expiration(&token) {
        Ok(expiration) => println!("  Token expires: {}", expiration),
        Err(e) => {
            // Don't fail the login if we can't parse expiration
            tracing::debug!("Failed to parse token expiration: {}", e);
        }
    }
    if let Some(access) = &granted {
        super::access::print_access(access);
    }

    Ok(())
}

/// Why the code flow failed after the browser came back.
enum LoginError {
    /// The user authenticated but may not use the platform (403 on exchange).
    PlatformAccessDenied(String),
    Other(anyhow::Error),
}

impl LoginError {
    /// The message the CLI result page shows in the browser.
    fn page_message(&self) -> String {
        match self {
            LoginError::PlatformAccessDenied(_) => {
                "Your account does not have permission to use the Rise platform".to_string()
            }
            LoginError::Other(e) => format!("{e:#}"),
        }
    }
}

/// Exchange the code for a session and save it, returning the token.
async fn exchange_and_save(
    http_client: &Client,
    token_endpoint: &str,
    backend_url: &str,
    config: &mut Config,
    request: CodeExchangeRequest,
) -> std::result::Result<String, LoginError> {
    let response = http_client
        .post(token_endpoint)
        .json(&request)
        .send()
        .await
        .context("Failed to exchange code with backend")
        .map_err(LoginError::Other)?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        if status == reqwest::StatusCode::FORBIDDEN {
            return Err(LoginError::PlatformAccessDenied(error_text));
        }
        return Err(LoginError::Other(anyhow::anyhow!(
            "Code exchange failed (status {}): {}",
            status,
            error_text
        )));
    }

    let exchange_response: CodeExchangeResponse = response
        .json()
        .await
        .context("Failed to parse code exchange response")
        .map_err(LoginError::Other)?;

    log_token_debug(&exchange_response.token, "OAuth login response");
    config
        .save_login(backend_url, exchange_response.token.clone())
        .context("Failed to save authentication token")
        .map_err(LoginError::Other)?;
    Ok(exchange_response.token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::net::TcpListener;

    fn test_client() -> Client {
        Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn callback_skips_occupied_ipv4_ports_and_receives_code() {
        let client = test_client();

        // Ephemeral ports isolate the test from local apps and parallel tests.
        for occupied_count in 0..3 {
            let mut occupied = Vec::new();
            let mut ports = Vec::new();
            for _ in 0..occupied_count {
                let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
                ports.push(listener.local_addr().unwrap().port());
                occupied.push(listener);
            }
            ports.resize(3, 0);
            let server = start_callback_server_on_ports("https://rise.example.com", &ports)
                .await
                .unwrap();
            let redirect = url::Url::parse(&server.redirect_uri).unwrap();
            assert_eq!(redirect.host_str(), Some("127.0.0.1"));
            assert_eq!(redirect.path(), "/callback");
            assert!(!ports.contains(&redirect.port().unwrap()));

            let browser = tokio::spawn(
                client
                    .get(format!("{}?code=test-code", server.redirect_uri))
                    .send(),
            );
            let callback = tokio::time::timeout(Duration::from_secs(5), server.callback)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(callback.code.unwrap(), "test-code");
            // The browser is answered only once the CLI says how the login ended.
            assert!(!browser.is_finished());
            let page = cli_result_url("https://rise.example.com", Ok(Some("app: deploy")));
            callback.reply.send(page).unwrap();

            let response = browser.await.unwrap().unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::SEE_OTHER);
            assert_eq!(
                response.headers()[reqwest::header::LOCATION],
                "https://rise.example.com/api/v1/auth/cli-success?success=true&access=app%3A%20deploy"
            );
        }
    }

    #[tokio::test]
    async fn callback_passes_idp_errors_and_shows_the_cli_error() {
        let server = start_callback_server_on_ports("https://rise.example.com", &[0])
            .await
            .unwrap();
        let browser = tokio::spawn(
            test_client()
                .get(format!(
                    "{}?error=access_denied&error_description=nope",
                    server.redirect_uri
                ))
                .send(),
        );
        let callback = server.callback.await.unwrap();
        let error = callback.code.unwrap_err().to_string();
        assert_eq!(error, "OAuth error: access_denied - nope");
        callback
            .reply
            .send(cli_result_url("https://rise.example.com", Err(&error)))
            .unwrap();

        let response = browser.await.unwrap().unwrap();
        assert_eq!(
            response.headers()[reqwest::header::LOCATION],
            "https://rise.example.com/api/v1/auth/cli-success?success=false&error=OAuth%20error%3A%20access_denied%20-%20nope"
        );
    }

    #[tokio::test]
    async fn callback_fails_when_all_ipv4_ports_are_occupied() {
        let mut occupied = Vec::new();
        let mut ports = Vec::new();
        for _ in 0..3 {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            ports.push(listener.local_addr().unwrap().port());
            occupied.push(listener);
        }

        let Err(error) = start_callback_server_on_ports("https://rise.example.com", &ports).await
        else {
            panic!("bound a callback server although every port is taken");
        };
        assert!(error.to_string().contains("Failed to bind to any port"));
    }
}
