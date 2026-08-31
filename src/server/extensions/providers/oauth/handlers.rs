use super::models::{
    AuthorizeFlowQuery, CallbackRequest, OAuth2ErrorResponse, OAuth2TokenResponse, OAuthCodeState,
    OAuthExtensionSpec, OAuthExtensionStatus, OAuthState, TokenRequest,
};
use crate::db::{extensions as db_extensions, oauth_transient_state, projects as db_projects};
use crate::server::auth::redirect::{is_loopback_host, origins_match};
use crate::server::state::AppState;
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Redirect, Response},
    Json,
};
use base64::Engine;
use chrono::Utc;
use std::sync::Arc;
use std::time::Duration as StdDuration;
use tracing::{debug, error, info, warn};
use url::Url;
use uuid::Uuid;

/// OIDC Discovery document (partial)
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct OidcDiscoveryDocument {
    authorization_endpoint: Option<String>,
    token_endpoint: Option<String>,
    jwks_uri: Option<String>,
}

#[cfg(test)]
mod redirect_tests {
    use super::redirect_uri_is_allowed;
    use url::Url;

    fn is_allowed(redirect: &str, public_url: &str, app_origins: &[&str]) -> bool {
        let Ok(redirect) = Url::parse(redirect) else {
            return false;
        };
        let public_url = Url::parse(public_url).unwrap();
        let app_origins = app_origins
            .iter()
            .map(|origin| Url::parse(origin).unwrap())
            .collect::<Vec<_>>();

        redirect_uri_is_allowed(&redirect, &public_url, &app_origins)
    }

    #[test]
    fn oauth_redirect_requires_an_exact_trusted_origin() {
        let public_url = "https://rise.example.com";
        let app_origins = ["https://victim.example.com"];

        for redirect in [
            "https://rise.example.com/oidc/done",
            "https://rise.example.com:443/oidc/done",
            "https://victim.example.com/callback",
            "https://victim.example.com:443/callback?state=ok",
            "https://victim.example.com./callback",
        ] {
            assert!(
                is_allowed(redirect, public_url, &app_origins),
                "expected {redirect} to be allowed"
            );
        }

        for redirect in [
            // Raw string prefix attacks: both URLs resolve to evil.com.
            "https://victim.example.com.evil.com/callback",
            "https://victim.example.com@evil.com/callback",
            // Origin mismatches.
            "https://victim.example.com:444/callback",
            "http://victim.example.com/callback",
            "https://sub.victim.example.com/callback",
            "https://victim-example.com/callback",
            "https://other.example.com/callback",
        ] {
            assert!(
                !is_allowed(redirect, public_url, &app_origins),
                "expected {redirect} to be rejected"
            );
        }
    }

    #[test]
    fn oauth_redirect_rejects_invalid_and_non_http_schemes_before_loopback() {
        let public_url = "http://rise.localhost:3000";

        for redirect in [
            "javascript://localhost/alert(1)",
            "data:text/html,hello",
            "file://localhost/tmp/code",
            "//localhost/callback",
            "not a URL",
        ] {
            assert!(
                !is_allowed(redirect, public_url, &[]),
                "expected {redirect} to be rejected"
            );
        }
    }

    #[test]
    fn oauth_loopback_exception_supports_local_apps_using_remote_rise() {
        for redirect in [
            "http://localhost:5173/callback",
            "http://app.localhost:8080/callback",
            "http://127.0.0.1:9000/callback",
            "http://[::1]:9000/callback",
        ] {
            assert!(
                is_allowed(redirect, "http://rise.localhost:3000", &[]),
                "expected {redirect} to be allowed for local Rise"
            );
            assert!(
                is_allowed(redirect, "https://rise.example.com", &[]),
                "expected {redirect} to be allowed for local development against remote Rise"
            );
        }

        assert!(!is_allowed(
            "http://localhost.evil.com/callback",
            "http://rise.localhost:3000",
            &[]
        ));
    }

    #[test]
    fn oauth_redirect_supports_kubernetes_https_and_docker_local_http_origins() {
        assert!(is_allowed(
            "https://app.example.com/oauth/callback",
            "https://rise.example.com",
            &["https://app.example.com"]
        ));
        assert!(is_allowed(
            "http://app.rise.localhost/oauth/callback",
            "http://rise.localhost:3000",
            &["http://app.rise.localhost"]
        ));
        assert!(is_allowed(
            "http://app.rise.localhost:80/oauth/callback",
            "http://rise.localhost:3000",
            &["http://app.rise.localhost"]
        ));

        assert!(!is_allowed(
            "http://app.example.com/oauth/callback",
            "https://rise.example.com",
            &["https://app.example.com"]
        ));
        assert!(!is_allowed(
            "https://app.rise.localhost/oauth/callback",
            "http://rise.localhost:3000",
            &["http://app.rise.localhost"]
        ));
    }
}

/// Resolved OAuth endpoints from spec or OIDC discovery
#[derive(Debug, Clone)]
struct ResolvedEndpoints {
    authorization_endpoint: String,
    token_endpoint: String,
}

/// Fetch OIDC discovery document from issuer URL
async fn fetch_oidc_discovery(
    issuer_url: &str,
    ssrf_config: &crate::server::ssrf::SsrfConfig,
) -> Result<OidcDiscoveryDocument, String> {
    let discovery_url = format!(
        "{}/.well-known/openid-configuration",
        issuer_url.trim_end_matches('/')
    );

    // SSRF-validate the discovery URL before fetching
    crate::server::ssrf::validate_url(&discovery_url, ssrf_config)
        .await
        .map_err(|e| {
            error!("OIDC discovery URL failed SSRF validation: {}", e);
            format!("OIDC discovery URL failed SSRF validation: {}", e)
        })?;

    let http_client = crate::server::ssrf::safe_client(ssrf_config);
    let response = http_client.get(&discovery_url).send().await.map_err(|e| {
        error!(
            "Failed to fetch OIDC discovery from {}: {:?}",
            discovery_url, e
        );
        format!("Failed to fetch OIDC discovery: {}", e)
    })?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unable to read error response".to_string());
        error!(
            "OIDC discovery failed with status {}: {}",
            status, error_text
        );
        return Err(format!("OIDC discovery failed: {}", status));
    }

    response.json().await.map_err(|e| {
        error!("Failed to parse OIDC discovery response: {:?}", e);
        format!("Failed to parse OIDC discovery: {}", e)
    })
}

/// Resolve OAuth endpoints from spec, falling back to OIDC discovery
async fn resolve_oauth_endpoints(
    spec: &OAuthExtensionSpec,
    ssrf_config: &crate::server::ssrf::SsrfConfig,
) -> Result<ResolvedEndpoints, String> {
    // If both endpoints are provided in spec, use them directly
    if let (Some(auth), Some(token)) = (&spec.authorization_endpoint, &spec.token_endpoint) {
        return Ok(ResolvedEndpoints {
            authorization_endpoint: auth.clone(),
            token_endpoint: token.clone(),
        });
    }

    // Fetch OIDC discovery document
    let discovery = fetch_oidc_discovery(&spec.issuer_url, ssrf_config).await?;

    // Use spec override if provided, otherwise use discovery
    let authorization_endpoint = spec
        .authorization_endpoint
        .clone()
        .or(discovery.authorization_endpoint)
        .ok_or_else(|| "No authorization_endpoint in spec or OIDC discovery".to_string())?;

    let token_endpoint = spec
        .token_endpoint
        .clone()
        .or(discovery.token_endpoint)
        .ok_or_else(|| "No token_endpoint in spec or OIDC discovery".to_string())?;

    Ok(ResolvedEndpoints {
        authorization_endpoint,
        token_endpoint,
    })
}

/// Generate a random state token for CSRF protection
fn generate_state_token() -> String {
    use rand::RngExt;
    let mut rng = rand::rng();
    (0..32)
        .map(|_| format!("{:02x}", rng.random::<u8>()))
        .collect()
}

/// Generate a PKCE code verifier (random string)
fn generate_code_verifier() -> String {
    use rand::RngExt;
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut rng = rand::rng();
    (0..128)
        .map(|_| {
            let idx = rng.random_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

/// Generate a PKCE code challenge from a code verifier (SHA256 hash, base64url encoded)
fn generate_code_challenge(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let hash = hasher.finalize();

    // Base64url encode (no padding)
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hash)
}

/// Validate CORS origin against allowed origins for a project
///
/// Returns the allowed origin if valid, None otherwise.
/// Allowed origins:
/// - localhost (any port) for local development
/// - Rise public URL
/// - Project deployment URLs (including custom domains)
async fn validate_cors_origin(
    pool: &sqlx::PgPool,
    origin: &str,
    project: &crate::db::models::Project,
    rise_public_url: &str,
    deployment_backend: &Arc<dyn crate::server::deployment::controller::DeploymentBackend>,
) -> Option<String> {
    let origin_url = match Url::parse(origin) {
        Ok(url) => url,
        Err(_) => return None,
    };

    // Allow localhost for local development (any port)
    if let Some(host) = origin_url.host_str() {
        if host == "localhost" || host == "127.0.0.1" {
            return Some(origin.to_string());
        }
    }

    // Allow if origin matches Rise public URL (same origin)
    if let Ok(rise_url) = Url::parse(rise_public_url) {
        if origin_url.host() == rise_url.host()
            && origin_url.port() == rise_url.port()
            && origin_url.scheme() == rise_url.scheme()
        {
            return Some(origin.to_string());
        }
    }

    // Check project's deployment URLs
    let all_deployments =
        match crate::db::deployments::get_active_deployments_for_project(pool, project.id).await {
            Ok(deployments) => deployments,
            Err(e) => {
                warn!(
                    "Failed to fetch active deployments for project {}: {:?}",
                    project.name, e
                );
                return None;
            }
        };

    for deployment in &all_deployments {
        match deployment_backend
            .get_deployment_urls(deployment, project)
            .await
        {
            Ok(urls) => {
                // Check default URL
                if !urls.default_url.is_empty() {
                    if let Ok(deployment_url) = Url::parse(&urls.default_url) {
                        if origin_url.host() == deployment_url.host()
                            && origin_url.port() == deployment_url.port()
                            && origin_url.scheme() == deployment_url.scheme()
                        {
                            return Some(origin.to_string());
                        }
                    }
                }

                // Check custom domain URLs
                for custom_url in &urls.custom_domain_urls {
                    if let Ok(custom_domain_url) = Url::parse(custom_url) {
                        if origin_url.host() == custom_domain_url.host()
                            && origin_url.port() == custom_domain_url.port()
                            && origin_url.scheme() == custom_domain_url.scheme()
                        {
                            return Some(origin.to_string());
                        }
                    }
                }
            }
            Err(e) => {
                warn!(
                    "Failed to get deployment URLs for deployment {}: {:?}",
                    deployment.deployment_id, e
                );
            }
        }
    }

    None
}

/// Create CORS response headers for allowed origin
fn cors_headers(origin: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Ok(origin_value) = HeaderValue::from_str(origin) {
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin_value);
    }
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("POST, OPTIONS"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("Content-Type"),
    );
    headers.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("86400"), // 24 hours
    );
    headers
}

/// Decide whether a parsed OAuth redirect target is allowed.
///
/// Redirects may target the exact Rise origin or one of this project's exact
/// active deployment origins. HTTP loopback targets (including `*.localhost`)
/// are a development exception and are accepted on any port so `rise run` can
/// use a remote Rise installation.
fn redirect_uri_is_allowed(redirect: &Url, public_url: &Url, app_origins: &[Url]) -> bool {
    // This gate must run before the loopback exception: URLs such as
    // `javascript://localhost/...` also have a parsed host.
    if !matches!(redirect.scheme(), "http" | "https") {
        return false;
    }

    let loopback_redirect_allowed =
        redirect.scheme() == "http" && redirect.host_str().is_some_and(is_loopback_host);

    loopback_redirect_allowed
        || origins_match(redirect, public_url)
        || app_origins
            .iter()
            .any(|allowed| origins_match(redirect, allowed))
}

/// Validate redirect URI against allowed origins
async fn validate_redirect_uri(
    pool: &sqlx::PgPool,
    redirect_uri: &str,
    project: &crate::db::models::Project,
    rise_public_url: &str,
    deployment_backend: &Arc<dyn crate::server::deployment::controller::DeploymentBackend>,
) -> Result<(), String> {
    let redirect_url =
        Url::parse(redirect_uri).map_err(|e| format!("Invalid redirect URI: {}", e))?;
    let public_url = Url::parse(rise_public_url)
        .map_err(|e| format!("Invalid configured Rise public URL: {}", e))?;
    let mut app_origins = Vec::new();

    // Get project's deployment URLs from the deployment backend
    // Check all active deployments (including staging/non-default groups)
    let all_deployments =
        match crate::db::deployments::get_active_deployments_for_project(pool, project.id).await {
            Ok(deployments) => deployments,
            Err(e) => {
                warn!(
                    "Failed to fetch active deployments for project {}: {:?}",
                    project.name, e
                );
                vec![]
            }
        };

    // Resolve all configured deployment origins. Invalid backend-provided URLs
    // are ignored rather than treated as trusted string prefixes.
    for deployment in &all_deployments {
        match deployment_backend
            .get_deployment_urls(deployment, project)
            .await
        {
            Ok(urls) => {
                if !urls.default_url.is_empty() {
                    match Url::parse(&urls.default_url) {
                        Ok(url) => app_origins.push(url),
                        Err(error) => warn!(
                            url = %urls.default_url,
                            deployment_id = %deployment.deployment_id,
                            %error,
                            "Ignoring invalid deployment URL while validating OAuth redirect"
                        ),
                    }
                }

                for custom_url in &urls.custom_domain_urls {
                    match Url::parse(custom_url) {
                        Ok(url) => app_origins.push(url),
                        Err(error) => warn!(
                            url = %custom_url,
                            deployment_id = %deployment.deployment_id,
                            %error,
                            "Ignoring invalid custom domain URL while validating OAuth redirect"
                        ),
                    }
                }
            }
            Err(e) => {
                warn!(
                    "Failed to get deployment URLs for deployment {}: {:?}",
                    deployment.deployment_id, e
                );
            }
        }
    }

    if redirect_uri_is_allowed(&redirect_url, &public_url, &app_origins) {
        return Ok(());
    }

    Err(format!(
        "Invalid redirect URI: origin is not authorized for this project. Allowed: the Rise public origin ({}), any active deployment origin, or an HTTP loopback origin for local development",
        rise_public_url
    ))
}

/// Initiate OAuth authorization flow
///
/// GET /oidc/{project}/{extension}/authorize
///
/// Query params:
/// - redirect_uri (optional): Where to redirect after auth (for local dev/custom domains)
/// - state (optional): Application's CSRF state parameter (passed through to final redirect)
/// - code_challenge (optional): PKCE code challenge for public clients (SPAs)
/// - code_challenge_method (optional): PKCE method (only "S256" is supported, and is the default)
pub async fn authorize(
    State(state): State<AppState>,
    Path((project_name, extension_name)): Path<(String, String)>,
    headers: HeaderMap,
    Query(req): Query<AuthorizeFlowQuery>,
) -> Result<Response, (StatusCode, String)> {
    debug!(
        "OAuth authorize request for project={}, extension={}",
        project_name, extension_name
    );

    // Rate limiting (keyed per project+IP so different projects have independent budgets)
    let client_ip = crate::server::rate_limit::extract_client_ip(&headers);
    let session_key = crate::server::rate_limit::extract_session_key(&headers);
    if let Err(retry_after) = state
        .oauth_rate_limiter
        .increment_and_check(&client_ip, session_key.as_deref(), &project_name)
        .await
    {
        tracing::warn!(
            ip = %client_ip,
            project = %project_name,
            extension = %extension_name,
            "OAuth authorize endpoint rate limited"
        );
        return Ok(crate::server::rate_limit::rate_limit_response(retry_after));
    }

    // Get project
    let project = db_projects::find_by_name(&state.db_pool, &project_name)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Project not found".to_string()))?;

    // Get OAuth extension
    let extension =
        db_extensions::find_by_project_and_name(&state.db_pool, project.id, &extension_name)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .ok_or((
                StatusCode::NOT_FOUND,
                "OAuth extension not configured".to_string(),
            ))?;

    // Verify extension type is oauth
    if extension.extension_type != "oauth" {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "Extension '{}' is not an OAuth extension (type: {})",
                extension_name, extension.extension_type
            ),
        ));
    }

    // Parse spec
    let spec: OAuthExtensionSpec = serde_json::from_value(extension.spec.clone()).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid spec: {}", e),
        )
    })?;

    // Validate PKCE parameters if provided
    if let Some(ref code_challenge) = req.code_challenge {
        // RFC 7636: code_challenge must be 43-128 characters, base64url charset
        if code_challenge.len() < 43 || code_challenge.len() > 128 {
            return Err((
                StatusCode::BAD_REQUEST,
                "code_challenge must be 43-128 characters".to_string(),
            ));
        }
        if !code_challenge
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err((
                StatusCode::BAD_REQUEST,
                "code_challenge contains invalid characters (must be base64url)".to_string(),
            ));
        }

        let method = req.code_challenge_method.as_deref().unwrap_or("S256");
        if method != "S256" {
            return Err((
                StatusCode::BAD_REQUEST,
                format!(
                    "Unsupported code_challenge_method '{}'. Only 'S256' is supported.",
                    method
                ),
            ));
        }
    }

    // Determine final redirect URI
    let final_redirect_uri = if let Some(ref uri) = req.redirect_uri {
        // Validate redirect URI
        validate_redirect_uri(
            &state.db_pool,
            uri,
            &project,
            &state.public_url,
            &state.deployment_backend,
        )
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
        uri.clone()
    } else {
        // Default to project's primary URL
        // Parse API URL to construct project URL
        let api_url = Url::parse(&state.public_url).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Invalid API URL configuration: {}", e),
            )
        })?;

        let api_host = api_url.host_str().ok_or((
            StatusCode::INTERNAL_SERVER_ERROR,
            "Missing host in API URL".to_string(),
        ))?;

        let project_host = if let Some(base_domain) = api_host.strip_prefix("api.") {
            // api.domain.com -> project.domain.com
            format!("{}.{}", project_name, base_domain)
        } else if api_host == "localhost" || api_host == "127.0.0.1" {
            // localhost -> project.apps.rise.local
            format!("{}.apps.rise.local", project_name)
        } else {
            // domain.com -> project.domain.com
            format!("{}.{}", project_name, api_host)
        };

        let scheme = api_url.scheme();

        // For deployed Rise apps, use port 8080 (the default app port)
        // For production with proper DNS, don't include port
        if api_host == "localhost" || api_host == "127.0.0.1" {
            // Deployed locally - use port 8080
            format!("{}://{}:8080/", scheme, project_host)
        } else {
            // Production - no port (handled by ingress)
            format!("{}://{}/", scheme, project_host)
        }
    };

    // Generate CSRF state token
    let state_token = generate_state_token();

    // Generate PKCE code verifier and challenge
    let code_verifier = generate_code_verifier();
    let code_challenge = generate_code_challenge(&code_verifier);

    // Store OAuth state in cache
    let oauth_state = OAuthState {
        redirect_uri: Some(final_redirect_uri),
        application_state: req.state,
        project_name: project_name.clone(),
        extension_name: extension_name.clone(),
        code_verifier,
        created_at: Utc::now(),
        client_code_challenge: req.code_challenge,
        client_code_challenge_method: req.code_challenge_method,
    };

    // Store state in DB (10-minute TTL)
    oauth_transient_state::insert(
        &state.db_pool,
        &state_token,
        &oauth_state,
        StdDuration::from_secs(600),
    )
    .await
    .map_err(|e| {
        error!("Failed to store OAuth state: {:?}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to initiate OAuth flow".to_string(),
        )
    })?;

    // Compute callback redirect URI for this extension
    // Use the same scheme and host as the API URL
    let api_url = Url::parse(&state.public_url).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid API URL configuration: {}", e),
        )
    })?;

    let api_host = api_url.host_str().ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        "Missing host in API URL".to_string(),
    ))?;

    let redirect_uri = if let Some(port) = api_url.port() {
        format!(
            "{}://{}:{}/oidc/{}/{}/callback",
            api_url.scheme(),
            api_host,
            port,
            project_name,
            extension_name
        )
    } else {
        format!(
            "{}://{}/oidc/{}/{}/callback",
            api_url.scheme(),
            api_host,
            project_name,
            extension_name
        )
    };

    // Resolve OAuth endpoints (from spec or OIDC discovery)
    let ssrf_config = &state.server_settings.ssrf;
    let endpoints = resolve_oauth_endpoints(&spec, ssrf_config)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to resolve OAuth endpoints: {}", e),
            )
        })?;

    // Build authorization URL
    let mut auth_url = Url::parse(&endpoints.authorization_endpoint).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid authorization endpoint: {}", e),
        )
    })?;

    auth_url
        .query_pairs_mut()
        .append_pair("client_id", &spec.client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", &spec.scopes.join(" "))
        .append_pair("state", &state_token)
        .append_pair("code_challenge", &code_challenge)
        .append_pair("code_challenge_method", "S256");

    debug!("Redirecting to OAuth provider: {}", auth_url.as_str());

    // Redirect to OAuth provider
    Ok(Redirect::to(auth_url.as_str()).into_response())
}

/// Handle OAuth callback from provider
///
/// GET /oidc/{project}/{extension}/callback
///
/// Query params:
/// - code: Authorization code from provider
/// - state: CSRF state token
pub async fn callback(
    State(state): State<AppState>,
    Path((project_name, extension_name)): Path<(String, String)>,
    headers: HeaderMap,
    Query(req): Query<CallbackRequest>,
) -> Result<Response, (StatusCode, String)> {
    debug!(
        "OAuth callback for project={}, extension={}",
        project_name, extension_name
    );

    // Rate limiting (keyed per project+IP so different projects have independent budgets)
    let client_ip = crate::server::rate_limit::extract_client_ip(&headers);
    let session_key = crate::server::rate_limit::extract_session_key(&headers);
    if let Err(retry_after) = state
        .oauth_rate_limiter
        .increment_and_check(&client_ip, session_key.as_deref(), &project_name)
        .await
    {
        tracing::warn!(
            ip = %client_ip,
            project = %project_name,
            extension = %extension_name,
            "OAuth callback endpoint rate limited"
        );
        return Ok(crate::server::rate_limit::rate_limit_response(retry_after));
    }

    let claimed_state = oauth_transient_state::claim(
        &state.db_pool,
        &req.state,
        Uuid::new_v4(),
        oauth_transient_state::CLAIM_GRACE_TTL,
    )
    .await
    .map_err(|e| {
        error!("Failed to retrieve OAuth state: {:?}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "OAuth callback failed".to_string(),
        )
    })?
    .ok_or((StatusCode::BAD_REQUEST, "Invalid state token".to_string()))?;

    let claimed_state: oauth_transient_state::ClaimedState<OAuthState> = claimed_state;

    // Verify project and extension match
    if claimed_state.project_name != project_name || claimed_state.extension_name != extension_name
    {
        return Err((StatusCode::BAD_REQUEST, "State mismatch".to_string()));
    }

    // Detect test vs real flow early (before expensive token parsing/encryption)
    let final_redirect_uri = match claimed_state.redirect_uri.clone() {
        Some(uri) => uri,
        None => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "Missing redirect URI in state".to_string(),
            ))
        }
    };

    let is_test_flow = final_redirect_uri.starts_with(&state.public_url);

    debug!(
        "OAuth callback: flow_type={}, final_redirect_uri={}",
        if is_test_flow { "test" } else { "real" },
        final_redirect_uri
    );

    // Get project
    let project = db_projects::find_by_name(&state.db_pool, &project_name)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Project not found".to_string()))?;

    // Get OAuth extension
    let extension =
        db_extensions::find_by_project_and_name(&state.db_pool, project.id, &extension_name)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .ok_or((
                StatusCode::NOT_FOUND,
                "OAuth extension not configured".to_string(),
            ))?;

    // Parse spec
    let spec: OAuthExtensionSpec = serde_json::from_value(extension.spec.clone()).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid spec: {}", e),
        )
    })?;

    // Resolve OAuth provider's client secret (prefers encrypted in spec, falls back to env var ref)
    let Some(encryption_provider) = state.encryption_provider.as_ref() else {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "Encryption provider not configured".to_string(),
        ));
    };

    let ssrf_config = &state.server_settings.ssrf;

    use super::provider::{OAuthProvider, OAuthProviderConfig};
    let oauth_provider = OAuthProvider::new(OAuthProviderConfig {
        db_pool: state.db_pool.clone(),
        encryption_provider: encryption_provider.clone(),
        http_client: crate::server::ssrf::safe_client(ssrf_config),
        api_domain: state.public_url.clone(),
    });

    let client_secret = oauth_provider
        .resolve_oauth_client_secret(project.id, &spec)
        .await
        .map_err(|e| {
            error!("Failed to resolve OAuth client secret: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to resolve OAuth client secret: {}", e),
            )
        })?;

    // Compute callback redirect URI - must match exactly what was sent in authorize request
    let api_url = Url::parse(&state.public_url).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid API URL configuration: {}", e),
        )
    })?;

    let api_host = api_url.host_str().ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        "Missing host in API URL".to_string(),
    ))?;

    let redirect_uri = if let Some(port) = api_url.port() {
        format!(
            "{}://{}:{}/oidc/{}/{}/callback",
            api_url.scheme(),
            api_host,
            port,
            project_name,
            extension_name
        )
    } else {
        format!(
            "{}://{}/oidc/{}/{}/callback",
            api_url.scheme(),
            api_host,
            project_name,
            extension_name
        )
    };

    // Resolve OAuth endpoints (from spec or OIDC discovery)
    let endpoints = resolve_oauth_endpoints(&spec, ssrf_config)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to resolve OAuth endpoints: {}", e),
            )
        })?;

    // SSRF-validate the token endpoint before exchanging credentials
    crate::server::ssrf::validate_url(&endpoints.token_endpoint, ssrf_config)
        .await
        .map_err(|e| {
            error!("Token endpoint failed SSRF validation: {}", e);
            (
                StatusCode::BAD_REQUEST,
                format!("Token endpoint failed SSRF validation: {}", e),
            )
        })?;

    // Exchange authorization code for tokens (with PKCE code verifier)
    let http_client = crate::server::ssrf::safe_client(ssrf_config);
    let response = http_client
        .post(&endpoints.token_endpoint)
        .header("Accept", "application/json")
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", &req.code),
            ("client_id", &spec.client_id),
            ("client_secret", &client_secret),
            ("redirect_uri", &redirect_uri),
            ("code_verifier", &claimed_state.code_verifier),
        ])
        .send()
        .await
        .map_err(|e| {
            error!("Token exchange request failed: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Token exchange request failed: {}", e),
            )
        })?;

    // Branch based on flow type: test flows skip token parsing/encryption
    if is_test_flow {
        // ===== TEST FLOW: Simplified path (no token parsing/encryption) =====
        info!(
            "Processing test OAuth flow for project {} extension {}",
            project_name, extension_name
        );

        // Check if token exchange was successful
        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unable to read error response".to_string());
            error!(
                "Token exchange failed with status {}: {}",
                status, error_text
            );

            // Update extension status with error
            let mut ext_status: OAuthExtensionStatus =
                serde_json::from_value(extension.status.clone()).unwrap_or_default();
            ext_status.error = Some(format!(
                "Token exchange failed with status {}: {}",
                status, error_text
            ));
            ext_status.auth_verified = false;

            db_extensions::update_status(
                &state.db_pool,
                project.id,
                &extension_name,
                &serde_json::to_value(&ext_status).unwrap(),
            )
            .await
            .map_err(|e| {
                warn!("Failed to update extension status: {:?}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to update status: {}", e),
                )
            })?;

            // Redirect to UI with error
            let mut redirect_url = Url::parse(&final_redirect_uri).map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Invalid redirect URI: {}", e),
                )
            })?;
            redirect_url
                .query_pairs_mut()
                .append_pair("error", "oauth_token_exchange_failed");

            if let Err(e) = claimed_state.finalize().await {
                warn!(
                    "Failed to finalize OAuth callback state (response already built, row will expire via TTL): {:?}",
                    e
                );
            }
            return Ok(Redirect::to(redirect_url.as_str()).into_response());
        }

        // Token exchange successful - update extension status
        let mut ext_status: OAuthExtensionStatus =
            serde_json::from_value(extension.status.clone()).unwrap_or_default();
        ext_status.redirect_uri = Some(redirect_uri);
        ext_status.configured_at = Some(Utc::now());
        ext_status.auth_verified = true;
        ext_status.error = None;

        db_extensions::update_status(
            &state.db_pool,
            project.id,
            &extension_name,
            &serde_json::to_value(&ext_status).unwrap(),
        )
        .await
        .map_err(|e| {
            warn!("Failed to update extension status: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to update status: {}", e),
            )
        })?;

        info!(
            "Completed test OAuth flow for project {} extension {}",
            project_name, extension_name
        );

        // Redirect back to Rise UI
        let redirect_url = Url::parse(&final_redirect_uri).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Invalid redirect URI: {}", e),
            )
        })?;

        if let Err(e) = claimed_state.finalize().await {
            warn!(
                "Failed to finalize OAuth callback state (response already built, row will expire via TTL): {:?}",
                e
            );
        }
        Ok(Redirect::to(redirect_url.as_str()).into_response())
    } else {
        // ===== REAL FLOW: Cache raw token response (no parsing) =====
        info!(
            "Processing real OAuth flow for project {} extension {}",
            project_name, extension_name
        );

        // Capture status code and Content-Type before consuming response
        let status_code = response.status().as_u16();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("application/json")
            .to_string();

        // Get raw response body (cache ALL responses - we're a passthrough proxy)
        let response_body = response.bytes().await.map_err(|e| {
            error!("Failed to read token response body: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to read token response".to_string(),
            )
        })?;

        // Encrypt raw response body
        let Some(encryption_provider) = state.encryption_provider.as_ref() else {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "Encryption provider not configured".to_string(),
            ));
        };

        let token_response_encrypted = encryption_provider
            .encrypt(&String::from_utf8_lossy(&response_body))
            .await
            .map_err(|e| {
                error!("Failed to encrypt token response: {:?}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to encrypt token response".to_string(),
                )
            })?;

        debug!(
            "Cached token response: status={}, content_type={}",
            status_code, content_type
        );

        // Update extension status (preserve credentials from existing status)
        let mut ext_status: OAuthExtensionStatus =
            serde_json::from_value(extension.status.clone()).unwrap_or_default();

        ext_status.redirect_uri = Some(redirect_uri);
        ext_status.configured_at = Some(Utc::now());
        ext_status.auth_verified = true;
        ext_status.error = None;

        db_extensions::update_status(
            &state.db_pool,
            project.id,
            &extension_name,
            &serde_json::to_value(&ext_status).unwrap(),
        )
        .await
        .map_err(|e| {
            warn!("Failed to update extension status: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to update status: {}", e),
            )
        })?;

        // Generate authorization code for token exchange
        let authorization_code = generate_state_token();

        // Store encrypted raw token response in authorization code state (5-minute TTL, single-use)
        let code_state = OAuthCodeState {
            project_id: project.id,
            extension_name: extension_name.clone(),
            created_at: Utc::now(),
            redirect_uri: claimed_state.redirect_uri.clone(),
            code_challenge: claimed_state.client_code_challenge.clone(),
            code_challenge_method: claimed_state.client_code_challenge_method.clone(),
            token_response_encrypted,
            content_type,
            status_code,
        };

        // Store authorization code in DB (5-minute TTL, single-use via consume)
        oauth_transient_state::insert(
            &state.db_pool,
            &authorization_code,
            &code_state,
            StdDuration::from_secs(300),
        )
        .await
        .map_err(|e| {
            error!("Failed to store authorization code: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "OAuth callback failed".to_string(),
            )
        })?;

        // Build redirect URL with authorization code (RFC 6749)
        let mut redirect_url = Url::parse(&final_redirect_uri).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Invalid redirect URI: {}", e),
            )
        })?;

        // Add authorization code as query parameter (RFC 6749)
        redirect_url
            .query_pairs_mut()
            .append_pair("code", &authorization_code);

        // Pass through application's CSRF state
        if let Some(app_state) = claimed_state.application_state.clone() {
            redirect_url
                .query_pairs_mut()
                .append_pair("state", &app_state);
        }

        info!(
            "Generated authorization code for project {} extension {}",
            project_name, extension_name
        );

        info!(
            "OAuth callback complete: redirecting to {}",
            redirect_url.as_str()
        );

        if let Err(e) = claimed_state.finalize().await {
            warn!(
                "Failed to finalize OAuth callback state (response already built, row will expire via TTL): {:?}",
                e
            );
        }
        Ok(Redirect::to(redirect_url.as_str()).into_response())
    }
}

/// Validate PKCE code_verifier against code_challenge
/// Returns true if valid, false otherwise
fn validate_pkce(code_verifier: &str, code_challenge: &str, code_challenge_method: &str) -> bool {
    use sha2::{Digest, Sha256};
    use subtle::ConstantTimeEq;

    match code_challenge_method {
        "S256" => {
            // SHA256 hash the verifier
            let mut hasher = Sha256::new();
            hasher.update(code_verifier.as_bytes());
            let hash = hasher.finalize();

            // Base64url encode (no padding)
            let computed_challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hash);

            // Constant-time comparison
            computed_challenge
                .as_bytes()
                .ct_eq(code_challenge.as_bytes())
                .into()
        }
        _ => false,
    }
}

/// Create OAuth2 error response
fn oauth2_error(
    error: &str,
    description: Option<String>,
) -> (StatusCode, Json<OAuth2ErrorResponse>) {
    let status_code = match error {
        "invalid_request" => StatusCode::BAD_REQUEST,
        "invalid_client" => StatusCode::UNAUTHORIZED,
        "invalid_grant" => StatusCode::BAD_REQUEST,
        "unsupported_grant_type" => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };

    (
        status_code,
        Json(OAuth2ErrorResponse {
            error: error.to_string(),
            error_description: description,
        }),
    )
}

/// RFC 6749-compliant token endpoint with per-project CORS support
///
/// POST /oidc/{project}/{extension}/token
///
/// Grant types:
/// - authorization_code: Exchange authorization code for tokens
/// - refresh_token: Refresh access token
///
/// Client authentication:
/// - Confidential clients: Use client_id + client_secret
/// - Public clients: Use client_id + code_verifier (PKCE)
///
/// CORS:
/// - Validates Origin header against project-specific allowed origins
/// - Allows localhost (any port), Rise public URL, project deployment URLs
pub async fn token_endpoint(
    State(state): State<AppState>,
    Path((project_name, extension_name)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    body: String,
) -> Response {
    debug!(
        "Token endpoint request for project={}, extension={}",
        project_name, extension_name
    );

    // Extract Origin header early so it's available for CORS on all response paths
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());

    // Rate limiting: count all attempts (including invalid ones) to thwart brute-force
    // Keyed per project+IP so different projects have independent budgets.
    let client_ip = crate::server::rate_limit::extract_client_ip(&headers);
    let session_key = crate::server::rate_limit::extract_session_key(&headers);
    if let Err(retry_after) = state
        .oauth_rate_limiter
        .increment_and_check(&client_ip, session_key.as_deref(), &project_name)
        .await
    {
        tracing::warn!(
            ip = %client_ip,
            project = %project_name,
            extension = %extension_name,
            "OAuth token endpoint rate limited"
        );
        // Return a JSON error with CORS headers, consistent with other error responses
        let mut response = (
            StatusCode::TOO_MANY_REQUESTS,
            Json(OAuth2ErrorResponse {
                error: "rate_limit_exceeded".to_string(),
                error_description: Some("Rate limit exceeded. Please try again later.".to_string()),
            }),
        )
            .into_response();
        if let Ok(value) = HeaderValue::from_str(&retry_after.to_string()) {
            response
                .headers_mut()
                .insert(axum::http::header::RETRY_AFTER, value);
        }
        if let Some(ref origin_str) = origin {
            response.headers_mut().extend(cors_headers(origin_str));
        }
        return response;
    }

    // Inner function to handle the actual token logic
    let result = token_endpoint_inner(&state, &project_name, &extension_name, &headers, body).await;

    // Get CORS headers if Origin was provided and project exists
    let validated_cors_headers = if let Some(ref origin_str) = origin {
        // We need to get the project to validate CORS
        if let Ok(Some(project)) = db_projects::find_by_name(&state.db_pool, &project_name).await {
            validate_cors_origin(
                &state.db_pool,
                origin_str,
                &project,
                &state.public_url,
                &state.deployment_backend,
            )
            .await
            .map(|allowed| cors_headers(&allowed))
        } else {
            None
        }
    } else {
        None
    };

    // Build response with CORS headers
    // For error responses, always include CORS headers if Origin was provided (even if validation failed)
    // This ensures proper CORS error handling in the browser
    match result {
        Ok(mut response) => {
            // Response is already built by grant handler, just add CORS headers
            if let Some(cors) = validated_cors_headers {
                response.headers_mut().extend(cors);
            }
            response
        }
        Err((status, error_json)) => {
            let mut response = (status, error_json).into_response();
            // For errors, use validated CORS headers if available, otherwise echo back Origin
            if let Some(cors) = validated_cors_headers {
                response.headers_mut().extend(cors);
            } else if let Some(origin_str) = origin {
                // Even if CORS validation failed, include CORS headers so browser gets proper error
                response.headers_mut().extend(cors_headers(&origin_str));
            }
            response
        }
    }
}

/// Inner implementation of token endpoint logic
async fn token_endpoint_inner(
    state: &AppState,
    project_name: &str,
    extension_name: &str,
    headers: &axum::http::HeaderMap,
    body: String,
) -> Result<Response, (StatusCode, Json<OAuth2ErrorResponse>)> {
    // Parse request body (support both form-urlencoded and JSON)
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("application/x-www-form-urlencoded");

    let req: TokenRequest = if content_type.contains("application/json") {
        serde_json::from_str(&body).map_err(|e| {
            oauth2_error(
                "invalid_request",
                Some(format!("Invalid JSON request body: {}", e)),
            )
        })?
    } else {
        // Parse as form-urlencoded
        serde_urlencoded::from_str(&body).map_err(|e| {
            oauth2_error(
                "invalid_request",
                Some(format!("Invalid form-urlencoded request body: {}", e)),
            )
        })?
    };

    // Get project
    let project = db_projects::find_by_name(&state.db_pool, project_name)
        .await
        .map_err(|e| {
            error!("Database error: {:?}", e);
            oauth2_error("server_error", Some("Internal server error".to_string()))
        })?
        .ok_or_else(|| oauth2_error("invalid_request", Some("Project not found".to_string())))?;

    // Get OAuth extension
    let extension =
        db_extensions::find_by_project_and_name(&state.db_pool, project.id, extension_name)
            .await
            .map_err(|e| {
                error!("Database error: {:?}", e);
                oauth2_error("server_error", Some("Internal server error".to_string()))
            })?
            .ok_or_else(|| {
                oauth2_error(
                    "invalid_request",
                    Some("OAuth extension not configured".to_string()),
                )
            })?;

    // Verify extension type
    if extension.extension_type != "oauth" {
        return Err(oauth2_error(
            "invalid_request",
            Some(format!(
                "Extension '{}' is not an OAuth extension",
                extension_name
            )),
        ));
    }

    // Parse spec
    let spec: OAuthExtensionSpec = serde_json::from_value(extension.spec.clone()).map_err(|e| {
        error!("Invalid extension spec: {:?}", e);
        oauth2_error(
            "server_error",
            Some("Invalid extension configuration".to_string()),
        )
    })?;

    // Parse status to get Rise client credentials
    let status: OAuthExtensionStatus =
        serde_json::from_value(extension.status.clone()).map_err(|e| {
            error!("Invalid extension status: {:?}", e);
            oauth2_error("server_error", Some("Invalid extension status".to_string()))
        })?;

    // Validate client_id from status
    let rise_client_id = status.rise_client_id.as_ref().ok_or_else(|| {
        error!("Rise client ID not configured for extension");
        oauth2_error(
            "server_error",
            Some("OAuth extension not fully configured".to_string()),
        )
    })?;

    if &req.client_id != rise_client_id {
        return Err(oauth2_error(
            "invalid_client",
            Some("Invalid client_id".to_string()),
        ));
    }

    // Grant-specific authentication validation
    let has_client_secret = req.client_secret.is_some();
    let has_code_verifier = req.code_verifier.is_some();

    match req.grant_type.as_str() {
        // authorization_code grant: REQUIRE client_secret OR code_verifier (PKCE), or both.
        // Confidential clients may use PKCE as additional protection (recommended by OAuth 2.1).
        "authorization_code" if !has_client_secret && !has_code_verifier => {
            return Err(oauth2_error(
                "invalid_request",
                Some("Missing client authentication: provide client_secret, code_verifier (PKCE), or both".to_string()),
            ));
        }
        "authorization_code" => {
            // Valid: has client_secret, code_verifier, or both
        }
        "refresh_token" if has_code_verifier => {
            // refresh_token grant: REJECT code_verifier (PKCE is only for authorization_code grant)
            return Err(oauth2_error(
                "invalid_request",
                Some("code_verifier not supported for refresh_token grant (PKCE is only for authorization_code)".to_string()),
            ));
        }
        "refresh_token" => {
            // refresh_token grant: ALLOW client_secret (confidential) or no auth (public)
        }
        _ => {
            // Unknown grant type will be rejected later
        }
    }

    // If client_secret provided, validate it
    if let Some(ref client_secret) = req.client_secret {
        // Get stored Rise client secret from status (plaintext)
        let stored_secret = status.rise_client_secret.as_ref().ok_or_else(|| {
            error!("Rise client secret not configured for extension");
            oauth2_error(
                "server_error",
                Some("OAuth extension not fully configured".to_string()),
            )
        })?;

        // Constant-time comparison
        use subtle::ConstantTimeEq;
        let is_valid: bool = client_secret
            .as_bytes()
            .ct_eq(stored_secret.as_bytes())
            .into();
        if !is_valid {
            return Err(oauth2_error(
                "invalid_client",
                Some("Invalid client_secret".to_string()),
            ));
        }
    }

    // Route to grant-specific handlers
    match req.grant_type.as_str() {
        "authorization_code" => {
            // Returns Response directly (raw passthrough)
            handle_authorization_code_grant(state.clone(), project, extension_name.to_string(), req)
                .await
        }
        "refresh_token" => {
            // Returns Json<OAuth2TokenResponse>, convert to Response
            let json_response = handle_refresh_token_grant(
                state.clone(),
                project,
                extension_name.to_string(),
                spec,
                req,
            )
            .await?;

            use axum::body::Body;
            let response = Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&json_response.0).unwrap()))
                .unwrap();
            Ok(response)
        }
        _ => Err(oauth2_error(
            "unsupported_grant_type",
            Some(format!("Unsupported grant_type: {}", req.grant_type)),
        )),
    }
}

/// Handle authorization_code grant type
async fn handle_authorization_code_grant(
    state: AppState,
    project: crate::db::models::Project,
    extension_name: String,
    req: TokenRequest,
) -> Result<Response, (StatusCode, Json<OAuth2ErrorResponse>)> {
    // Validate required parameters
    let code = req.code.ok_or_else(|| {
        oauth2_error(
            "invalid_request",
            Some("Missing required parameter: code".to_string()),
        )
    })?;

    let code_state = oauth_transient_state::claim(
        &state.db_pool,
        &code,
        Uuid::new_v4(),
        oauth_transient_state::CLAIM_GRACE_TTL,
    )
    .await
    .map_err(|e| {
        error!("Failed to claim authorization code: {:?}", e);
        oauth2_error("server_error", Some("Internal server error".to_string()))
    })?
    .ok_or_else(|| {
        oauth2_error(
            "invalid_grant",
            Some("Invalid or expired authorization code".to_string()),
        )
    })?;

    let code_state: oauth_transient_state::ClaimedState<OAuthCodeState> = code_state;

    // Verify project and extension match
    if code_state.project_id != project.id || code_state.extension_name != extension_name {
        return Err(oauth2_error(
            "invalid_grant",
            Some("Authorization code mismatch".to_string()),
        ));
    }

    // RFC 6749 Section 4.1.3: Validate redirect_uri matches authorization request
    // If redirect_uri was included in authorization, it MUST be included here and match exactly
    if let Some(ref stored_redirect_uri) = code_state.redirect_uri {
        match &req.redirect_uri {
            Some(req_redirect_uri) if req_redirect_uri == stored_redirect_uri => {
                // Match - validation passed
            }
            Some(_) => {
                return Err(oauth2_error(
                    "invalid_grant",
                    Some("redirect_uri does not match authorization request".to_string()),
                ));
            }
            None => {
                return Err(oauth2_error(
                    "invalid_request",
                    Some("redirect_uri required (was provided during authorization)".to_string()),
                ));
            }
        }
    } else if req.redirect_uri.is_some() {
        // redirect_uri provided in token request but not in authorization request
        return Err(oauth2_error(
            "invalid_request",
            Some("redirect_uri was not provided during authorization".to_string()),
        ));
    }

    // CRITICAL: If code_verifier provided, challenge must have been provided during authz
    // This prevents bypassing authentication by providing code_verifier without prior code_challenge
    if req.code_verifier.is_some() && code_state.code_challenge.is_none() {
        return Err(oauth2_error(
            "invalid_request",
            Some("code_verifier requires prior code_challenge during authorization".to_string()),
        ));
    }

    // PKCE validation if code_challenge was provided during authorization
    if let Some(ref code_challenge) = code_state.code_challenge {
        // PKCE flow - require code_verifier
        let code_verifier = req.code_verifier.ok_or_else(|| {
            oauth2_error(
                "invalid_request",
                Some("Missing code_verifier for PKCE flow".to_string()),
            )
        })?;

        // RFC 7636: code_verifier must be 43-128 characters, unreserved charset [A-Za-z0-9-._~]
        if code_verifier.len() < 43 || code_verifier.len() > 128 {
            return Err(oauth2_error(
                "invalid_request",
                Some("code_verifier must be 43-128 characters".to_string()),
            ));
        }
        if !code_verifier
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c))
        {
            return Err(oauth2_error(
                "invalid_request",
                Some("code_verifier contains invalid characters".to_string()),
            ));
        }

        let code_challenge_method = code_state
            .code_challenge_method
            .as_deref()
            .unwrap_or("S256");

        if !validate_pkce(&code_verifier, code_challenge, code_challenge_method) {
            return Err(oauth2_error(
                "invalid_grant",
                Some("PKCE validation failed".to_string()),
            ));
        }

        debug!("PKCE validation successful");
    }

    // Note: For non-PKCE flows, client_secret was already validated in token_endpoint()

    // Decrypt raw token response from code_state (Rise acts as passthrough proxy)
    let encryption_provider = state.encryption_provider.as_ref().ok_or_else(|| {
        error!("Encryption provider not configured");
        oauth2_error("server_error", Some("Internal server error".to_string()))
    })?;

    let token_response_body = encryption_provider
        .decrypt(&code_state.token_response_encrypted)
        .await
        .map_err(|e| {
            error!("Failed to decrypt token response: {:?}", e);
            oauth2_error("server_error", Some("Internal server error".to_string()))
        })?;

    info!(
        "Authorization code grant successful for project {} extension {}",
        project.name, extension_name
    );

    // Return raw response with original status code and Content-Type (NO parsing)
    use axum::body::Body;
    let response = Response::builder()
        .status(StatusCode::from_u16(code_state.status_code).unwrap_or(StatusCode::OK))
        .header(header::CONTENT_TYPE, code_state.content_type.clone())
        .body(Body::from(token_response_body))
        .unwrap();

    if let Err(e) = code_state.finalize().await {
        warn!(
            "Failed to finalize authorization code (response already built, row will expire via TTL): {:?}",
            e
        );
    }
    Ok(response)
}

/// Handle refresh_token grant type
async fn handle_refresh_token_grant(
    state: AppState,
    project: crate::db::models::Project,
    extension_name: String,
    spec: OAuthExtensionSpec,
    req: TokenRequest,
) -> Result<Json<OAuth2TokenResponse>, (StatusCode, Json<OAuth2ErrorResponse>)> {
    // Validate required parameters
    let refresh_token = req.refresh_token.ok_or_else(|| {
        oauth2_error(
            "invalid_request",
            Some("Missing required parameter: refresh_token".to_string()),
        )
    })?;

    // Call OAuth provider's refresh_token method
    use super::provider::{OAuthProvider, OAuthProviderConfig};

    let oauth_provider = OAuthProvider::new(OAuthProviderConfig {
        db_pool: state.db_pool.clone(),
        encryption_provider: state.encryption_provider.clone().ok_or_else(|| {
            error!("Encryption provider not configured");
            oauth2_error("server_error", Some("Internal server error".to_string()))
        })?,
        http_client: crate::server::ssrf::safe_client(&state.server_settings.ssrf),
        api_domain: state.public_url.clone(),
    });

    // Get upstream OAuth client secret (prefers encrypted in spec, falls back to env var ref)
    let client_secret = oauth_provider
        .resolve_oauth_client_secret(project.id, &spec)
        .await
        .map_err(|e| {
            error!("Failed to resolve OAuth client secret: {:?}", e);
            oauth2_error("server_error", Some("Internal server error".to_string()))
        })?;

    // Refresh tokens with upstream provider
    let token_response = oauth_provider
        .refresh_token(&spec, &client_secret, &refresh_token)
        .await
        .map_err(|e| {
            error!("Failed to refresh token with upstream provider: {:?}", e);
            oauth2_error("invalid_grant", Some("Failed to refresh token".to_string()))
        })?;

    // Pass through expires_in (already optional)
    let expires_in = token_response.expires_in;

    // Build scope
    let scope = if spec.scopes.is_empty() {
        None
    } else {
        Some(spec.scopes.join(" "))
    };

    info!(
        "Refresh token grant successful for project {} extension {}",
        project.name, extension_name
    );

    Ok(Json(OAuth2TokenResponse {
        access_token: token_response.access_token,
        token_type: token_response.token_type,
        expires_in,
        refresh_token: token_response.refresh_token,
        scope,
        id_token: token_response.id_token,
    }))
}

/// CORS preflight handler for token endpoint
///
/// OPTIONS /oidc/{project}/{extension}/token
///
/// Validates the Origin header against project-specific allowed origins:
/// - localhost (any port) for local development
/// - Rise public URL
/// - Project deployment URLs (including custom domains)
pub async fn token_endpoint_options(
    State(state): State<AppState>,
    Path((project_name, _extension_name)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> Response {
    // Get Origin header
    let origin = match headers.get(header::ORIGIN).and_then(|h| h.to_str().ok()) {
        Some(o) => o,
        None => {
            // No Origin header - not a CORS request, return empty 204
            return StatusCode::NO_CONTENT.into_response();
        }
    };

    // Get project to validate origin
    let project = match db_projects::find_by_name(&state.db_pool, &project_name).await {
        Ok(Some(p)) => p,
        _ => {
            // Project not found - reject CORS
            return StatusCode::FORBIDDEN.into_response();
        }
    };

    // Validate origin against project's allowed origins
    match validate_cors_origin(
        &state.db_pool,
        origin,
        &project,
        &state.public_url,
        &state.deployment_backend,
    )
    .await
    {
        Some(allowed_origin) => {
            // Origin is allowed - return CORS headers
            let cors = cors_headers(&allowed_origin);
            (StatusCode::NO_CONTENT, cors).into_response()
        }
        None => {
            // Origin not allowed
            debug!(
                "CORS origin '{}' not allowed for project '{}'",
                origin, project_name
            );
            StatusCode::FORBIDDEN.into_response()
        }
    }
}

/// Proxy OIDC discovery document from upstream provider
///
/// GET /oidc/{project}/{extension}/.well-known/openid-configuration
///
/// OAuth 2.0 Provider Support:
/// This endpoint supports both OIDC-compliant providers (Google, Dex, etc.) and
/// plain OAuth 2.0 providers (GitHub, etc.) that don't provide OIDC discovery.
///
/// When both authorization_endpoint and token_endpoint are manually specified,
/// we synthesize a minimal OIDC discovery document from the spec, allowing
/// non-OIDC providers to work seamlessly.
///
/// Returns the OIDC discovery document with URLs rewritten to point to Rise's OIDC proxy:
/// - issuer -> {RISE_PUBLIC_URL}/oidc/{project}/{extension}
/// - authorization_endpoint -> {RISE_PUBLIC_URL}/oidc/{project}/{extension}/authorize
/// - token_endpoint -> {RISE_PUBLIC_URL}/oidc/{project}/{extension}/token
/// - jwks_uri -> {RISE_PUBLIC_URL}/oidc/{project}/{extension}/jwks (only for OIDC providers)
pub async fn oidc_discovery(
    State(state): State<AppState>,
    Path((project_name, extension_name)): Path<(String, String)>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    debug!(
        "OIDC discovery request for project={}, extension={}",
        project_name, extension_name
    );

    // Get project
    let project = db_projects::find_by_name(&state.db_pool, &project_name)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Project not found".to_string()))?;

    // Get OAuth extension
    let extension =
        db_extensions::find_by_project_and_name(&state.db_pool, project.id, &extension_name)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .ok_or((
                StatusCode::NOT_FOUND,
                "OAuth extension not configured".to_string(),
            ))?;

    // Verify extension type is oauth
    if extension.extension_type != "oauth" {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "Extension '{}' is not an OAuth extension (type: {})",
                extension_name, extension.extension_type
            ),
        ));
    }

    // Parse spec
    let spec: OAuthExtensionSpec = serde_json::from_value(extension.spec.clone()).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid spec: {}", e),
        )
    })?;

    // Build Rise OIDC base URL
    let rise_oidc_base = format!(
        "{}/oidc/{}/{}",
        state.public_url.trim_end_matches('/'),
        project_name,
        extension_name
    );

    // If both endpoints are in spec, synthesize discovery document immediately
    // This supports plain OAuth 2.0 providers (e.g., GitHub) that don't have OIDC discovery
    if spec.authorization_endpoint.is_some() && spec.token_endpoint.is_some() {
        debug!(
            "Both authorization_endpoint and token_endpoint in spec - synthesizing discovery document for {}/{}",
            project_name, extension_name
        );

        let discovery = serde_json::json!({
            "issuer": rise_oidc_base,
            "authorization_endpoint": format!("{}/authorize", rise_oidc_base),
            "token_endpoint": format!("{}/token", rise_oidc_base),
            "response_types_supported": ["code"],
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "code_challenge_methods_supported": ["S256"],
            "token_endpoint_auth_methods_supported": ["client_secret_post", "none"]
        });

        info!(
            "Returning synthesized OIDC discovery for {}/{} (non-OIDC OAuth 2.0 provider)",
            project_name, extension_name
        );

        return Ok((
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            Json(discovery),
        ));
    }

    // Try to fetch upstream OIDC discovery
    let ssrf_config = &state.server_settings.ssrf;
    let upstream_issuer = &spec.issuer_url;
    let discovery_result = fetch_oidc_discovery(upstream_issuer, ssrf_config).await;

    match discovery_result {
        Ok(upstream_discovery) => {
            // Successfully fetched upstream discovery - rewrite URLs
            debug!(
                "Fetched upstream OIDC discovery for {}/{}",
                project_name, extension_name
            );

            let mut discovery = serde_json::json!({
                "issuer": rise_oidc_base,
                "authorization_endpoint": format!("{}/authorize", rise_oidc_base),
                "token_endpoint": format!("{}/token", rise_oidc_base),
                "jwks_uri": format!("{}/jwks", rise_oidc_base),
            });

            // Copy other fields from upstream discovery
            if let Ok(upstream_json) = serde_json::to_value(&upstream_discovery) {
                if let Some(upstream_obj) = upstream_json.as_object() {
                    if let Some(discovery_obj) = discovery.as_object_mut() {
                        for (key, value) in upstream_obj {
                            // Skip fields we're overriding
                            if key != "issuer"
                                && key != "authorization_endpoint"
                                && key != "token_endpoint"
                                && key != "jwks_uri"
                            {
                                discovery_obj.insert(key.clone(), value.clone());
                            }
                        }

                        // Override code_challenge_methods_supported to only advertise S256,
                        // regardless of what the upstream provider supports.
                        discovery_obj.insert(
                            "code_challenge_methods_supported".to_string(),
                            serde_json::json!(["S256"]),
                        );
                    }
                }
            }

            info!(
                "Returning OIDC discovery for {}/{} with Rise OIDC base: {}",
                project_name, extension_name, rise_oidc_base
            );

            Ok((
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                Json(discovery),
            ))
        }
        Err(e) => {
            // OIDC discovery failed - try to synthesize from spec if authorization_endpoint exists
            debug!(
                "OIDC discovery failed for {}/{}: {}",
                project_name, extension_name, e
            );

            if spec.authorization_endpoint.is_some() {
                // Synthesize minimal discovery document from spec
                warn!(
                    "OIDC discovery failed for {}/{} - synthesizing from spec (fallback for non-OIDC provider)",
                    project_name, extension_name
                );

                let discovery = serde_json::json!({
                    "issuer": rise_oidc_base,
                    "authorization_endpoint": format!("{}/authorize", rise_oidc_base),
                    "token_endpoint": format!("{}/token", rise_oidc_base),
                    "response_types_supported": ["code"],
                    "grant_types_supported": ["authorization_code", "refresh_token"],
                    "code_challenge_methods_supported": ["S256"],
                    "token_endpoint_auth_methods_supported": ["client_secret_post", "none"]
                });

                info!(
                    "Returning synthesized OIDC discovery for {}/{} (fallback)",
                    project_name, extension_name
                );

                Ok((
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    Json(discovery),
                ))
            } else {
                // No authorization_endpoint in spec and OIDC discovery failed
                error!(
                    "OIDC discovery failed and no authorization_endpoint in spec for {}/{}",
                    project_name, extension_name
                );
                Err((
                    StatusCode::BAD_GATEWAY,
                    format!(
                        "OIDC discovery failed and no authorization_endpoint configured: {}",
                        e
                    ),
                ))
            }
        }
    }
}

/// Proxy JWKS from upstream provider
///
/// GET /oidc/{project}/{extension}/jwks
///
/// Fetches the JWKS from the upstream OAuth provider and returns it.
/// For non-OIDC providers (e.g., GitHub) that don't support OIDC discovery,
/// returns 501 Not Implemented.
pub async fn oidc_jwks(
    State(state): State<AppState>,
    Path((project_name, extension_name)): Path<(String, String)>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    debug!(
        "OIDC JWKS request for project={}, extension={}",
        project_name, extension_name
    );

    // Get project
    let project = db_projects::find_by_name(&state.db_pool, &project_name)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Project not found".to_string()))?;

    // Get OAuth extension
    let extension =
        db_extensions::find_by_project_and_name(&state.db_pool, project.id, &extension_name)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .ok_or((
                StatusCode::NOT_FOUND,
                "OAuth extension not configured".to_string(),
            ))?;

    // Verify extension type is oauth
    if extension.extension_type != "oauth" {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "Extension '{}' is not an OAuth extension (type: {})",
                extension_name, extension.extension_type
            ),
        ));
    }

    // Parse spec
    let spec: OAuthExtensionSpec = serde_json::from_value(extension.spec.clone()).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid spec: {}", e),
        )
    })?;

    // Try to fetch OIDC discovery to get jwks_uri
    let ssrf_config = &state.server_settings.ssrf;
    let discovery_result = fetch_oidc_discovery(&spec.issuer_url, ssrf_config).await;

    match discovery_result {
        Ok(discovery) => {
            // Get jwks_uri from discovery
            let jwks_uri = discovery.jwks_uri.ok_or((
                StatusCode::INTERNAL_SERVER_ERROR,
                "No jwks_uri in OIDC discovery".to_string(),
            ))?;

            // SSRF-validate the JWKS URI before fetching
            crate::server::ssrf::validate_url(&jwks_uri, ssrf_config)
                .await
                .map_err(|e| {
                    error!("JWKS URI failed SSRF validation: {}", e);
                    (
                        StatusCode::BAD_REQUEST,
                        format!("JWKS URI failed SSRF validation: {}", e),
                    )
                })?;

            let http_client = crate::server::ssrf::safe_client(ssrf_config);

            // Fetch JWKS
            let jwks_response = http_client.get(&jwks_uri).send().await.map_err(|e| {
                error!("Failed to fetch JWKS from {}: {:?}", jwks_uri, e);
                (
                    StatusCode::BAD_GATEWAY,
                    format!("Failed to fetch JWKS: {}", e),
                )
            })?;

            if !jwks_response.status().is_success() {
                let status = jwks_response.status();
                return Err((
                    StatusCode::BAD_GATEWAY,
                    format!("Upstream JWKS fetch failed: {}", status),
                ));
            }

            let jwks: serde_json::Value = jwks_response.json().await.map_err(|e| {
                error!("Failed to parse JWKS response: {:?}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to parse JWKS".to_string(),
                )
            })?;

            info!(
                "Returning JWKS for {}/{} from upstream: {}",
                project_name, extension_name, jwks_uri
            );

            Ok((
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                Json(jwks),
            ))
        }
        Err(e) => {
            // For non-OIDC providers, JWKS is not available
            warn!(
                "JWKS not available for {}/{}: upstream OIDC discovery failed: {}",
                project_name, extension_name, e
            );
            Err((
                StatusCode::NOT_IMPLEMENTED,
                "JWKS endpoint not available: upstream provider does not support OIDC discovery. \
                JWKS is only available for OIDC-compliant providers (e.g., Google, Dex). \
                Plain OAuth 2.0 providers (e.g., GitHub) do not provide public keys via JWKS."
                    .to_string(),
            ))
        }
    }
}
