use anyhow::{anyhow, Context, Result};
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// OIDC Discovery Document
#[derive(Debug, Deserialize)]
struct OidcDiscovery {
    authorization_endpoint: String,
    token_endpoint: String,
}

/// OAuth2 token response from OIDC provider
#[derive(Debug, Serialize, Deserialize)]
pub struct TokenInfo {
    pub access_token: String,
    pub id_token: String,
    pub token_type: String,
    pub expires_in: u64,
}

/// Raw token response from OIDC provider (includes id_token)
#[derive(Debug, Deserialize)]
struct OidcTokenResponse {
    access_token: String,
    token_type: String,
    expires_in: Option<u64>,
    id_token: Option<String>,
    #[allow(dead_code)]
    refresh_token: Option<String>,
}

/// Parameters for building OAuth2 authorization URLs
///
/// Represents the parameters needed to construct an OAuth2 authorization URL.
/// Used by `OAuthClient::build_authorize_url()` to ensure type-safe parameter passing.
#[derive(Debug)]
pub struct AuthorizeParams<'a> {
    pub client_id: &'a str,
    pub redirect_uri: &'a str,
    pub response_type: &'a str,
    pub scope: &'a str,
    pub code_challenge: &'a str,
    pub code_challenge_method: &'a str,
    pub state: Option<&'a str>,
}

/// OAuth2 client for OIDC provider
pub struct OAuthClient {
    client_id: String,
    client_secret: String,
    http_client: HttpClient,
    authorize_url: String,
    token_url: String,
    /// Space-separated OAuth2 scopes requested during login flows.
    scopes: String,
}

impl OAuthClient {
    /// Discover OIDC endpoints from the issuer's .well-known/openid-configuration
    async fn discover_endpoints(
        http_client: &HttpClient,
        issuer: &str,
    ) -> Result<(String, String)> {
        let discovery_url = format!(
            "{}/.well-known/openid-configuration",
            issuer.trim_end_matches('/')
        );

        tracing::debug!("Attempting OIDC discovery from: {}", discovery_url);

        let response = http_client
            .get(&discovery_url)
            .send()
            .await
            .context("Failed to fetch OIDC discovery document")?;

        if !response.status().is_success() {
            tracing::warn!(
                "OIDC discovery failed with status {}, falling back to default endpoints",
                response.status()
            );
            let authorize_url = format!("{}/authorize", issuer.trim_end_matches('/'));
            let token_url = format!("{}/token", issuer.trim_end_matches('/'));
            return Ok((authorize_url, token_url));
        }

        let discovery: OidcDiscovery = response
            .json()
            .await
            .context("Failed to parse OIDC discovery document")?;

        tracing::info!(
            "OIDC discovery successful: authorize_endpoint={}, token_endpoint={}",
            discovery.authorization_endpoint,
            discovery.token_endpoint
        );

        Ok((discovery.authorization_endpoint, discovery.token_endpoint))
    }

    /// Create a new OAuth2 client for the configured OIDC provider
    /// If authorize_url or token_url are not provided, they will be discovered from
    /// the issuer's .well-known/openid-configuration endpoint
    pub async fn new(
        issuer: String,
        client_id: String,
        client_secret: String,
        authorize_url: Option<String>,
        token_url: Option<String>,
        scopes: Vec<String>,
    ) -> Result<Self> {
        let http_client = HttpClient::new();

        // If either URL is missing, attempt OIDC discovery
        let (final_authorize_url, final_token_url) = match (authorize_url, token_url) {
            (Some(auth), Some(token)) => (auth, token),
            (auth_opt, token_opt) => {
                tracing::info!(
                    "One or both OAuth endpoints not configured, attempting OIDC discovery"
                );
                let (discovered_auth, discovered_token) =
                    Self::discover_endpoints(&http_client, &issuer).await?;

                (
                    auth_opt.unwrap_or(discovered_auth),
                    token_opt.unwrap_or(discovered_token),
                )
            }
        };

        tracing::info!(
            "OAuth2 client initialized with authorize_url={}, token_url={}",
            final_authorize_url,
            final_token_url
        );

        Ok(Self {
            client_id,
            client_secret,
            http_client,
            authorize_url: final_authorize_url,
            token_url: final_token_url,
            scopes: scopes.join(" "),
        })
    }

    /// Space-separated OAuth2 scopes requested during login flows.
    pub fn scopes(&self) -> &str {
        &self.scopes
    }

    /// Exchange authorization code for tokens (PKCE flow)
    pub async fn exchange_code_pkce(
        &self,
        code: &str,
        code_verifier: &str,
        redirect_uri: &str,
    ) -> Result<TokenInfo> {
        let token_url = &self.token_url;

        let mut params = HashMap::new();
        params.insert("grant_type", "authorization_code");
        params.insert("code", code);
        params.insert("redirect_uri", redirect_uri);
        params.insert("code_verifier", code_verifier);

        let response = self
            .http_client
            .post(token_url)
            .header("Accept", "application/json")
            .basic_auth(&self.client_id, Some(&self.client_secret))
            .form(&params)
            .send()
            .await
            .context("Failed to exchange authorization code")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(anyhow!(
                "Code exchange failed with status {}: {}",
                status,
                error_text
            ));
        }

        let token_response: OidcTokenResponse = response
            .json()
            .await
            .context("Failed to parse token response")?;

        let id_token = token_response
            .id_token
            .ok_or_else(|| anyhow!("No id_token in response"))?;

        Ok(TokenInfo {
            access_token: token_response.access_token,
            id_token,
            token_type: token_response.token_type,
            expires_in: token_response.expires_in.unwrap_or(3600),
        })
    }

    pub fn authorize_url(&self) -> &str {
        &self.authorize_url
    }

    /// Build authorization URL with typed parameters
    ///
    /// Takes the discovered authorize_url and appends OAuth2 authorization parameters.
    /// Handles existing query parameters correctly (using ? or &).
    /// URL-encodes parameter values for safety.
    ///
    /// This method serves as the single source of truth for constructing
    /// OAuth2 authorization URLs, ensuring all flows use the correctly
    /// discovered endpoint from OIDC.
    ///
    /// # Arguments
    /// * `params` - Typed authorization parameters (client_id, redirect_uri, etc.)
    ///
    /// # Example
    /// ```ignore
    /// let params = AuthorizeParams {
    ///     client_id: "my-client",
    ///     redirect_uri: "https://example.com/callback",
    ///     response_type: "code",
    ///     scope: "openid email profile",
    ///     code_challenge: "...",
    ///     code_challenge_method: "S256",
    ///     state: Some("random-state"),
    /// };
    /// let url = oauth_client.build_authorize_url(&params);
    /// ```
    pub fn build_authorize_url(&self, params: &AuthorizeParams) -> String {
        let mut query_params = vec![
            ("client_id", params.client_id),
            ("redirect_uri", params.redirect_uri),
            ("response_type", params.response_type),
            ("scope", params.scope),
            ("code_challenge", params.code_challenge),
            ("code_challenge_method", params.code_challenge_method),
        ];

        // Add optional state parameter
        if let Some(state) = params.state {
            query_params.push(("state", state));
        }

        let query_string: String = query_params
            .iter()
            .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&");

        if self.authorize_url.contains('?') {
            format!("{}&{}", self.authorize_url, query_string)
        } else {
            format!("{}?{}", self.authorize_url, query_string)
        }
    }
}
