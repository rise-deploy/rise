use crate::db::{projects, users};
use crate::server::auth::{
    cookie_helpers,
    token_storage::{
        generate_code_challenge, generate_code_verifier, generate_state_token,
        CompletedAuthSession, OAuth2State,
    },
};
use crate::server::frontend::load_auth_template;
use crate::server::state::AppState;
use axum::{
    extract::{Query, State},
    http::{header, uri::Uri, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    Json,
};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::instrument;

/// Build the project URL for the `aud` claim from the ingress URL template
///
/// Returns `None` if no ingress URL template is configured.
fn build_project_url(state: &AppState, project_name: &str) -> Option<String> {
    let template = state.production_ingress_url_template.as_ref()?;
    let resolved = template.replace("{project_name}", project_name);
    // Split host from optional path prefix (e.g. "rise.dev/myapp" → host="rise.dev", path="/myapp")
    let (host, path) = match resolved.find('/') {
        Some(pos) => (&resolved[..pos], &resolved[pos..]),
        None => (resolved.as_str(), ""),
    };
    match state.ingress_port {
        Some(port) => Some(format!(
            "{}://{}:{}{}",
            state.ingress_schema, host, port, path
        )),
        None => Some(format!("{}://{}{}", state.ingress_schema, host, path)),
    }
}

/// Validate and sanitize a redirect URL to prevent open redirect vulnerabilities
///
/// This function ensures that redirect URLs are safe before using them in templates
/// or JavaScript redirects. It prevents:
/// - Open redirects to arbitrary external sites
/// - JavaScript execution via javascript: URLs
/// - Data URL exploits
/// - Other dangerous URL schemes
///
/// # Arguments
/// * `redirect_url` - The redirect URL from user input (query params)
/// * `public_url` - The Rise public URL (trusted domain)
///
/// # Returns
/// A safe redirect URL, or "/" if the input is invalid
///
/// # Security
/// - Relative paths starting with "/" are always allowed
/// - Absolute URLs must be HTTPS (or HTTP for localhost/development)
/// - Absolute URLs must match the Rise public domain
/// - All dangerous schemes (javascript:, data:, vbscript:, etc.) are blocked
/// - Invalid or suspicious URLs default to "/"
fn validate_redirect_url(redirect_url: &str, public_url: &str) -> String {
    const SAFE_FALLBACK: &str = "/";

    // Empty or whitespace-only URLs default to safe fallback
    let redirect_url = redirect_url.trim();
    if redirect_url.is_empty() {
        return SAFE_FALLBACK.to_string();
    }

    // Allow relative paths that start with /
    if redirect_url.starts_with('/') {
        // Additional safety: ensure it doesn't start with // (protocol-relative URL)
        if redirect_url.starts_with("//") {
            tracing::warn!(
                redirect_url = %redirect_url,
                "Blocked protocol-relative URL in redirect"
            );
            return SAFE_FALLBACK.to_string();
        }
        return redirect_url.to_string();
    }

    // Try to parse as absolute URL
    let parsed_redirect = match url::Url::parse(redirect_url) {
        Ok(url) => url,
        Err(e) => {
            tracing::warn!(
                redirect_url = %redirect_url,
                error = ?e,
                "Failed to parse redirect URL, using safe fallback"
            );
            return SAFE_FALLBACK.to_string();
        }
    };

    // Block dangerous schemes
    let scheme = parsed_redirect.scheme().to_lowercase();
    if !matches!(scheme.as_str(), "http" | "https") {
        tracing::warn!(
            redirect_url = %redirect_url,
            scheme = %scheme,
            "Blocked dangerous URL scheme in redirect"
        );
        return SAFE_FALLBACK.to_string();
    }

    // Parse the trusted public URL
    let parsed_public = match url::Url::parse(public_url) {
        Ok(url) => url,
        Err(e) => {
            tracing::error!(
                public_url = %public_url,
                error = ?e,
                "Failed to parse public_url, blocking redirect"
            );
            return SAFE_FALLBACK.to_string();
        }
    };

    // Extract host for comparison
    let redirect_host = match parsed_redirect.host_str() {
        Some(host) => host,
        None => {
            tracing::warn!(
                redirect_url = %redirect_url,
                "Redirect URL has no host, using safe fallback"
            );
            return SAFE_FALLBACK.to_string();
        }
    };

    let public_host = match parsed_public.host_str() {
        Some(host) => host,
        None => {
            tracing::error!(
                public_url = %public_url,
                "Public URL has no host, blocking redirect"
            );
            return SAFE_FALLBACK.to_string();
        }
    };

    // Allow redirects to the same host as public_url
    if redirect_host == public_host {
        return redirect_url.to_string();
    }

    // Allow redirects to subdomains of the public domain
    // e.g., if public_url is "https://rise.dev", allow "https://app.rise.dev"
    if redirect_host.ends_with(&format!(".{}", public_host)) {
        return redirect_url.to_string();
    }

    // Allow localhost and 127.0.0.1 for development (only if public_url is also local)
    // Extract host without port for comparison
    let redirect_host_base = redirect_host.split(':').next().unwrap_or(redirect_host);
    let public_host_base = public_host.split(':').next().unwrap_or(public_host);

    let is_redirect_localhost =
        redirect_host_base == "localhost" || redirect_host_base == "127.0.0.1";
    let is_public_localhost = public_host_base == "localhost" || public_host_base == "127.0.0.1";

    if is_redirect_localhost && is_public_localhost {
        return redirect_url.to_string();
    }

    // All other external URLs are blocked
    tracing::warn!(
        redirect_url = %redirect_url,
        redirect_host = %redirect_host,
        public_host = %public_host,
        "Blocked redirect to untrusted external domain"
    );

    SAFE_FALLBACK.to_string()
}

/// Helper function to sync IdP groups after login
///
/// This validates the token and syncs the user's team memberships from IdP groups.
/// Should be called during login flows (code exchange, device exchange, OAuth callback).
async fn sync_groups_after_login(
    state: &AppState,
    id_token: &str,
) -> Result<(), (StatusCode, String)> {
    // Only sync if enabled
    if !state.auth_settings.idp_group_sync_enabled {
        return Ok(());
    }

    // Build expected claims for validation
    let mut expected_claims = HashMap::new();
    expected_claims.insert("aud".to_string(), state.auth_settings.client_id.clone());

    // Validate token to get claims
    let claims_value = state
        .jwt_validator
        .validate(id_token, &state.auth_settings.issuer, &expected_claims)
        .await
        .map_err(|e| {
            tracing::warn!("Failed to validate token for group sync: {:#}", e);
            (StatusCode::UNAUTHORIZED, format!("Invalid token: {}", e))
        })?;

    // Parse claims
    let claims: crate::server::auth::jwt::Claims =
        serde_json::from_value(claims_value).map_err(|e| {
            tracing::warn!("Failed to parse claims for group sync: {:#}", e);
            (
                StatusCode::UNAUTHORIZED,
                format!("Invalid token claims: {}", e),
            )
        })?;

    // Get or create user. Always pairs the user row with a default-Org
    // membership so bootstrap validation never observes a half-created user.
    let user = users::find_or_create_with_default_organization(
        &state.db_pool,
        &claims.email,
        state.default_organization_uid,
    )
    .await
    .map_err(|e| {
        tracing::error!("Failed to find/create user for group sync: {:#}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Database error".to_string(),
        )
    })?;

    // Sync groups if present in claims (including empty groups - user may have been removed from all groups)
    if let Some(ref groups) = claims.groups {
        tracing::debug!(
            "Syncing {} IdP groups for user {} during login",
            groups.len(),
            user.email
        );

        if let Err(e) =
            crate::server::auth::group_sync::sync_user_groups(&state.db_pool, user.id, groups).await
        {
            // Log error but don't fail login
            tracing::error!(
                "Failed to sync IdP groups during login for user {}: {:#}",
                user.email,
                e
            );
        }
    }

    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct CodeExchangeRequest {
    pub code: String,
    pub code_verifier: String,
    pub redirect_uri: String,
}

#[derive(Debug, Deserialize)]
pub struct DeviceExchangeRequest {
    pub device_code: String,
}

#[derive(Debug, Serialize)]
pub struct DeviceExchangeResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_description: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub token: String,
}

#[derive(Debug, Deserialize)]
pub struct AuthorizeRequest {
    /// For authorization code flow: the redirect URI
    #[serde(default)]
    pub redirect_uri: Option<String>,
    /// For authorization code flow: the PKCE code challenge
    #[serde(default)]
    pub code_challenge: Option<String>,
    /// For authorization code flow: the PKCE code challenge method
    #[serde(default)]
    pub code_challenge_method: Option<String>,
    /// Flow type: "code" for authorization code flow, "device" for device flow
    pub flow: String,
}

#[derive(Debug, Serialize)]
pub struct AuthorizeResponse {
    /// For authorization code flow: the full authorization URL to open in browser
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization_url: Option<String>,
    /// For device flow: the device code
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_code: Option<String>,
    /// For device flow: the user code to display
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_code: Option<String>,
    /// For device flow: the verification URI to display
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification_uri: Option<String>,
    /// For device flow: the complete verification URI (with user code embedded)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification_uri_complete: Option<String>,
    /// For device flow: how long the device code is valid (seconds)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<u64>,
    /// For device flow: how often to poll (seconds)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval: Option<u64>,
}

/// Build OAuth2 authorization URL or initiate device flow (for CLI)
#[instrument(skip(state))]
pub async fn authorize(
    State(state): State<AppState>,
    Json(payload): Json<AuthorizeRequest>,
) -> Result<Json<AuthorizeResponse>, (StatusCode, String)> {
    match payload.flow.as_str() {
        "code" => {
            // Authorization code flow with PKCE
            let redirect_uri = payload.redirect_uri.ok_or_else(|| {
                (
                    StatusCode::BAD_REQUEST,
                    "redirect_uri required for code flow".to_string(),
                )
            })?;
            let code_challenge = payload.code_challenge.ok_or_else(|| {
                (
                    StatusCode::BAD_REQUEST,
                    "code_challenge required for code flow".to_string(),
                )
            })?;
            let code_challenge_method = payload.code_challenge_method.ok_or_else(|| {
                (
                    StatusCode::BAD_REQUEST,
                    "code_challenge_method required for code flow".to_string(),
                )
            })?;

            // Build authorization URL with typed parameters
            let params = crate::server::auth::oauth::AuthorizeParams {
                client_id: &state.auth_settings.client_id,
                redirect_uri: &redirect_uri,
                response_type: "code",
                scope: state.oauth_client.scopes(),
                code_challenge: &code_challenge,
                code_challenge_method: &code_challenge_method,
                state: None,
            };

            let authorization_url = state.oauth_client.build_authorize_url(&params);

            Ok(Json(AuthorizeResponse {
                authorization_url: Some(authorization_url),
                device_code: None,
                user_code: None,
                verification_uri: None,
                verification_uri_complete: None,
                expires_in: None,
                interval: None,
            }))
        }
        "device" => {
            // Device authorization flow
            let device_response = state.oauth_client.device_flow_start().await.map_err(|e| {
                tracing::error!("Failed to start device flow: {:#}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to start device flow: {}", e),
                )
            })?;

            Ok(Json(AuthorizeResponse {
                authorization_url: None,
                device_code: Some(device_response.device_code),
                user_code: Some(device_response.user_code),
                verification_uri: Some(device_response.verification_uri.clone()),
                verification_uri_complete: Some(device_response.verification_uri_complete),
                expires_in: Some(device_response.expires_in),
                interval: Some(device_response.interval),
            }))
        }
        _ => Err((
            StatusCode::BAD_REQUEST,
            format!("Invalid flow type: {}", payload.flow),
        )),
    }
}

/// Exchange authorization code for token (OAuth2 PKCE flow)
#[instrument(skip(state, payload))]
pub async fn code_exchange(
    State(state): State<AppState>,
    Json(payload): Json<CodeExchangeRequest>,
) -> Result<Json<LoginResponse>, (StatusCode, String)> {
    tracing::debug!(
        "Code exchange request: redirect_uri={}",
        payload.redirect_uri
    );

    // Exchange authorization code for tokens using PKCE
    let token_info = state
        .oauth_client
        .exchange_code_pkce(&payload.code, &payload.code_verifier, &payload.redirect_uri)
        .await
        .map_err(|e| {
            tracing::warn!("OAuth2 code exchange failed: {:#}", e);
            (
                StatusCode::UNAUTHORIZED,
                format!("Code exchange failed: {}", e),
            )
        })?;

    tracing::info!(
        "Code exchange successful, token_type={}, expires_in={}",
        token_info.token_type,
        token_info.expires_in
    );

    // Decode and log token claims for debugging (without validating yet)
    if let Ok(header) = jsonwebtoken::decode_header(&token_info.id_token) {
        tracing::debug!("ID token header: {:?}", header);
    }

    // Try to decode payload for logging (this doesn't validate signature)
    let parts: Vec<&str> = token_info.id_token.split('.').collect();
    if parts.len() == 3 {
        if let Ok(decoded) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(parts[1]) {
            if let Ok(claims_str) = String::from_utf8(decoded) {
                tracing::debug!("ID token claims: {}", claims_str);
            }
        }
    }

    // Sync IdP groups after successful login
    if let Err(e) = sync_groups_after_login(&state, &token_info.id_token).await {
        tracing::warn!("Group sync failed during code exchange: {:?}", e);
        // Don't fail the login if group sync fails
    }

    // Validate the IdP JWT to extract claims
    let mut expected_claims = HashMap::new();
    expected_claims.insert("aud".to_string(), state.auth_settings.client_id.clone());

    let claims = state
        .jwt_validator
        .validate(
            &token_info.id_token,
            &state.auth_settings.issuer,
            &expected_claims,
        )
        .await
        .map_err(|e| {
            tracing::error!("Failed to validate ID token: {:#}", e);
            (StatusCode::UNAUTHORIZED, "Invalid token".to_string())
        })?;

    // Extract email from claims
    let email = claims
        .get("email")
        .and_then(|v| v.as_str())
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "Email claim missing".to_string()))?;

    // Find or create user (paired with default-Org membership in one transaction)
    let user = users::find_or_create_with_default_organization(
        &state.db_pool,
        email,
        state.default_organization_uid,
    )
    .await
    .map_err(|e| {
        tracing::error!("Failed to find/create user: {:#}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to process user".to_string(),
        )
    })?;

    // Issue Rise JWT for user authentication (consumed by the CLI)
    let rise_jwt = state
        .jwt_signer
        .sign_user_jwt(&claims, user.id, &state.db_pool, &state.public_url, None)
        .await
        .map_err(|e| {
            tracing::error!("Failed to sign Rise JWT: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to create token".to_string(),
            )
        })?;

    tracing::info!(
        "CLI login successful for user {} - issued Rise JWT",
        user.email
    );

    Ok(Json(LoginResponse { token: rise_jwt }))
}

/// Exchange device code for token (Device Flow)
#[instrument(skip(state, payload))]
pub async fn device_exchange(
    State(state): State<AppState>,
    Json(payload): Json<DeviceExchangeRequest>,
) -> Json<DeviceExchangeResponse> {
    tracing::debug!(
        "Device exchange request: device_code={}...",
        &payload.device_code[..8.min(payload.device_code.len())]
    );

    // Poll the identity provider's token endpoint with the device code
    match state
        .oauth_client
        .device_flow_poll(&payload.device_code)
        .await
    {
        Ok(Some(token_info)) => {
            tracing::info!("Device authorization successful");

            // Sync IdP groups after successful login
            if let Err(e) = sync_groups_after_login(&state, &token_info.id_token).await {
                tracing::warn!("Group sync failed during device exchange: {:?}", e);
                // Don't fail the login if group sync fails
            }

            // Validate the IdP JWT to extract claims
            let mut expected_claims = HashMap::new();
            expected_claims.insert("aud".to_string(), state.auth_settings.client_id.clone());

            let claims = match state
                .jwt_validator
                .validate(
                    &token_info.id_token,
                    &state.auth_settings.issuer,
                    &expected_claims,
                )
                .await
            {
                Ok(claims) => claims,
                Err(e) => {
                    tracing::error!("Failed to validate ID token: {:#}", e);
                    return Json(DeviceExchangeResponse {
                        token: None,
                        error: Some("invalid_token".to_string()),
                        error_description: Some("Failed to validate ID token".to_string()),
                    });
                }
            };

            // Extract email from claims
            let email = match claims.get("email").and_then(|v| v.as_str()) {
                Some(email) => email,
                None => {
                    tracing::error!("Email claim missing from ID token");
                    return Json(DeviceExchangeResponse {
                        token: None,
                        error: Some("invalid_token".to_string()),
                        error_description: Some("Email claim missing".to_string()),
                    });
                }
            };

            // Find or create user (paired with default-Org membership in one transaction)
            let user = match users::find_or_create_with_default_organization(
                &state.db_pool,
                email,
                state.default_organization_uid,
            )
            .await
            {
                Ok(user) => user,
                Err(e) => {
                    tracing::error!("Failed to find/create user: {:#}", e);
                    return Json(DeviceExchangeResponse {
                        token: None,
                        error: Some("server_error".to_string()),
                        error_description: Some("Failed to process user".to_string()),
                    });
                }
            };

            // Issue Rise JWT for user authentication (consumed by the CLI)
            let rise_jwt = match state
                .jwt_signer
                .sign_user_jwt(&claims, user.id, &state.db_pool, &state.public_url, None)
                .await
            {
                Ok(jwt) => jwt,
                Err(e) => {
                    tracing::error!("Failed to sign Rise JWT: {:#}", e);
                    return Json(DeviceExchangeResponse {
                        token: None,
                        error: Some("server_error".to_string()),
                        error_description: Some("Failed to create token".to_string()),
                    });
                }
            };

            tracing::info!(
                "CLI device login successful for user {} - issued Rise JWT",
                user.email
            );

            Json(DeviceExchangeResponse {
                token: Some(rise_jwt),
                error: None,
                error_description: None,
            })
        }
        Ok(None) => {
            // authorization_pending - user hasn't authorized yet
            tracing::debug!("Device authorization pending");
            Json(DeviceExchangeResponse {
                token: None,
                error: Some("authorization_pending".to_string()),
                error_description: None,
            })
        }
        Err(e) => {
            let error_msg = e.to_string();
            tracing::warn!("Device authorization error: {}", error_msg);

            // Check for standard OAuth2 device flow errors
            let (error, description) = if error_msg.contains("slow_down") {
                ("slow_down".to_string(), None)
            } else {
                ("access_denied".to_string(), Some(error_msg))
            };

            Json(DeviceExchangeResponse {
                token: None,
                error: Some(error),
                error_description: description,
            })
        }
    }
}

#[derive(Debug, Serialize)]
pub struct MeResponse {
    pub id: String,
    pub email: String,
    pub is_admin: bool,
    pub is_operator: bool,
    pub can_create_teams: bool,
}

/// Get current user info from auth middleware
#[instrument(skip(state, auth))]
pub async fn me(
    State(state): State<AppState>,
    auth: crate::server::auth::context::AuthContext,
) -> Result<Json<MeResponse>, (StatusCode, String)> {
    let user = auth.user().map_err(|e| (e.status, e.message))?;
    // User is injected by auth middleware
    tracing::debug!("GET /me: user_id={}, email={}", user.id, user.email);
    let is_admin = state.is_admin(&user.email);
    let is_operator = state.is_operator(&user.email);
    let can_create_teams = is_admin || state.auth_settings.allow_team_creation;
    Ok(Json(MeResponse {
        id: user.id.to_string(),
        email: user.email.clone(),
        is_admin,
        is_operator,
        can_create_teams,
    }))
}

#[derive(Debug, Deserialize)]
pub struct UsersLookupRequest {
    pub emails: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct UsersLookupResponse {
    pub users: Vec<UserInfo>,
}

#[derive(Debug, Serialize)]
pub struct UserInfo {
    pub id: String,
    pub email: String,
}

/// Lookup users by email addresses
#[instrument(skip(state, auth))]
pub async fn users_lookup(
    State(state): State<AppState>,
    auth: crate::server::auth::context::AuthContext,
    Json(payload): Json<UsersLookupRequest>,
) -> Result<Json<UsersLookupResponse>, (StatusCode, String)> {
    let _user = auth.user().map_err(|e| (e.status, e.message))?;
    let mut user_infos = Vec::new();

    for email in payload.emails {
        // Query database for user by email
        let user = users::find_by_email(&state.db_pool, &email)
            .await
            .map_err(|e| {
                tracing::error!("Database error looking up user {}: {:#}", email, e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Database error".to_string(),
                )
            })?;

        match user {
            Some(u) => {
                user_infos.push(UserInfo {
                    id: u.id.to_string(),
                    email: u.email,
                });
            }
            None => {
                return Err((StatusCode::NOT_FOUND, format!("User not found: {}", email)));
            }
        }
    }

    Ok(Json(UsersLookupResponse { users: user_infos }))
}

// ============================================================================
// OAuth2 Proxy Handlers for Ingress Authentication
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct SigninQuery {
    /// Optional redirect URL to return to after authentication (path only)
    pub redirect: Option<String>,
    /// Optional full redirect URL from Nginx ingress (includes host)
    pub rd: Option<String>,
    /// Optional project name for ingress authentication flow
    pub project: Option<String>,
}

/// Pre-authentication page for ingress auth
///
/// Shows the user which project they're about to authenticate for before
/// starting the OAuth flow. This provides better UX by explaining what's happening.
#[instrument(skip(state, params, headers, uri))]
pub async fn signin_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Query(params): Query<SigninQuery>,
) -> Result<Response, (StatusCode, String)> {
    let project_name = params.project.as_deref().unwrap_or("Unknown");
    let raw_redirect_url = params
        .redirect
        .as_ref()
        .or(params.rd.as_ref())
        .cloned()
        .unwrap_or_else(|| "/".to_string());

    // Validate and sanitize the redirect URL to prevent open redirects
    let redirect_url = validate_redirect_url(&raw_redirect_url, &state.public_url);

    tracing::info!(
        project = %project_name,
        has_redirect = !redirect_url.is_empty(),
        raw_redirect = %raw_redirect_url,
        validated_redirect = %redirect_url,
        "Signin page requested"
    );

    // Load template from static directory
    let static_dir = state.server_settings.static_dir.as_deref().ok_or_else(|| {
        tracing::error!("static_dir not configured");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Static dir not configured".to_string(),
        )
    })?;

    let tera = load_auth_template(static_dir, "auth-signin.html.tera").await?;

    // Determine if this is via `/.rise/auth` path (custom domain Ingress routing)
    let is_rise_path = uri.path().starts_with("/.rise/auth");

    // Build continue URL (to oauth_signin_start)
    let mut continue_params = vec![];
    if let Some(ref project) = params.project {
        continue_params.push(format!("project={}", urlencoding::encode(project)));
    }
    if !redirect_url.is_empty() {
        continue_params.push(format!("redirect={}", urlencoding::encode(&redirect_url)));
    }

    // Use request base URL for continue link when accessed via /.rise/auth path
    let continue_url = if is_rise_path {
        format!(
            "{}/.rise/auth/signin/start?{}",
            extract_request_base_url(&headers, &state),
            continue_params.join("&")
        )
    } else {
        format!(
            "{}/api/v1/auth/signin/start?{}",
            state.public_url.trim_end_matches('/'),
            continue_params.join("&")
        )
    };

    // Render template
    let mut context = tera::Context::new();
    context.insert("project_name", project_name);
    context.insert("continue_url", &continue_url);
    context.insert("redirect_url", &redirect_url);

    let html = tera
        .render("auth-signin.html.tera", &context)
        .map_err(|e| {
            tracing::error!("Failed to render template: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Template rendering error".to_string(),
            )
        })?;

    Ok(Html(html).into_response())
}

/// Extract base URL (scheme + host) from request headers.
///
/// Used for OAuth callback URL when handling requests via Ingress routing.
/// This allows the OAuth flow to use the actual request host (e.g., custom domain)
/// instead of the configured public_url.
///
/// Falls back to the configured public_url if no valid host header is present.
fn extract_request_base_url(headers: &HeaderMap, state: &AppState) -> String {
    // Get Host header
    if let Some(host) = headers.get("host") {
        if let Ok(host_str) = host.to_str() {
            // Get X-Forwarded-Proto header (set by Nginx ingress)
            let scheme = headers
                .get("x-forwarded-proto")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("http");

            return format!("{}://{}", scheme, host_str);
        }
    }

    // Fallback to configured public URL
    state.public_url.trim_end_matches('/').to_string()
}

/// Extract the host (with port if present) from a URL like `scheme://host:port/path`.
fn url_host(url: &str) -> &str {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("")
}

/// Initiate OAuth2 login flow for ingress auth (start of OAuth flow)
///
/// This handler starts the OAuth2 authorization code flow with PKCE.
/// It generates a PKCE verifier/challenge pair, stores the state, and
/// redirects the user to the OIDC provider for authentication.
#[instrument(skip(state, params, uri))]
pub async fn oauth_signin_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Query(params): Query<SigninQuery>,
) -> Result<Response, (StatusCode, String)> {
    // Prefer rd (full URL) over redirect (path only)
    let raw_redirect_url = params.rd.as_ref().or(params.redirect.as_ref());

    // Validate and sanitize redirect URL if provided
    let redirect_url = raw_redirect_url.map(|url| validate_redirect_url(url, &state.public_url));

    tracing::info!(
        project = ?params.project,
        has_redirect = redirect_url.is_some(),
        raw_redirect = ?raw_redirect_url,
        validated_redirect = ?redirect_url,
        "OAuth signin initiated"
    );

    // Determine if this is via `/.rise/auth` path (Ingress routing)
    let is_rise_path = uri.path().starts_with("/.rise/auth");

    // Generate PKCE parameters
    let code_verifier = generate_code_verifier();
    let code_challenge = generate_code_challenge(&code_verifier);
    let state_token = generate_state_token();

    // For /.rise/auth paths, determine the app base URL for cookie setting after the callback.
    // Only use the request host as the base URL when it's a *true* custom domain — i.e. when
    // the request host doesn't match the standard ingress template host.  For sub-path ingress
    // (`apps.example.com/{project_name}`), extract_request_base_url would return just
    // `https://apps.example.com`, missing the path prefix and producing a wrong JWT audience.
    // In that case we leave custom_domain_base_url as None and let build_project_url supply the
    // full URL (including path) in the callback handler.
    let custom_domain_base_url = if is_rise_path {
        let request_base_url = extract_request_base_url(&headers, &state);
        let is_standard_ingress = params
            .project
            .as_deref()
            .and_then(|p| build_project_url(&state, p))
            .map(|template_url| url_host(&template_url) == url_host(&request_base_url))
            .unwrap_or(false);
        if is_standard_ingress {
            None
        } else {
            Some(request_base_url)
        }
    } else {
        None
    };

    // Store PKCE state with redirect URL, project name, and custom domain base URL
    let oauth_state = OAuth2State {
        code_verifier: code_verifier.clone(),
        redirect_url,
        project_name: params.project.clone(), // For ingress auth flow
        custom_domain_base_url,
    };
    state
        .token_store
        .save(state_token.clone(), oauth_state)
        .await
        .map_err(|e| {
            tracing::error!("Failed to store PKCE state: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to initiate login".to_string(),
            )
        })?;

    // Build OAuth2 authorization URL
    // IdP callback always uses the main Rise domain (pre-registered with IdP)
    // For custom domains, we'll redirect to them after the callback completes
    let callback_url = format!(
        "{}/api/v1/auth/callback",
        state.public_url.trim_end_matches('/')
    );

    let params = crate::server::auth::oauth::AuthorizeParams {
        client_id: &state.auth_settings.client_id,
        redirect_uri: &callback_url,
        response_type: "code",
        scope: "openid email profile",
        code_challenge: &code_challenge,
        code_challenge_method: "S256",
        state: Some(&state_token),
    };

    let auth_url = state.oauth_client.build_authorize_url(&params);

    tracing::debug!("Redirecting to OIDC provider for authentication");
    Ok(Redirect::to(&auth_url).into_response())
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub code: String,
    pub state: String,
}

/// OAuth2 callback from OIDC provider
///
/// This handler receives the authorization code from the OIDC provider, exchanges it for tokens,
/// sets a session cookie, and redirects the user back to their original URL.
///
/// For custom domain auth routing:
/// - IdP always redirects to the main Rise domain (single pre-registered redirect URI)
/// - If `custom_domain_callback_url` is set in state, we store a one-time token and redirect
///   to the custom domain's `/.rise/auth/complete` endpoint to set cookies there
#[instrument(skip(state, params))]
pub async fn oauth_callback(
    State(state): State<AppState>,
    Query(params): Query<CallbackQuery>,
) -> Result<Response, (StatusCode, String)> {
    tracing::info!("OAuth callback received");

    let claimed_state = state
        .token_store
        .claim(&params.state)
        .await
        .map_err(|e| {
            tracing::error!("Failed to claim PKCE state: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Login failed".to_string(),
            )
        })?
        .ok_or_else(|| {
            tracing::warn!("Invalid or expired state token");
            (
                StatusCode::BAD_REQUEST,
                "Invalid or expired state token".to_string(),
            )
        })?;
    // Build callback URL (must match the one used in signin)
    // IdP callback always uses the main Rise domain (pre-registered with IdP)
    let callback_url = format!(
        "{}/api/v1/auth/callback",
        state.public_url.trim_end_matches('/')
    );

    // Exchange authorization code for tokens
    let token_info = state
        .oauth_client
        .exchange_code_pkce(&params.code, &claimed_state.code_verifier, &callback_url)
        .await
        .map_err(|e| {
            tracing::error!("Failed to exchange code: {:#}", e);
            (
                StatusCode::UNAUTHORIZED,
                format!("Code exchange failed: {}", e),
            )
        })?;

    tracing::info!("Successfully exchanged code for tokens");

    // Sync IdP groups after successful login
    if let Err(e) = sync_groups_after_login(&state, &token_info.id_token).await {
        tracing::warn!("Group sync failed during OAuth callback: {:?}", e);
        // Don't fail the login if group sync fails
    }

    // Validate the IdP JWT to extract claims
    let mut expected_claims = HashMap::new();
    expected_claims.insert("aud".to_string(), state.auth_settings.client_id.clone());

    let claims = state
        .jwt_validator
        .validate(
            &token_info.id_token,
            &state.auth_settings.issuer,
            &expected_claims,
        )
        .await
        .map_err(|e| {
            tracing::error!("Failed to validate JWT: {:#}", e);
            (StatusCode::UNAUTHORIZED, "Invalid token".to_string())
        })?;

    // Use configured JWT expiry for Rise tokens and cookies
    // (Don't inherit the short-lived IdP token's expiry)
    let max_age = state.jwt_signer.default_expiry_seconds;

    // Determine redirect URL
    let redirect_url = claimed_state
        .data()
        .redirect_url
        .clone()
        .unwrap_or_else(|| "/".to_string());

    // For ingress auth flow (with project), issue Rise JWT and redirect to the
    // app's domain to set the cookie there (scoped to the app host, not shared).
    if let Some(project) = claimed_state.data().project_name.as_deref() {
        tracing::info!(
            "Issuing Rise JWT for ingress auth (project context: {})",
            project
        );

        // Get user email from claims
        let user_email = claims["email"].as_str().ok_or_else(|| {
            tracing::error!("No email in JWT claims");
            (StatusCode::UNAUTHORIZED, "Invalid token claims".to_string())
        })?;

        // Find or create user to get user_id for team lookup (paired with
        // default-Org membership in one transaction)
        let user = users::find_or_create_with_default_organization(
            &state.db_pool,
            user_email,
            state.default_organization_uid,
        )
        .await
        .map_err(|e| {
            tracing::error!("Failed to find/create user: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Database error".to_string(),
            )
        })?;

        // Issue Rise JWT with user's team memberships
        // Use custom domain URL as audience when available, otherwise build from ingress template
        let project_url = claimed_state
            .data()
            .custom_domain_base_url
            .as_deref()
            .map(|url| url.trim_end_matches('/').to_string())
            .or_else(|| build_project_url(&state, project))
            .unwrap_or_else(|| state.public_url.trim_end_matches('/').to_string());

        let rise_jwt = state
            .jwt_signer
            .sign_ingress_jwt(&claims, user.id, &state.db_pool, &project_url, None)
            .await
            .map_err(|e| {
                tracing::error!("Failed to sign Rise JWT: {:#}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to create authentication token".to_string(),
                )
            })?;

        // Build the app base URL to redirect to for cookie setting.
        // Use custom_domain_base_url if available, otherwise build from ingress template.
        let app_base_url = claimed_state
            .data()
            .custom_domain_base_url
            .clone()
            .or_else(|| build_project_url(&state, project))
            .ok_or_else(|| {
                tracing::error!(
                    "Cannot redirect to app for cookie setting: no ingress URL template configured"
                );
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Ingress URL template not configured".to_string(),
                )
            })?;

        // Always redirect to the app's domain to set the cookie there.
        // The /.rise/auth/complete handler sets the cookie scoped to the current host.
        let completion_token = generate_state_token();

        let completed_session = CompletedAuthSession {
            rise_jwt,
            max_age,
            redirect_url: redirect_url.clone(),
            project_name: project.to_string(),
        };
        state
            .token_store
            .save_completed_session(completion_token.clone(), completed_session)
            .await
            .map_err(|e| {
                tracing::error!("Failed to store completed session: {:?}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to complete authentication".to_string(),
                )
            })?;

        let complete_url = format!(
            "{}/.rise/auth/complete?token={}",
            app_base_url.trim_end_matches('/'),
            completion_token
        );

        tracing::info!(
            "Redirecting to app domain for cookie setting: {}",
            complete_url
        );

        if let Err(e) = claimed_state.finalize().await {
            tracing::warn!(
                "Failed to finalize PKCE state (response already built, row will expire via TTL): {:?}",
                e
            );
        }
        return Ok(Redirect::to(&complete_url).into_response());
    }

    // Regular OAuth flow (not ingress auth) - UI login
    tracing::info!("Using Rise JWT for UI session");

    // Get claims from IdP token (use existing validation from earlier in the function)
    let mut expected_claims = HashMap::new();
    expected_claims.insert("aud".to_string(), state.auth_settings.client_id.clone());

    let claims = state
        .jwt_validator
        .validate(
            &token_info.id_token,
            &state.auth_settings.issuer,
            &expected_claims,
        )
        .await
        .map_err(|e| {
            tracing::error!("Failed to validate ID token: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to validate token".to_string(),
            )
        })?;

    let email = claims
        .get("email")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            tracing::error!("Email claim missing from ID token");
            (
                StatusCode::BAD_REQUEST,
                "Email claim missing from token".to_string(),
            )
        })?;

    // Find or create user (paired with default-Org membership in one transaction)
    let user = users::find_or_create_with_default_organization(
        &state.db_pool,
        email,
        state.default_organization_uid,
    )
    .await
    .map_err(|e| {
        tracing::error!("Failed to find or create user: {:#}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to process user".to_string(),
        )
    })?;

    // Sync groups after login
    sync_groups_after_login(&state, &token_info.id_token).await?;

    // Issue Rise HS256 JWT for user authentication (consumed by the UI)
    let rise_jwt = state
        .jwt_signer
        .sign_user_jwt(&claims, user.id, &state.db_pool, &state.public_url, None)
        .await
        .map_err(|e| {
            tracing::error!("Failed to sign user JWT: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to create authentication token".to_string(),
            )
        })?;

    let cookie = cookie_helpers::create_rise_jwt_cookie(&rise_jwt, &state.cookie_settings, max_age);

    tracing::info!(
        "Setting Rise JWT cookie and redirecting to {}",
        redirect_url
    );

    // Use success page with delayed redirect to ensure cookie is properly persisted
    let response = render_ui_login_success_page(&state, &redirect_url, &cookie).await?;
    if let Err(e) = claimed_state.finalize().await {
        tracing::warn!(
            "Failed to finalize PKCE state (response already built, row will expire via TTL): {:?}",
            e
        );
    }
    Ok(response)
}

/// Build an HTTP response with the new host-only cookie and, when configured, an additional
/// `Max-Age=0` Set-Cookie header to expire any legacy domain-scoped cookie.
fn build_cookie_response<B>(
    state: &AppState,
    status: StatusCode,
    cookie: &str,
    body: B,
) -> Result<Response, (StatusCode, String)>
where
    B: IntoResponse,
{
    let mut headers = HeaderMap::new();
    headers.insert(
        header::SET_COOKIE,
        HeaderValue::from_str(cookie).map_err(|e| {
            tracing::error!("Failed to build Set-Cookie header: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal error".to_string(),
            )
        })?,
    );
    if let Some(domain) = &state.cookie_settings.cookie_domain {
        let legacy = cookie_helpers::clear_legacy_domain_cookie(domain, &state.cookie_settings);
        if let Ok(val) = HeaderValue::from_str(&legacy) {
            headers.append(header::SET_COOKIE, val);
        }
    }
    Ok((status, headers, body).into_response())
}

/// Helper function to render the success page with cookie
async fn render_success_page(
    state: &AppState,
    project_name: &str,
    redirect_url: &str,
    cookie: &str,
) -> Result<Response, (StatusCode, String)> {
    // Load success template
    let static_dir = state.server_settings.static_dir.as_deref().ok_or_else(|| {
        tracing::error!("static_dir not configured");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Static dir not configured".to_string(),
        )
    })?;

    let tera = load_auth_template(static_dir, "auth-success.html.tera").await?;

    // Render success template
    let mut context = tera::Context::new();
    context.insert("success", &true);
    context.insert("project_name", project_name);
    context.insert("redirect_url", redirect_url);

    let html = tera
        .render("auth-success.html.tera", &context)
        .map_err(|e| {
            tracing::error!("Failed to render template: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Template rendering error".to_string(),
            )
        })?;

    tracing::info!(
        "Setting ingress JWT cookie and showing success page for project: {}",
        project_name
    );

    let response = build_cookie_response(state, StatusCode::OK, cookie, Html(html))?;
    Ok(response)
}

/// Helper function to render the UI login success page with cookie
async fn render_ui_login_success_page(
    state: &AppState,
    redirect_url: &str,
    cookie: &str,
) -> Result<Response, (StatusCode, String)> {
    // Load UI success template
    let static_dir = state.server_settings.static_dir.as_deref().ok_or_else(|| {
        tracing::error!("static_dir not configured");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Static dir not configured".to_string(),
        )
    })?;

    let tera = load_auth_template(static_dir, "auth-ui-success.html.tera").await?;

    // Render success template
    let mut context = tera::Context::new();
    context.insert("redirect_url", redirect_url);

    let html = tera
        .render("auth-ui-success.html.tera", &context)
        .map_err(|e| {
            tracing::error!("Failed to render template: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Template rendering error".to_string(),
            )
        })?;

    tracing::info!(
        "Setting UI JWT cookie and showing success page, redirecting to: {}",
        redirect_url
    );

    let response = build_cookie_response(state, StatusCode::OK, cookie, Html(html))?;
    Ok(response)
}

#[derive(Debug, Deserialize)]
pub struct CompleteQuery {
    pub token: String,
}

/// Complete OAuth flow on custom domain
///
/// This handler is called on the custom domain after the IdP callback completes on the main domain.
/// It receives a one-time token, retrieves the stored auth session, sets the cookie on the
/// custom domain, and shows the success page.
#[instrument(skip(state, params))]
pub async fn oauth_complete(
    State(state): State<AppState>,
    Query(params): Query<CompleteQuery>,
) -> Result<Response, (StatusCode, String)> {
    tracing::info!("Custom domain auth complete received");

    let claimed_session = state
        .token_store
        .claim_completed_session(&params.token)
        .await
        .map_err(|e| {
            tracing::error!("Failed to claim completed session: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Login failed".to_string(),
            )
        })?
        .ok_or_else(|| {
            tracing::warn!("Invalid or expired completion token");
            (
                StatusCode::BAD_REQUEST,
                "Invalid or expired completion token. Please try logging in again.".to_string(),
            )
        })?;
    let cookie = cookie_helpers::create_rise_jwt_cookie(
        &claimed_session.rise_jwt,
        &state.cookie_settings,
        claimed_session.max_age,
    );

    let response = render_success_page(
        &state,
        &claimed_session.project_name,
        &claimed_session.redirect_url,
        &cookie,
    )
    .await?;
    if let Err(e) = claimed_session.finalize().await {
        tracing::warn!(
            "Failed to finalize completed session (response already built, row will expire via TTL): {:?}",
            e
        );
    }
    Ok(response)
}

#[derive(Debug, Deserialize)]
pub struct IngressAuthQuery {
    pub project: String,
}

/// Nginx ingress auth endpoint
///
/// This handler is called by Nginx for every request to a private project.
/// It validates the session cookie, checks JWT validity, and verifies
/// project access authorization.
#[instrument(skip(state, params, headers))]
pub async fn ingress_auth(
    State(state): State<AppState>,
    Query(params): Query<IngressAuthQuery>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, String)> {
    // Log only safe, relevant information (excluding sensitive cookies, tokens, etc.)
    let request_id = headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("none");

    tracing::debug!(
        project = %params.project,
        request_id = %request_id,
        "Ingress auth check"
    );

    // Allow access to /.rise/* paths without authentication (login page, static assets)
    // This prevents redirect loops when users try to access the signin page
    // Use x-auth-request-redirect header which contains the request path
    if let Some(redirect_path) = headers
        .get("x-auth-request-redirect")
        .and_then(|v| v.to_str().ok())
    {
        if redirect_path.starts_with("/.rise/") {
            tracing::debug!(
                project = %params.project,
                redirect_path = %redirect_path,
                "Allowing unauthenticated access to .rise path"
            );
            return Ok((
                StatusCode::OK,
                [("X-Auth-Request-User", "anonymous".to_string())],
            )
                .into_response());
        }
    }

    // Extract and validate Rise JWT (required)
    let rise_jwt = cookie_helpers::extract_rise_jwt_cookie(&headers).ok_or_else(|| {
        tracing::debug!("No Rise JWT cookie found");
        (StatusCode::UNAUTHORIZED, "No session cookie".to_string())
    })?;

    let ingress_claims = state
        .jwt_signer
        .verify_jwt_skip_aud(&rise_jwt)
        .map_err(|e| {
            tracing::warn!("Invalid or expired ingress JWT: {:#}", e);
            (
                StatusCode::UNAUTHORIZED,
                "Invalid or expired session".to_string(),
            )
        })?;

    let email = ingress_claims.email;

    // Find or create user in database (paired with default-Org membership)
    let user = users::find_or_create_with_default_organization(
        &state.db_pool,
        &email,
        state.default_organization_uid,
    )
    .await
    .map_err(|e| {
        tracing::error!("Database error finding/creating user: {:#}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Database error".to_string(),
        )
    })?;

    tracing::debug!(
        project = %params.project,
        user_id = %user.id,
        user_email = %user.email,
        "Rise JWT validated"
    );

    // Find project by name
    let project = projects::find_by_name(&state.db_pool, &params.project)
        .await
        .map_err(|e| {
            tracing::error!("Database error finding project: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Database error".to_string(),
            )
        })?
        .ok_or_else(|| {
            tracing::debug!("Project not found: {}", params.project);
            (StatusCode::NOT_FOUND, "Project not found".to_string())
        })?;

    // Get project's access class configuration
    use crate::server::settings::AccessRequirement;
    let access_class = state
        .access_classes
        .get(&project.access_class)
        .ok_or_else(|| {
            tracing::error!("Access class '{}' not configured", project.access_class);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Invalid access class".to_string(),
            )
        })?;

    // Handle different access requirements
    match access_class.access_requirement {
        AccessRequirement::None => {
            // Should never be called - None means no nginx auth annotations
            // But if it is called, deny access as a safety measure
            tracing::warn!(
                project = %params.project,
                "Auth endpoint called for AccessRequirement::None project"
            );
            Err((
                StatusCode::FORBIDDEN,
                "This project should not require authentication".to_string(),
            ))
        }

        AccessRequirement::Authenticated => {
            // Allow all authenticated users (no membership check)
            tracing::debug!(
                project = %params.project,
                user_id = %user.id,
                user_email = %user.email,
                access_class = %project.access_class,
                "Access granted - authenticated user"
            );
            Ok((
                StatusCode::OK,
                [
                    ("X-Auth-Request-Email", email),
                    ("X-Auth-Request-User", user.id.to_string()),
                ],
            )
                .into_response())
        }

        AccessRequirement::Member => {
            // Check project membership (owner or team member)
            let has_member_access = projects::user_can_access(&state.db_pool, project.id, user.id)
                .await
                .map_err(|e| {
                    tracing::error!("Database error checking access: {:#}", e);
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Database error".to_string(),
                    )
                })?;

            if has_member_access {
                tracing::debug!(
                    project = %params.project,
                    user_id = %user.id,
                    user_email = %user.email,
                    "Access granted - project member"
                );
                return Ok((
                    StatusCode::OK,
                    [
                        ("X-Auth-Request-Email", email),
                        ("X-Auth-Request-User", user.id.to_string()),
                    ],
                )
                    .into_response());
            }

            // Check if user is an app user (view-only access to deployed app)
            let has_app_access = crate::db::project_app_users::user_can_access_app(
                &state.db_pool,
                project.id,
                user.id,
            )
            .await
            .map_err(|e| {
                tracing::error!("Database error checking app user access: {:#}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Database error".to_string(),
                )
            })?;

            if has_app_access {
                tracing::debug!(
                    project = %params.project,
                    user_id = %user.id,
                    user_email = %user.email,
                    "Access granted - app user"
                );
                Ok((
                    StatusCode::OK,
                    [
                        ("X-Auth-Request-Email", email),
                        ("X-Auth-Request-User", user.id.to_string()),
                    ],
                )
                    .into_response())
            } else {
                tracing::warn!(
                    project = %params.project,
                    user_id = %user.id,
                    user_email = %user.email,
                    "Access denied - not a project member or app user"
                );
                Err((
                    StatusCode::FORBIDDEN,
                    "You do not have access to this project".to_string(),
                ))
            }
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct LogoutQuery {
    /// Optional redirect URL after logout
    pub redirect: Option<String>,
}

/// Logout endpoint
///
/// Clears the session cookie and redirects the user.
#[instrument(skip(state))]
pub async fn oauth_logout(
    State(state): State<AppState>,
    Query(params): Query<LogoutQuery>,
) -> Result<Response, (StatusCode, String)> {
    tracing::info!("Logout initiated");

    // Clear the Rise JWT cookie
    let cookie = cookie_helpers::clear_rise_jwt_cookie(&state.cookie_settings);

    // Determine redirect URL
    let redirect_url = params.redirect.unwrap_or_else(|| "/".to_string());

    tracing::info!(
        "Clearing Rise JWT cookie and redirecting to {}",
        redirect_url
    );

    // Build response with Set-Cookie header(s) and redirect
    let mut headers = HeaderMap::new();
    headers.insert(
        header::LOCATION,
        HeaderValue::from_str(&redirect_url).map_err(|e| {
            tracing::error!("Invalid redirect URL: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Invalid redirect URL".to_string(),
            )
        })?,
    );
    headers.insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).map_err(|e| {
            tracing::error!("Failed to build Set-Cookie header: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal error".to_string(),
            )
        })?,
    );
    if let Some(domain) = &state.cookie_settings.cookie_domain {
        let legacy = cookie_helpers::clear_legacy_domain_cookie(domain, &state.cookie_settings);
        if let Ok(val) = HeaderValue::from_str(&legacy) {
            headers.append(header::SET_COOKIE, val);
        }
    }
    let response = (StatusCode::FOUND, headers).into_response();

    Ok(response)
}

#[derive(Debug, Deserialize)]
pub struct CliAuthSuccessQuery {
    pub success: Option<bool>,
    pub error: Option<String>,
}

/// Handler for CLI authentication success/failure page
///
/// This endpoint is used to show a styled success or error page when CLI login completes.
/// The CLI callback redirects to this endpoint instead of showing a basic HTML page.
#[instrument(skip(state))]
pub async fn cli_auth_success(
    State(state): State<AppState>,
    Query(params): Query<CliAuthSuccessQuery>,
) -> Result<Response, (StatusCode, String)> {
    let success = params.success.unwrap_or(true);

    // Load CLI success template
    let static_dir = state.server_settings.static_dir.as_deref().ok_or_else(|| {
        tracing::error!("static_dir not configured");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Static dir not configured".to_string(),
        )
    })?;

    let tera = load_auth_template(static_dir, "cli-auth-success.html.tera").await?;

    // Render success template
    let mut context = tera::Context::new();
    context.insert("success", &success);
    if let Some(error) = params.error {
        context.insert("error_message", &error);
    }

    let html = tera
        .render("cli-auth-success.html.tera", &context)
        .map_err(|e| {
            tracing::error!("Failed to render template: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Template rendering error".to_string(),
            )
        })?;

    tracing::info!("Showing CLI auth success page (success={})", success);

    Ok(Html(html).into_response())
}

/// JWKS (JSON Web Key Set) endpoint
///
/// Returns the public keys used to sign Rise-issued RS256 JWTs.
/// Deployed applications can use this endpoint to validate Rise-issued tokens.
#[instrument(skip(state))]
pub async fn jwks(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    tracing::debug!("JWKS endpoint called");

    let jwks = state.jwt_signer.generate_jwks().map_err(|e| {
        tracing::error!("Failed to generate JWKS: {:#}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to generate JWKS".to_string(),
        )
    })?;

    Ok(Json(jwks))
}

/// OpenID Connect Discovery endpoint
///
/// Returns OpenID Provider metadata as per OpenID Connect Discovery 1.0.
/// Applications can use this to discover the JWKS endpoint and other metadata.
#[instrument(skip(state))]
pub async fn openid_configuration(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    tracing::debug!("OpenID configuration endpoint called");

    let jwks_uri = format!("{}/api/v1/auth/jwks", state.public_url);
    let authorization_endpoint = format!("{}/api/v1/auth/authorize", state.public_url);
    let token_endpoint = format!("{}/api/v1/auth/code/exchange", state.public_url);

    let config = serde_json::json!({
        "issuer": state.public_url,
        "authorization_endpoint": authorization_endpoint,
        "token_endpoint": token_endpoint,
        "jwks_uri": jwks_uri,
        "response_types_supported": ["code", "token", "id_token"],
        "id_token_signing_alg_values_supported": ["RS256", "HS256"],
        "subject_types_supported": ["public"],
        "claims_supported": ["sub", "email", "name", "groups", "iat", "exp", "iss", "aud"]
    });

    Ok(Json(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_redirect_url_relative_paths() {
        let public_url = "https://rise.dev";

        // Valid relative paths
        assert_eq!(validate_redirect_url("/", public_url), "/");
        assert_eq!(
            validate_redirect_url("/dashboard", public_url),
            "/dashboard"
        );
        assert_eq!(
            validate_redirect_url("/app/project/123", public_url),
            "/app/project/123"
        );

        // Protocol-relative URLs should be blocked
        assert_eq!(validate_redirect_url("//evil.com", public_url), "/");
        assert_eq!(validate_redirect_url("//evil.com/path", public_url), "/");
    }

    #[test]
    fn test_validate_redirect_url_dangerous_schemes() {
        let public_url = "https://rise.dev";

        // JavaScript URLs should be blocked
        assert_eq!(
            validate_redirect_url("javascript:alert('xss')", public_url),
            "/"
        );

        // Data URLs should be blocked
        assert_eq!(
            validate_redirect_url("data:text/html,<script>alert('xss')</script>", public_url),
            "/"
        );

        // vbscript URLs should be blocked
        assert_eq!(
            validate_redirect_url("vbscript:msgbox('xss')", public_url),
            "/"
        );
    }

    #[test]
    fn test_validate_redirect_url_same_domain() {
        let public_url = "https://rise.dev";

        // Same domain should be allowed
        assert_eq!(
            validate_redirect_url("https://rise.dev/dashboard", public_url),
            "https://rise.dev/dashboard"
        );

        // Same domain with port should be allowed
        assert_eq!(
            validate_redirect_url("https://rise.dev:8080/dashboard", public_url),
            "https://rise.dev:8080/dashboard"
        );
    }

    #[test]
    fn test_validate_redirect_url_subdomains() {
        let public_url = "https://rise.dev";

        // Subdomain should be allowed
        assert_eq!(
            validate_redirect_url("https://app.rise.dev/dashboard", public_url),
            "https://app.rise.dev/dashboard"
        );

        assert_eq!(
            validate_redirect_url("https://staging.rise.dev/dashboard", public_url),
            "https://staging.rise.dev/dashboard"
        );

        // Multi-level subdomain should be allowed
        assert_eq!(
            validate_redirect_url("https://my-project.app.rise.dev/", public_url),
            "https://my-project.app.rise.dev/"
        );
    }

    #[test]
    fn test_validate_redirect_url_external_domains() {
        let public_url = "https://rise.dev";

        // External domains should be blocked
        assert_eq!(validate_redirect_url("https://evil.com", public_url), "/");

        assert_eq!(
            validate_redirect_url("https://phishing.site/login", public_url),
            "/"
        );

        // Domains that look similar but are not subdomains should be blocked
        assert_eq!(
            validate_redirect_url("https://rise.dev.evil.com", public_url),
            "/"
        );
    }

    #[test]
    fn test_validate_redirect_url_localhost() {
        let public_url = "http://localhost:3000";

        // localhost to localhost should be allowed
        assert_eq!(
            validate_redirect_url("http://localhost:3000/dashboard", public_url),
            "http://localhost:3000/dashboard"
        );

        assert_eq!(
            validate_redirect_url("http://127.0.0.1:3000/dashboard", public_url),
            "http://127.0.0.1:3000/dashboard"
        );

        // Malicious localhost URLs with invalid ports should be rejected during parsing
        // The URL parser will fail to parse "localhost:evil.com" as a valid port
        assert_eq!(
            validate_redirect_url("http://localhost:evil.com/path", public_url),
            "/"
        );

        // But external URLs should still be blocked even when public_url is localhost
        assert_eq!(validate_redirect_url("https://evil.com", public_url), "/");
    }

    #[test]
    fn test_validate_redirect_url_localhost_production_blocked() {
        let public_url = "https://rise.dev";

        // localhost should be blocked when public_url is not localhost
        assert_eq!(
            validate_redirect_url("http://localhost:3000/dashboard", public_url),
            "/"
        );

        assert_eq!(
            validate_redirect_url("http://127.0.0.1:3000/dashboard", public_url),
            "/"
        );
    }

    #[test]
    fn test_validate_redirect_url_empty_and_invalid() {
        let public_url = "https://rise.dev";

        // Empty string should return fallback
        assert_eq!(validate_redirect_url("", public_url), "/");

        // Whitespace only should return fallback
        assert_eq!(validate_redirect_url("   ", public_url), "/");

        // Invalid URLs should return fallback
        assert_eq!(validate_redirect_url("not a url", public_url), "/");
    }

    #[test]
    fn test_validate_redirect_url_http_vs_https() {
        let public_url = "https://rise.dev";

        // HTTP URLs should be allowed for same domain
        assert_eq!(
            validate_redirect_url("http://rise.dev/dashboard", public_url),
            "http://rise.dev/dashboard"
        );

        // HTTPS URLs should be allowed for same domain
        assert_eq!(
            validate_redirect_url("https://rise.dev/dashboard", public_url),
            "https://rise.dev/dashboard"
        );
    }
}
