use crate::db::{projects, users};
use crate::server::auth::session_scope::MaybeSessionScope;
use crate::server::auth::{
    browser_binding::BrowserProof,
    cookie_helpers,
    token_storage::{
        generate_code_challenge, generate_code_verifier, generate_state_token,
        CompletedAuthSession, OAuth2State,
    },
    user_identity::{LoginError, LoginProfile, ResolvedIdentity, TokenStanding},
};
use crate::server::frontend::load_auth_template;
use crate::server::middleware::url_for_log;
use crate::server::state::AppState;
use axum::{
    extract::{Query, State},
    http::{header, uri::Uri, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    Json,
};
use rise_backend_auth::session_scope::AccessRequest;
use rise_backend_auth::{SessionClient, SessionUser};
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

/// Resolve a sign-in target before its name contributes to URLs or trust lists.
async fn resolve_signin_project(
    pool: &sqlx::PgPool,
    project_name: Option<&str>,
) -> Result<Option<String>, (StatusCode, String)> {
    let Some(name) = project_name else {
        return Ok(None);
    };
    let project = projects::find_by_name(pool, name)
        .await
        .map_err(|error| {
            tracing::error!(?error, "Failed to resolve sign-in project");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to resolve project".to_string(),
            )
        })?
        .filter(|project| project.status != crate::db::models::ProjectStatus::Deleting)
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Project not found".to_string()))?;
    Ok(Some(project.name))
}

/// Resolve the set of hostnames a post-login `redirect` may target for a given
/// project: the project's canonical ingress host plus every active deployment's
/// URL (default host + custom domains).
///
/// In the standard project-subdomain layout the app host (e.g.
/// `secret.example.com`, from `production_ingress_url_template =
/// {project_name}.example.com`) is a **sibling** of the control-plane host
/// (`rise.example.com`), not a subdomain of it — so the post-login deep-link
/// `redirect` would otherwise fail `validate_redirect_url` (which only trusts
/// `public_url`'s host and its subdomains) and drop to `/`. Scoping the trust to
/// *this project's own* resolved hosts lets the deep-link through **without**
/// trusting every sibling under the shared parent domain (which a wildcard
/// suffix would — a cross-project open-redirect / info-disclosure risk in a
/// multi-tenant deployment).
///
/// Hosts are returned bare (no scheme/port) and lowercased. Lookups are
/// best-effort: the canonical host is always included, so the common deep-link
/// keeps working even if the deployment lookups fail.
async fn project_redirect_hosts(state: &AppState, project_name: &str) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    for url in project_app_urls(state, project_name).await {
        let host = url::Url::parse(&url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_lowercase));
        if let Some(host) = host {
            if !hosts.contains(&host) {
                hosts.push(host);
            }
        }
    }
    hosts
}

/// Whether a token's `aud` names the app at `candidate`: same host and
/// explicit port (every app owns its host). The scheme is ignored, as one app
/// may be reached over either.
fn audience_names_app(aud: &str, candidate: &str) -> bool {
    fn key(url: &str) -> Option<(String, Option<u16>)> {
        let url = url::Url::parse(url).ok()?;
        Some((url.host_str()?.to_lowercase(), url.port()))
    }
    match (key(aud), key(candidate)) {
        (Some(aud), Some(candidate)) => aud == candidate,
        _ => false,
    }
}

/// Whether a token whose audience is `aud` may open `project_name`'s app.
///
/// Sign-in mints an app token for the URL the visitor came through (the
/// project's template URL, a custom domain, or another deployment group's
/// host) and the app receives that cookie. Without this check, the owner of one
/// app could replay a visitor's token against another app the visitor can
/// reach. A token for Rise's own URL is a web UI session, or an app token from
/// an install without per-project URLs, where every app shares Rise's host and
/// cookie anyway.
async fn audience_admits_project(state: &AppState, project_name: &str, aud: &str) -> bool {
    // The cheap candidates cover the common case on every app request.
    if audience_names_app(aud, &state.public_url)
        || build_project_url(state, project_name).is_some_and(|url| audience_names_app(aud, &url))
    {
        return true;
    }
    project_app_urls(state, project_name)
        .await
        .iter()
        .any(|url| audience_names_app(aud, url))
}

/// The base URLs `project_name`'s app answers on: its template URL plus every
/// active deployment's URL (default host + custom domains), across groups.
/// Lookups are best-effort; a failed one leaves its URLs out.
async fn project_app_urls(state: &AppState, project_name: &str) -> Vec<String> {
    let mut urls = Vec::new();
    if let Ok(Some(project)) = projects::find_by_name(&state.db_pool, project_name).await {
        urls.extend(build_project_url(state, &project.name));
        if let Ok(deployments) =
            crate::db::deployments::get_active_deployments_for_project(&state.db_pool, project.id)
                .await
        {
            for deployment in &deployments {
                if let Ok(deployment_urls) = state
                    .deployment_backend
                    .get_deployment_urls(deployment, &project)
                    .await
                {
                    urls.push(deployment_urls.default_url);
                    urls.extend(deployment_urls.custom_domain_urls);
                }
            }
        }
    }
    urls
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
/// * `allowed_hosts` - This project's own resolved hosts (bare, lowercased; see
///   [`project_redirect_hosts`]). App hosts are *siblings* of the control-plane
///   host (`{project}.example.com` vs `rise.example.com`), so they match neither
///   the public host nor its subdomains; the redirect host must **exactly equal**
///   one of these to be accepted. Scoping to the specific project's hosts (rather
///   than the whole ingress parent domain) prevents a cross-project open redirect.
///
/// # Returns
/// A safe redirect URL, or "/" if the input is invalid
///
/// # Security
/// - Relative paths are allowed only in the strict form
///   [`crate::server::auth::redirect::is_safe_relative_path`] accepts
/// - Backslashes, whitespace, and control characters are never allowed
/// - Absolute URLs must be HTTPS (or HTTP for localhost/development)
/// - Absolute URLs must match the Rise public domain or one of `allowed_hosts`
/// - Host matching is exact (case-insensitive), never a string prefix, so
///   lookalikes like `secret.example.com.evil.com` are rejected
/// - All dangerous schemes (javascript:, data:, vbscript:, etc.) are blocked
/// - Invalid or suspicious URLs default to "/"
fn validate_redirect_url(redirect_url: &str, public_url: &str, allowed_hosts: &[String]) -> String {
    const SAFE_FALLBACK: &str = "/";

    // Empty or whitespace-only URLs default to safe fallback
    let redirect_url = redirect_url.trim();
    if redirect_url.is_empty() {
        return SAFE_FALLBACK.to_string();
    }

    // A relative path stays on the current host only in its strict form; see
    // `is_safe_relative_path` for the shapes browsers resolve off-site.
    if redirect_url.starts_with('/') || redirect_url.starts_with('\\') {
        if crate::server::auth::redirect::is_safe_relative_path(redirect_url) {
            return redirect_url.to_string();
        }
        tracing::warn!(
            redirect_url = %url_for_log(redirect_url),
            "Blocked relative redirect that a browser could resolve to another host"
        );
        return SAFE_FALLBACK.to_string();
    }

    // The URL parser drops tabs and newlines and reads `\` as `/`, as browsers
    // do, but the raw string is what gets returned. Refuse any URL carrying
    // those characters so the checked URL and the returned one cannot differ.
    if redirect_url
        .chars()
        .any(|c| c == '\\' || c.is_whitespace() || c.is_control())
    {
        tracing::warn!(
            redirect_url = %url_for_log(redirect_url),
            "Blocked redirect URL containing backslashes, whitespace, or control characters"
        );
        return SAFE_FALLBACK.to_string();
    }

    // Try to parse as absolute URL
    let parsed_redirect = match url::Url::parse(redirect_url) {
        Ok(url) => url,
        Err(e) => {
            tracing::warn!(
                redirect_url = %url_for_log(redirect_url),
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
            redirect_url = %url_for_log(redirect_url),
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
                redirect_url = %url_for_log(redirect_url),
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

    // Allow the public host and its dot-anchored subdomains.
    if crate::server::auth::redirect::host_is_same_or_subdomain(redirect_host, public_host) {
        return redirect_url.to_string();
    }

    // Allow redirects to one of this project's own resolved hosts. In the
    // standard project-subdomain layout app hosts are siblings of the
    // control-plane host (e.g. `secret.example.com` vs `rise.example.com`), so
    // they match neither check above. Matching is EXACT host equality
    // (case-insensitive) against the project's canonical + deployment hosts —
    // never a string prefix and never the whole parent domain — so a sibling
    // project's host or a lookalike (`secret.example.com.evil.com`) is rejected.
    if allowed_hosts.iter().any(|allowed_host| {
        // This policy requires exact host equality, so check the shared
        // same-or-subdomain primitive in both directions.
        crate::server::auth::redirect::host_is_same_or_subdomain(redirect_host, allowed_host)
            && crate::server::auth::redirect::host_is_same_or_subdomain(allowed_host, redirect_host)
    }) {
        return redirect_url.to_string();
    }

    // Allow loopback redirects for development only when Rise itself is local.
    if crate::server::auth::redirect::is_loopback_host(redirect_host)
        && crate::server::auth::redirect::is_loopback_host(public_host)
    {
        return redirect_url.to_string();
    }

    // All other external URLs are blocked
    tracing::warn!(
        redirect_url = %url_for_log(redirect_url),
        redirect_host = %redirect_host,
        public_host = %public_host,
        "Blocked redirect to untrusted external domain"
    );

    SAFE_FALLBACK.to_string()
}

/// Mirror the login's IdP groups onto `user_id`'s IdP-managed teams.
///
/// Admin, operator, and platform access granted by IdP group are read from
/// those teams, so a login whose groups cannot be applied must not go on to
/// issue a session: the teams would still hold whatever the IdP last
/// asserted, including groups the user has since left. Every failure here is
/// returned for the caller to refuse the login. Missing/null assertions follow
/// the configured policy; an explicit empty claim removes mirrored memberships.
async fn sync_login_groups(
    pool: &sqlx::PgPool,
    user_id: uuid::Uuid,
    claims: &serde_json::Value,
    group_claim: &str,
    allowed_prefixes: &[String],
    missing_claim_policy: crate::server::settings::MissingGroupClaimPolicy,
) -> anyhow::Result<()> {
    use anyhow::Context as _;

    let claims =
        crate::server::auth::jwt::Claims::from_value_with_group_claim(claims.clone(), group_claim)
            .context("Failed to parse ID token claims for group sync")?;
    let groups = match claims.groups {
        Some(groups) => groups,
        None => match missing_claim_policy {
            crate::server::settings::MissingGroupClaimPolicy::RefuseLogin => {
                anyhow::bail!("ID token is missing the configured group claim")
            }
            crate::server::settings::MissingGroupClaimPolicy::TreatAsEmpty => Vec::new(),
        },
    };
    tracing::debug!(
        "Syncing {} IdP groups for user {} during login",
        groups.len(),
        user_id
    );
    crate::server::auth::group_sync::sync_user_groups(pool, user_id, &groups, allowed_prefixes)
        .await
}

/// Why a validated upstream login did not produce a Rise session.
#[derive(Debug)]
enum LoginFailure {
    /// The ID token has no `email`, which the typed APIs key users on.
    MissingEmail,
    /// The IdP does not vouch for the `email`: see [`verified_login_email`].
    UnverifiedEmail,
    /// The exact identity mapping, or its User, is inactive (ADR-0001 §1).
    Disabled,
    /// The upstream `sub` cannot be recorded as a `UserIdentity` subject.
    InvalidSubject(String),
    /// The login's IdP groups could not be applied; already logged. See
    /// [`sync_login_groups`].
    GroupSync,
    /// A database or signing failure; already logged.
    Internal,
}

impl LoginFailure {
    /// The status and message a login endpoint answers with.
    fn response(&self) -> (StatusCode, String) {
        match self {
            Self::MissingEmail => (StatusCode::BAD_REQUEST, "Email claim missing".to_string()),
            Self::UnverifiedEmail => (
                StatusCode::FORBIDDEN,
                "Your identity provider has not verified your email address.".to_string(),
            ),
            Self::Disabled => (
                StatusCode::FORBIDDEN,
                "This account is disabled. Contact your Rise administrator.".to_string(),
            ),
            Self::InvalidSubject(reason) => (StatusCode::UNAUTHORIZED, reason.clone()),
            Self::GroupSync => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Could not refresh your group memberships from the identity provider. \
                 Please try again."
                    .to_string(),
            ),
            Self::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to process user".to_string(),
            ),
        }
    }
}

/// The two identities a validated login resolves to.
struct ResolvedLogin {
    /// The typed-API user, found or created by email.
    user: crate::db::User,
    /// The live `User` resource and the `UserIdentity` the login came
    /// through, found by exact `(issuer, subject)` or provisioned on first
    /// sight (ADR-0001 §1).
    identity: ResolvedIdentity,
}

/// The ID token's `email`, provided the IdP vouches for it.
///
/// Users, `auth.admin_users`, `auth.operator_users` and the platform-access
/// allowlist all match on this address, so a self-asserted one would let its
/// holder claim another person's account and roles. `email_verified` must be
/// `true` (a JSON boolean, or the string `"true"` some IdPs send). An absent
/// claim is accepted only when `auth.allow_missing_email_verified` says the IdP
/// verifies every address it issues; `email_verified: false` never is.
fn verified_login_email(
    claims: &serde_json::Value,
    allow_missing_email_verified: bool,
) -> Result<&str, LoginFailure> {
    let email = claims
        .get("email")
        .and_then(|v| v.as_str())
        .ok_or(LoginFailure::MissingEmail)?;
    let verified = match claims.get("email_verified") {
        Some(serde_json::Value::Bool(verified)) => *verified,
        Some(serde_json::Value::String(verified)) => verified.eq_ignore_ascii_case("true"),
        Some(_) => false,
        None => allow_missing_email_verified,
    };
    if !verified {
        tracing::warn!(
            email_verified = ?claims.get("email_verified"),
            "Login refused: the ID token's email is not verified"
        );
        return Err(LoginFailure::UnverifiedEmail);
    }
    Ok(email)
}

/// Resolve an ID token already validated against `auth.issuer` to its typed
/// user and its `User` resource.
///
/// An inactive mapping or User fails here, before anything is issued — the
/// ingress flow included, since a disabled User may not log in anywhere.
async fn resolve_login(
    state: &AppState,
    claims: &serde_json::Value,
) -> Result<ResolvedLogin, LoginFailure> {
    let email = verified_login_email(claims, state.auth_settings.allow_missing_email_verified)?;
    // `JwtValidator` verified the signature and `iss`; `sub` is required there
    // too, so an absent one is a malformed token rather than an unknown user.
    let subject = claims
        .get("sub")
        .and_then(|v| v.as_str())
        .ok_or_else(|| LoginFailure::InvalidSubject("Subject claim missing".to_string()))?;

    let profile = LoginProfile {
        email: Some(email.to_string()),
        display_name: claims
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    };
    let identity = state
        .user_logins
        .resolve_or_provision(subject, &profile)
        .await
        .map_err(|error| match error {
            LoginError::Inactive => {
                tracing::warn!("Login refused: the identity or its User is inactive");
                LoginFailure::Disabled
            }
            LoginError::InvalidSubject(reason) => {
                tracing::warn!("Login refused: {reason}");
                LoginFailure::InvalidSubject("Invalid subject claim".to_string())
            }
            LoginError::Store(error) => {
                tracing::error!("Failed to resolve the login's User: {error:?}");
                LoginFailure::Internal
            }
        })?;

    // Paired with a default-Org membership in one transaction.
    let user = users::find_or_create_with_default_organization(
        &state.db_pool,
        email,
        state.default_organization_uid,
    )
    .await
    .map_err(|e| {
        tracing::error!("Failed to find/create user: {:#}", e);
        LoginFailure::Internal
    })?;

    if state.auth_settings.idp_group_sync_enabled {
        sync_login_groups(
            &state.db_pool,
            user.id,
            claims,
            &state.auth_settings.idp_group_claim,
            &state.auth_settings.idp_group_sync_prefixes,
            state.auth_settings.idp_group_missing_claim_policy,
        )
        .await
        .map_err(|e| {
            tracing::error!(
                user_id = %user.id,
                "Login refused: failed to sync IdP groups: {e:?}"
            );
            LoginFailure::GroupSync
        })?;
    }

    // A cross-reference only: nothing authenticates through it, so losing a
    // race for it must not fail the login.
    let resource_uid = identity.principal.uid;
    if let Err(e) = users::link_resource_user(&state.db_pool, user.id, resource_uid).await {
        tracing::warn!(
            user_id = %user.id,
            resource_user_uid = %resource_uid,
            "Failed to link the typed user to its User resource: {:?}",
            e
        );
    }

    Ok(ResolvedLogin { user, identity })
}

/// Issue the Rise session token for a resolved login, to `client`.
async fn issue_session(
    state: &AppState,
    claims: &serde_json::Value,
    login: &ResolvedLogin,
    client: SessionClient,
    authorization_details: Option<Vec<serde_json::Value>>,
) -> Result<String, LoginFailure> {
    // Resolve the user's team memberships for the groups claim; on a DB error,
    // fall back to no groups rather than failing the login.
    let groups = crate::db::teams::get_team_names_for_user(&state.db_pool, login.user.id)
        .await
        .ok();
    let session_user = SessionUser {
        subject: login.identity.principal.subject().to_string(),
        rise_uid: login.identity.principal.uid,
        identity_uid: login.identity.identity_uid,
        client,
        authorization_details,
    };
    state
        .jwt_signer
        .sign_user_jwt(claims, &session_user, groups, &state.public_url, None)
        .map_err(|e| {
            tracing::error!("Failed to sign Rise JWT: {:#}", e);
            LoginFailure::Internal
        })
}

#[derive(Debug, Deserialize)]
pub struct CodeExchangeRequest {
    pub code: String,
    pub code_verifier: String,
    pub redirect_uri: String,
    /// The access the session is restricted to (ADR-0006). Omitted means
    /// full access.
    #[serde(default)]
    pub access: Option<AccessRequest>,
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
    /// For device flow: a name for the requesting machine (e.g. its hostname),
    /// shown on the confirmation page
    #[serde(default)]
    pub client_name: Option<String>,
    /// Flow type: "code" for authorization code flow, "device" for device flow
    pub flow: String,
    /// For device flow: the access the login asks for (ADR-0006). Omitted
    /// means full access. The approver may change it.
    #[serde(default)]
    pub access: Option<AccessRequest>,
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

/// Rate-limit bucket shared by every device-login start.
const DEVICE_START_RATE_LIMIT_BUCKET: &str = "auth-device-start";

/// Build OAuth2 authorization URL or initiate device flow (for CLI)
///
/// The device flow is served by Rise itself (see [`super::device`]): the user
/// confirms the code on Rise's `/device` page, not at the IdP.
#[instrument(skip(state, headers))]
pub async fn authorize(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<AuthorizeRequest>,
) -> Result<Json<AuthorizeResponse>, (StatusCode, String)> {
    match payload.flow.as_str() {
        "code" => {
            if payload.access.is_some() {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "For the code flow, send `access` with the code exchange".to_string(),
                ));
            }
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
            let client_ip = crate::server::rate_limit::extract_client_ip(&headers);
            // Unauthenticated, and each start stores a row until it expires.
            state
                .oauth_rate_limiter
                .increment_and_check(&client_ip, None, DEVICE_START_RATE_LIMIT_BUCKET)
                .await
                .map_err(|retry_after| {
                    (
                        StatusCode::TOO_MANY_REQUESTS,
                        format!("Too many device logins started; retry in {retry_after}s"),
                    )
                })?;
            let client_ip = (client_ip != "unknown").then_some(client_ip);
            // Only the shape is checked here: the endpoint is unauthenticated,
            // so whether the named projects exist is the approver's to see.
            if let Some(access) = &payload.access {
                access.validate().map_err(|e| {
                    (
                        StatusCode::BAD_REQUEST,
                        format!("Invalid access request: {e}"),
                    )
                })?;
            }
            let started = super::device::DeviceFlow::new(&state)
                .start(
                    payload.client_name.as_deref(),
                    client_ip.as_deref(),
                    payload.access.as_ref(),
                )
                .await
                .map_err(|e| {
                    tracing::error!("Failed to start device flow: {:?}", e);
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Failed to start device flow".to_string(),
                    )
                })?;

            Ok(Json(AuthorizeResponse {
                authorization_url: None,
                device_code: Some(started.device_code),
                user_code: Some(started.user_code),
                verification_uri: Some(started.verification_uri),
                verification_uri_complete: Some(started.verification_uri_complete),
                expires_in: Some(started.expires_in),
                interval: Some(started.interval),
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

    // A malformed access request fails before the single-use code is spent.
    if let Some(access) = &payload.access {
        access.validate().map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("Invalid access request: {e}"),
            )
        })?;
    }

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

    crate::token_trace::trace_token("IdP ID token", &token_info.id_token);

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

    let login = resolve_login(&state, &claims)
        .await
        .map_err(|failure| failure.response())?;
    let authorization_details = match &payload.access {
        Some(access) => super::session_scope::compile_grant(&state, &login.user, access)
            .await
            .map_err(|e| (e.status, e.message))?,
        None => None,
    };
    let rise_jwt = issue_session(
        &state,
        &claims,
        &login,
        SessionClient::Cli,
        authorization_details,
    )
    .await
    .map_err(|failure| failure.response())?;
    let user = login.user;

    tracing::info!(
        "CLI login successful for user {} - issued Rise JWT",
        user.email
    );

    Ok(Json(LoginResponse { token: rise_jwt }))
}

#[derive(Debug, Serialize)]
pub struct MeResponse {
    pub id: String,
    pub email: String,
    pub is_admin: bool,
    pub is_operator: bool,
    pub can_create_teams: bool,
    /// What this session may do: `{"kind": "full"}`, or the grants of a
    /// scoped CLI session (ADR-0006).
    pub access: AccessRequest,
}

/// Get current user info from auth middleware
#[instrument(skip(state, auth))]
pub async fn me(
    State(state): State<AppState>,
    auth: crate::server::auth::context::AuthContext,
    MaybeSessionScope(scope): MaybeSessionScope,
) -> Result<Json<MeResponse>, (StatusCode, String)> {
    let user = auth.user().map_err(|e| (e.status, e.message))?;
    // User is injected by auth middleware
    tracing::debug!("GET /me: user_id={}, email={}", user.id, user.email);
    let is_admin = state.is_admin(user).await;
    let is_operator = state.is_operator(user).await;
    let can_create_teams = is_admin || state.auth_settings.allow_team_creation;
    Ok(Json(MeResponse {
        id: user.id.to_string(),
        email: user.email.clone(),
        is_admin,
        is_operator,
        can_create_teams,
        access: scope.map_or(AccessRequest::Full, |scope| scope.access()),
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
    Query(mut params): Query<SigninQuery>,
) -> Result<Response, (StatusCode, String)> {
    params.project = resolve_signin_project(&state.db_pool, params.project.as_deref()).await?;
    let project_name = params.project.as_deref().unwrap_or("Unknown");
    let raw_redirect_url = params
        .redirect
        .as_ref()
        .or(params.rd.as_ref())
        .cloned()
        .unwrap_or_else(|| "/".to_string());

    // Validate and sanitize the redirect URL to prevent open redirects. The
    // redirect is scoped to this project's own hosts (not the parent domain).
    let allowed_hosts = match params.project.as_deref() {
        Some(p) => project_redirect_hosts(&state, p).await,
        None => Vec::new(),
    };
    let redirect_url = validate_redirect_url(&raw_redirect_url, &state.public_url, &allowed_hosts);

    tracing::info!(
        project = %project_name,
        has_redirect = !redirect_url.is_empty(),
        raw_redirect = %url_for_log(&raw_redirect_url),
        validated_redirect = %url_for_log(&redirect_url),
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
/// The scheme is the configured `ingress_schema`, not `X-Forwarded-Proto`: behind
/// a load balancer that terminates TLS, the proxy in front of Rise only sees
/// plain HTTP and reports `http` for a request the browser made over `https`.
///
/// Falls back to the configured public_url if no valid host header is present.
fn extract_request_base_url(headers: &HeaderMap, state: &AppState) -> String {
    if let Some(host) = headers.get("host") {
        if let Ok(host_str) = host.to_str() {
            return format!("{}://{}", state.ingress_schema, host_str);
        }
    }

    // Fallback to configured public URL
    state.public_url.trim_end_matches('/').to_string()
}

/// Whether a browser request reached the exact configured origin.
fn request_matches_origin(headers: &HeaderMap, expected_url: &str) -> bool {
    let Ok(expected) = url::Url::parse(expected_url) else {
        return false;
    };
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let Ok(actual) = url::Url::parse(&format!("{}://{}", expected.scheme(), host)) else {
        return false;
    };
    actual.username().is_empty()
        && actual.password().is_none()
        && actual.path() == "/"
        && actual.query().is_none()
        && actual.fragment().is_none()
        && actual.origin() == expected.origin()
}

fn with_binding_cookie(
    mut response: Response,
    cookie: &str,
) -> Result<Response, (StatusCode, String)> {
    let value = HeaderValue::from_str(cookie).map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to set sign-in cookie".into(),
        )
    })?;
    response.headers_mut().append(header::SET_COOKIE, value);
    Ok(response)
}

fn login_storage_error(error: anyhow::Error) -> (StatusCode, String) {
    tracing::error!(?error, "Sign-in transaction storage failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "Failed to process sign-in transaction".into(),
    )
}

fn invalid_browser_binding() -> (StatusCode, String) {
    (
        StatusCode::BAD_REQUEST,
        "Sign-in must finish in the browser that started it. Please start again.".into(),
    )
}

/// Initiate browser login on its UI or app origin.
#[instrument(skip(state, params, headers, uri))]
pub async fn oauth_signin_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Query(mut params): Query<SigninQuery>,
) -> Result<Response, (StatusCode, String)> {
    params.project = resolve_signin_project(&state.db_pool, params.project.as_deref()).await?;
    let allowed_hosts = match params.project.as_deref() {
        Some(project) => project_redirect_hosts(&state, project).await,
        None => Vec::new(),
    };
    let redirect_url = params
        .rd
        .as_ref()
        .or(params.redirect.as_ref())
        .map(|target| validate_redirect_url(target, &state.public_url, &allowed_hosts));
    let is_app_start = uri.path().starts_with("/.rise/auth");
    let mut transaction = OAuth2State {
        browser_proof: None,
        app_browser_proof: None,
        project_id: None,
        code_verifier: generate_code_verifier(),
        redirect_url,
        project_name: params.project.clone(),
        app_base_url: None,
    };

    if let Some(project_name) = params.project.as_deref() {
        let app_urls = project_app_urls(&state, project_name).await;
        let app_url = if is_app_start {
            app_urls
                .iter()
                .find(|url| request_matches_origin(&headers, url))
        } else {
            if !request_matches_origin(&headers, &state.public_url) {
                return Err(invalid_browser_binding());
            }
            transaction
                .redirect_url
                .as_ref()
                .and_then(|target| url::Url::parse(target).ok())
                .and_then(|target| {
                    app_urls.iter().find(|url| {
                        url::Url::parse(url).is_ok_and(|url| url.origin() == target.origin())
                    })
                })
                .or_else(|| app_urls.first())
        }
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                "This host does not serve the requested project".into(),
            )
        })?;
        let app_url = url::Url::parse(app_url).map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Invalid project URL".into(),
            )
        })?;
        let app_origin = app_url.origin().ascii_serialization();
        if !is_app_start {
            let mut target = url::Url::parse(&format!("{app_origin}/.rise/auth/signin/start"))
                .map_err(|_| invalid_browser_binding())?;
            target
                .query_pairs_mut()
                .append_pair("project", project_name);
            if let Some(redirect) = &transaction.redirect_url {
                target.query_pairs_mut().append_pair("redirect", redirect);
            }
            return Ok(Redirect::to(target.as_str()).into_response());
        }
        let project = projects::find_by_name(&state.db_pool, project_name)
            .await
            .map_err(login_storage_error)?
            .filter(|project| project.status != crate::db::models::ProjectStatus::Deleting)
            .ok_or_else(|| (StatusCode::NOT_FOUND, "Project not found".into()))?;
        transaction.project_id = Some(project.id);
        transaction.app_base_url = Some(app_origin);
        let (proof, cookie) = BrowserProof::new("app", &state.cookie_settings, &headers);
        transaction.app_browser_proof = Some(proof);
        let handle = generate_state_token();
        state
            .token_store
            .save(format!("app-start:{handle}"), transaction)
            .await
            .map_err(login_storage_error)?;
        let begin = format!(
            "{}/api/v1/auth/signin/begin?transaction={handle}",
            state.public_url.trim_end_matches('/')
        );
        return with_binding_cookie(Redirect::to(&begin).into_response(), &cookie);
    }

    if is_app_start || !request_matches_origin(&headers, &state.public_url) {
        return Err(invalid_browser_binding());
    }
    start_central_oauth(&state, &headers, transaction).await
}

#[derive(Debug, Deserialize)]
pub struct BeginQuery {
    pub transaction: String,
}

/// Establish central-host browser proof for an app-origin sign-in transaction.
#[instrument(skip(state, headers, params))]
pub async fn oauth_signin_begin(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<BeginQuery>,
) -> Result<Response, (StatusCode, String)> {
    if !request_matches_origin(&headers, &state.public_url) {
        return Err(invalid_browser_binding());
    }
    let pending = state
        .token_store
        .claim(&format!("app-start:{}", params.transaction))
        .await
        .map_err(login_storage_error)?
        .ok_or_else(invalid_browser_binding)?;
    if pending.app_browser_proof.is_none()
        || pending.browser_proof.is_some()
        || pending.project_id.is_none()
        || pending.project_name.is_none()
        || pending.app_base_url.is_none()
    {
        pending.release().await.map_err(login_storage_error)?;
        return Err(invalid_browser_binding());
    }
    let response = start_central_oauth(&state, &headers, pending.data().clone()).await?;
    pending.finalize().await.map_err(login_storage_error)?;
    Ok(response)
}

async fn start_central_oauth(
    state: &AppState,
    headers: &HeaderMap,
    mut transaction: OAuth2State,
) -> Result<Response, (StatusCode, String)> {
    let (proof, cookie) = BrowserProof::new("central", &state.cookie_settings, headers);
    transaction.browser_proof = Some(proof);
    let code_challenge = generate_code_challenge(&transaction.code_verifier);
    let state_token = generate_state_token();
    state
        .token_store
        .save(state_token.clone(), transaction)
        .await
        .map_err(login_storage_error)?;
    let callback_url = format!(
        "{}/api/v1/auth/callback",
        state.public_url.trim_end_matches('/')
    );
    let params = crate::server::auth::oauth::AuthorizeParams {
        client_id: &state.auth_settings.client_id,
        redirect_uri: &callback_url,
        response_type: "code",
        scope: state.oauth_client.scopes(),
        code_challenge: &code_challenge,
        code_challenge_method: "S256",
        state: Some(&state_token),
    };
    with_binding_cookie(
        Redirect::to(&state.oauth_client.build_authorize_url(&params)).into_response(),
        &cookie,
    )
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
/// - App sign-in carries proofs on both hosts and an exact app origin.
///   Its one-time completion token requires the original app-host proof.
#[instrument(skip(state, params, headers))]
pub async fn oauth_callback(
    State(state): State<AppState>,
    headers: HeaderMap,
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
    let binding_valid = request_matches_origin(&headers, &state.public_url)
        && claimed_state
            .browser_proof
            .as_ref()
            .is_some_and(|proof| proof.matches(&headers));
    if !binding_valid {
        claimed_state.release().await.map_err(login_storage_error)?;
        return Err(invalid_browser_binding());
    }
    if let Some(project_name) = claimed_state.project_name.as_deref() {
        let project = projects::find_by_name(&state.db_pool, project_name)
            .await
            .map_err(login_storage_error)?;
        let valid_project = project.as_ref().is_some_and(|project| {
            Some(project.id) == claimed_state.project_id
                && project.status != crate::db::models::ProjectStatus::Deleting
        });
        let valid_origin = if let Some(origin) = claimed_state.app_base_url.as_ref() {
            project_app_urls(&state, project_name)
                .await
                .iter()
                .any(|url| {
                    url::Url::parse(url)
                        .is_ok_and(|url| url.origin().ascii_serialization() == *origin)
                })
        } else {
            false
        };
        if !valid_project || !valid_origin || claimed_state.app_browser_proof.is_none() {
            claimed_state.release().await.map_err(login_storage_error)?;
            return Err(invalid_browser_binding());
        }
    }
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

    crate::token_trace::trace_token("IdP ID token", &token_info.id_token);

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

        // A disabled User may not log in to an app either. The ingress token
        // itself keeps its app-facing shape: the IdP's `sub`, no Rise UID.
        let user = resolve_login(&state, &claims)
            .await
            .map_err(|failure| failure.response())?
            .user;

        let app_base_url = claimed_state
            .app_base_url
            .clone()
            .ok_or_else(invalid_browser_binding)?;

        // Resolve the user's team memberships for the groups claim; on a DB
        // error, fall back to no groups rather than failing the login.
        let groups = crate::db::teams::get_team_names_for_user(&state.db_pool, user.id)
            .await
            .ok();

        let rise_jwt = state
            .jwt_signer
            .sign_ingress_jwt(&claims, groups, &app_base_url, None)
            .map_err(|e| {
                tracing::error!("Failed to sign Rise JWT: {:#}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to create authentication token".to_string(),
                )
            })?;

        // Always redirect to the app's domain to set the cookie there.
        // The /.rise/auth/complete handler sets the cookie scoped to the current host.
        let completion_token = generate_state_token();

        let completed_session = CompletedAuthSession {
            browser_proof: claimed_state.app_browser_proof.clone(),
            app_base_url: Some(app_base_url.clone()),
            project_id: claimed_state.project_id,
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
            crate::server::middleware::url_for_log(&complete_url)
        );

        claimed_state
            .finalize()
            .await
            .map_err(login_storage_error)?;
        return Ok(Redirect::to(&complete_url).into_response());
    }

    // Regular OAuth flow (not ingress auth) - UI login
    tracing::info!("Using Rise JWT for UI session");

    let login = resolve_login(&state, &claims)
        .await
        .map_err(|failure| failure.response())?;
    let rise_jwt = issue_session(&state, &claims, &login, SessionClient::Browser, None)
        .await
        .map_err(|failure| failure.response())?;

    let cookie = cookie_helpers::create_rise_jwt_cookie(&rise_jwt, &state.cookie_settings, max_age);

    tracing::info!(
        "Setting Rise JWT cookie and redirecting to {}",
        url_for_log(&redirect_url)
    );

    // Use success page with delayed redirect to ensure cookie is properly persisted
    let response = render_ui_login_success_page(&state, &redirect_url, &cookie).await?;
    claimed_state
        .finalize()
        .await
        .map_err(login_storage_error)?;
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
        url_for_log(redirect_url)
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
#[instrument(skip(state, params, headers))]
pub async fn oauth_complete(
    State(state): State<AppState>,
    headers: HeaderMap,
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
    let binding_valid = claimed_session
        .browser_proof
        .as_ref()
        .is_some_and(|proof| proof.matches(&headers))
        && claimed_session
            .app_base_url
            .as_ref()
            .is_some_and(|origin| request_matches_origin(&headers, origin));
    let project = projects::find_by_name(&state.db_pool, &claimed_session.project_name)
        .await
        .map_err(login_storage_error)?;
    let valid_project = project.as_ref().is_some_and(|project| {
        Some(project.id) == claimed_session.project_id
            && project.status != crate::db::models::ProjectStatus::Deleting
    });
    let valid_origin = if let Some(origin) = claimed_session.app_base_url.as_ref() {
        project_app_urls(&state, &claimed_session.project_name)
            .await
            .iter()
            .any(|url| {
                url::Url::parse(url).is_ok_and(|url| url.origin().ascii_serialization() == *origin)
            })
    } else {
        false
    };
    if !binding_valid || !valid_project || !valid_origin {
        claimed_session
            .release()
            .await
            .map_err(login_storage_error)?;
        return Err(invalid_browser_binding());
    }
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
    claimed_session
        .finalize()
        .await
        .map_err(login_storage_error)?;
    Ok(response)
}

#[derive(Debug, Deserialize)]
pub struct IngressAuthQuery {
    pub project: String,
    /// Traefik mode. Traefik forwardAuth has no nginx-style `auth-signin`: on a
    /// non-2xx auth response it returns that response (status + Location) to the
    /// browser. When set (`signin_redirect=1`), an unauthenticated/invalid
    /// session yields a `302 Found` to the login page instead of a bare `401`,
    /// so the browser is sent to sign in. Unset (nginx) keeps the `401` behavior
    /// byte-identical — nginx itself performs the auth-signin redirect.
    #[serde(default, deserialize_with = "deserialize_bool_flag")]
    pub signin_redirect: bool,
    /// Per-route access requirement, stamped into the `auth-url` / `forwardAuth`
    /// address by the reconciler for the route group this ingress/router serves.
    /// When present it is enforced instead of the project's access-class default,
    /// so a single route can be opened or tightened. Control-plane input only —
    /// the client never influences which `auth-url` the proxy calls; it supplies
    /// only the path, and the proxy selects the matching route group. When absent,
    /// the project's access-class requirement applies (unchanged behavior).
    #[serde(default)]
    pub access: Option<crate::server::settings::AccessRequirement>,
}

/// Deserialize a query flag that may be `1`/`true`/`0`/`false` (or absent).
fn deserialize_bool_flag<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    Ok(matches!(
        opt.as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    ))
}

/// Select the trusted request-path header for the `/.rise/*` allowlist by proxy
/// mode.
///
/// nginx authoritatively sets `x-auth-request-redirect` (inbound client copies
/// are stripped), while Traefik forwardAuth (`signin_redirect=true`)
/// authoritatively overwrites `x-forwarded-uri` with the real request URI. Only
/// the mode-appropriate header is read: reading the other would trust a
/// client-controllable value (e.g. under Traefik a client can spoof
/// `x-auth-request-redirect: /.rise/x` to bypass auth on an arbitrary path).
fn allowlist_path(headers: &HeaderMap, traefik_mode: bool) -> Option<&str> {
    if traefik_mode {
        headers.get("x-forwarded-uri")
    } else {
        headers.get("x-auth-request-redirect")
    }
    .and_then(|v| v.to_str().ok())
}

/// Whether a request URI addresses the Rise-served `/.rise/*` tree and may pass
/// the ingress gate without a session.
///
/// `uri` is the request URI as the proxy received it. ingress-nginx fills
/// `x-auth-request-redirect` from the raw `$request_uri` but routes by the
/// normalized path (percent-decoded, dot-segments resolved), so a raw
/// `/.rise/../secret` or `/.rise/%2e%2e/secret` routes to the app at `/secret`.
/// The check therefore fails closed on anything a proxy could normalize: the
/// path (query stripped) must start with `/.rise/` and contain no `%` escape, no
/// backslash, and no `.` or `..` segment. The Rise-served `/.rise/auth/*` paths
/// need none of these.
fn is_rise_allowlisted_path(uri: &str) -> bool {
    let path = uri.split(['?', '#']).next().unwrap_or_default();
    path.starts_with("/.rise/")
        && !path.contains(['%', '\\'])
        && !path
            .split('/')
            .any(|segment| segment == "." || segment == "..")
}

/// Build the browser-facing login redirect URL for Traefik forwardAuth mode.
///
/// Mirrors the Kubernetes `backend_address` ingress-auth flow (see
/// `ResourceBuilder::build_ingress_annotations` in
/// `crates/rise-backend-kubernetes/src/resource_builder.rs`): the signin page must be served
/// on the **same host** as the app so the login cookie is set on the app host
/// that forwardAuth subsequently reads. A cross-host redirect to the control
/// plane would set the cookie on the wrong host and loop.
///
/// Traefik routes `PathPrefix('/.rise')` on every host to the Rise backend, so
/// `{scheme}://{X-Forwarded-Host}/.rise/auth/signin` is served by the backend
/// even though the host is the app's. The original request URL is
/// reconstructed from `scheme` + `X-Forwarded-Host` + `X-Forwarded-Uri` and
/// embedded as `redirect` so the user returns to where they started after
/// signing in.
///
/// `scheme` is the configured `ingress_schema`. `X-Forwarded-Proto` is not
/// usable: forwardAuth derives it from Traefik's own connection, which is plain
/// HTTP behind a TLS-terminating load balancer. Trusting the incoming header
/// instead (`trustForwardHeader`) is not an option either — it would also trust
/// a client-supplied `X-Forwarded-Uri`, which `allowlist_path` relies on.
///
/// Falls back to the control-plane `signin_base` URL only when
/// `X-Forwarded-Host` is absent (degraded path — the same-host redirect cannot
/// be constructed without the forwarded host).
fn build_signin_redirect_url(
    signin_base: &str,
    scheme: &str,
    project: &str,
    headers: &HeaderMap,
) -> String {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let host = header("x-forwarded-host");
    let uri = header("x-forwarded-uri").unwrap_or("/");

    let enc_project = urlencoding::encode(project);

    if let Some(host) = host {
        // Same-host signin page (served by the backend via the /.rise router).
        let original = format!("{scheme}://{host}{uri}");
        format!(
            "{scheme}://{host}/.rise/auth/signin?project={enc_project}&redirect={}",
            urlencoding::encode(&original)
        )
    } else {
        // Degraded fallback: no forwarded host → target the control-plane signin
        // page on the configured base URL (cookie host may be wrong).
        let base = signin_base.trim_end_matches('/');
        format!("{base}/api/v1/auth/signin?project={enc_project}")
    }
}

/// Ingress auth endpoint (nginx `auth-url` / Traefik `forwardAuth`)
///
/// Called by the ingress proxy for every request to a project whose access
/// class requires authentication. It validates the session cookie, checks JWT
/// validity, and verifies project access authorization.
///
/// Two modes, selected by the `signin_redirect` query flag:
/// - **nginx** (flag unset): unauthenticated → `401`; nginx redirects to
///   `auth-signin` itself.
/// - **Traefik** (`signin_redirect=1`): unauthenticated → `302` to the login
///   page, since Traefik forwardAuth has no `auth-signin`.
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

    // Allow access to /.rise/* paths without authentication (login page, static
    // assets). This prevents redirect loops when users try to access the signin
    // page. The trusted path header is selected by proxy mode: nginx authoritatively
    // sets `x-auth-request-redirect` (inbound client copies are stripped), while
    // Traefik forwardAuth authoritatively overwrites `x-forwarded-uri` with the real
    // request URI. Read ONLY the mode-appropriate header — reading the other would
    // trust a client-controllable value (e.g. under Traefik a client can spoof
    // `x-auth-request-redirect: /.rise/x` to bypass auth on an arbitrary path).
    // Both headers carry the raw URI, which `is_rise_allowlisted_path` rejects
    // whenever the proxy's normalized routing path could leave `/.rise/`.
    if let Some(redirect_path) = allowlist_path(&headers, params.signin_redirect) {
        if is_rise_allowlisted_path(redirect_path) {
            tracing::debug!(
                project = %params.project,
                redirect_path = %redirect_path,
                "Allowing unauthenticated access to .rise path"
            );
            // Emit BOTH identity headers (matching Traefik's authResponseHeaders
            // list) so a client cannot smuggle a forged X-Auth-Request-User /
            // X-Auth-Request-Email through the allowlisted /.rise path — the
            // empty/anonymous values clobber any client-supplied ones.
            return Ok((
                StatusCode::OK,
                [
                    ("X-Auth-Request-User", "anonymous".to_string()),
                    ("X-Auth-Request-Email", String::new()),
                ],
            )
                .into_response());
        }
    }

    // Helper: build the unauthenticated response. In Traefik mode
    // (`signin_redirect=1`) return a 302 to the login page (Traefik relays it to
    // the browser); otherwise keep the bare 401 nginx expects.
    let unauthenticated = |reason: &str| -> Response {
        if params.signin_redirect {
            let location = build_signin_redirect_url(
                &state.signin_base_url,
                &state.ingress_schema,
                &params.project,
                &headers,
            );
            tracing::debug!(
                project = %params.project,
                location = %location,
                "Unauthenticated ingress request — redirecting to signin (Traefik mode)"
            );
            (StatusCode::FOUND, [("Location", location)]).into_response()
        } else {
            (StatusCode::UNAUTHORIZED, reason.to_string()).into_response()
        }
    };

    // Extract and validate Rise JWT (required)
    let rise_jwt = match cookie_helpers::extract_rise_jwt_cookie(&headers) {
        Some(jwt) => jwt,
        None => {
            tracing::debug!("No Rise JWT cookie found");
            return Ok(unauthenticated("No session cookie"));
        }
    };

    let ingress_claims = match state.jwt_signer.verify_jwt_skip_aud(&rise_jwt) {
        Ok(claims) => claims,
        Err(e) => {
            tracing::warn!("Invalid or expired ingress JWT: {:#}", e);
            return Ok(unauthenticated("Invalid or expired session"));
        }
    };

    // A token minted for another app (which received it as its own cookie)
    // never opens this one.
    if !audience_admits_project(&state, &params.project, &ingress_claims.aud).await {
        tracing::warn!(
            project = %params.project,
            aud = %ingress_claims.aud,
            "Ingress token was minted for another app"
        );
        return Ok(unauthenticated("Session is for another app"));
    }

    // A disabled User or identity loses app access with every other token: a
    // session re-resolves its User and minting identity, and an ingress token
    // — which keeps the IdP's `sub` — is checked against that identity's
    // mapping.
    let uids = ingress_claims
        .rise_uid
        .zip(ingress_claims.rise_identity_uid);
    match state
        .user_logins
        .check_token(&ingress_claims.sub, uids)
        .await
    {
        Ok(TokenStanding::Active(_)) => {}
        Ok(TokenStanding::Rejected(rejection)) => {
            tracing::warn!("Ingress token rejected: {rejection}");
            return Ok(unauthenticated("Invalid or expired session"));
        }
        Err(error) => {
            tracing::error!("Failed to re-resolve the ingress token's User: {error:?}");
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "Database error".to_string(),
            ));
        }
    }

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

    // The effective requirement is the per-route `access` override stamped into
    // the auth-url by the reconciler (control-plane input), falling back to the
    // project's access-class requirement. The proxy — not the client — selects
    // which route group's auth-url is called, so trusting this param is safe.
    let requirement = params
        .access
        .clone()
        .unwrap_or_else(|| access_class.access_requirement.clone());

    // Handle different access requirements
    match requirement {
        AccessRequirement::None => {
            // Should never be called - None means no auth annotations/middleware
            // But if it is called, deny access as a safety measure
            tracing::warn!(
                project = %params.project,
                "Auth endpoint called for AccessRequirement::None route"
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

/// Where logout sends the browser: `redirect` under the post-login redirect
/// policy, without project hosts since logout is served on the control-plane
/// host only. Anything the policy refuses, and no `redirect` at all, is `/`.
fn logout_redirect_target(redirect: Option<&str>, public_url: &str) -> String {
    redirect
        .map(|url| validate_redirect_url(url, public_url, &[]))
        .unwrap_or_else(|| "/".to_string())
}

/// Logout endpoint
///
/// Clears the session cookie and redirects the user.
#[instrument(skip(state, params))]
pub async fn oauth_logout(
    State(state): State<AppState>,
    Query(params): Query<LogoutQuery>,
) -> Result<Response, (StatusCode, String)> {
    tracing::info!("Logout initiated");

    // Clear the Rise JWT cookie
    let cookie = cookie_helpers::clear_rise_jwt_cookie(&state.cookie_settings);

    let redirect_url = logout_redirect_target(params.redirect.as_deref(), &state.public_url);

    tracing::info!(
        "Clearing Rise JWT cookie and redirecting to {}",
        url_for_log(&redirect_url)
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
    /// The session's access as the CLI read it back: `full`, or one
    /// `<target>: <access>` line per grant.
    pub access: Option<String>,
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
    match params.access.as_deref() {
        Some("full") => context.insert("access_full", &true),
        Some(grants) => context.insert("access_grants", &grants.lines().collect::<Vec<_>>()),
        None => {}
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

    #[sqlx::test]
    async fn signin_requires_an_existing_project_before_constructing_urls(pool: sqlx::PgPool) {
        for supplied in ["evil.example/", "evil.example%2F", "missing", ""] {
            let error = resolve_signin_project(&pool, Some(supplied))
                .await
                .unwrap_err();
            assert_eq!(error.0, StatusCode::NOT_FOUND);
        }
        assert_eq!(resolve_signin_project(&pool, None).await.unwrap(), None);
        let owner = users::create(&pool, "owner@example.com").await.unwrap();
        let project = projects::create(
            &pool,
            "valid-project",
            crate::db::models::ProjectStatus::Stopped,
            "public".to_string(),
            Some(owner.id),
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            resolve_signin_project(&pool, Some(&project.name))
                .await
                .unwrap(),
            Some(project.name)
        );
    }

    #[test]
    fn signin_redirect_url_reconstructs_from_forwarded_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-host", "secret.rise.localhost".parse().unwrap());
        headers.insert("x-forwarded-uri", "/dashboard?tab=1".parse().unwrap());
        let url =
            build_signin_redirect_url("http://localhost:3000", "https", "secret app", &headers);
        // Same-host signin page served by the /.rise router on the app host,
        // project (URL-encoded), and redirect (full original URL, encoded).
        assert!(
            url.starts_with("https://secret.rise.localhost/.rise/auth/signin?project=secret%20app")
        );
        assert!(url.contains("&redirect=https%3A%2F%2Fsecret.rise.localhost%2Fdashboard%3Ftab%3D1"));
    }

    #[test]
    fn signin_redirect_url_targets_app_host_not_control_plane() {
        // The signin page must be served on the SAME (app) host so the login
        // cookie lands on the host forwardAuth reads — never on the control
        // plane host from signin_base.
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-host", "app.rise.localhost".parse().unwrap());
        headers.insert("x-forwarded-uri", "/".parse().unwrap());
        let url = build_signin_redirect_url("http://rise.localhost:3000", "https", "app", &headers);
        let parsed = url::Url::parse(&url).unwrap();
        assert_eq!(parsed.host_str(), Some("app.rise.localhost"));
        assert_ne!(parsed.host_str(), Some("rise.localhost"));
        assert!(url.starts_with("https://app.rise.localhost/.rise/auth/signin?project=app"));
    }

    #[test]
    fn signin_redirect_url_uses_configured_scheme_over_forwarded_proto() {
        // Behind a TLS-terminating load balancer forwardAuth reports `http`; the
        // browser is on https, and so must the sign-in page and return URL be.
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-proto", "http".parse().unwrap());
        headers.insert("x-forwarded-host", "app.rise.localhost".parse().unwrap());
        headers.insert("x-forwarded-uri", "/docs".parse().unwrap());
        let url = build_signin_redirect_url("http://rise.localhost:3000", "https", "app", &headers);
        assert!(url.starts_with("https://app.rise.localhost/.rise/auth/signin?project=app"));
        assert!(url.contains("&redirect=https%3A%2F%2Fapp.rise.localhost%2Fdocs"));
        assert!(!url.contains("http%3A"));
    }

    #[test]
    fn signin_redirect_url_falls_back_without_forwarded_host() {
        let headers = HeaderMap::new();
        let url =
            build_signin_redirect_url("http://rise.localhost:3000/", "https", "app", &headers);
        // No forwarded host → degraded fallback to the control-plane signin page.
        // Trailing slash on base is trimmed; no redirect param when host absent.
        assert_eq!(
            url,
            "http://rise.localhost:3000/api/v1/auth/signin?project=app"
        );
    }

    #[test]
    fn audience_names_only_its_own_app() {
        let app = "https://shop.apps.example.com";
        assert!(audience_names_app(app, "https://shop.apps.example.com/"));
        // Either scheme reaches the same app.
        assert!(audience_names_app(app, "http://shop.apps.example.com"));
        assert!(!audience_names_app(app, "https://other.apps.example.com"));
        assert!(!audience_names_app(
            app,
            "https://shop.apps.example.com:8443"
        ));

        assert!(!audience_names_app("not a url", app));
    }

    #[test]
    fn ingress_auth_query_parses_signin_redirect_flag() {
        let q: IngressAuthQuery =
            serde_urlencoded::from_str("project=app&signin_redirect=1").unwrap();
        assert!(q.signin_redirect);
        let q: IngressAuthQuery = serde_urlencoded::from_str("project=app").unwrap();
        assert!(!q.signin_redirect);
        let q: IngressAuthQuery =
            serde_urlencoded::from_str("project=app&signin_redirect=0").unwrap();
        assert!(!q.signin_redirect);
    }

    #[test]
    fn ingress_auth_query_parses_reconciler_stamped_access_param() {
        use crate::server::settings::AccessRequirement;
        // The reconciler stamps `&access=<PascalCase>` into the auth-url; the
        // handler must parse it back to override the project's requirement.
        let q: IngressAuthQuery =
            serde_urlencoded::from_str("project=app&access=Member&signin_redirect=1").unwrap();
        assert_eq!(q.access, Some(AccessRequirement::Member));
        // Absent → None → project default applies (unchanged behavior).
        let q: IngressAuthQuery = serde_urlencoded::from_str("project=app").unwrap();
        assert_eq!(q.access, None);
    }

    #[test]
    fn rise_allowlist_accepts_rise_served_paths() {
        for uri in [
            "/.rise/auth/signin",
            "/.rise/auth/signin?project=app&redirect=https%3A%2F%2Fapp.rise.dev%2F..%2Fx",
            "/.rise/auth/signin/start?project=app",
            "/.rise/auth/complete?token=abc",
            "/.rise/assets/app.v1.2.js",
        ] {
            assert!(is_rise_allowlisted_path(uri), "{uri} must be allowlisted");
        }
    }

    #[test]
    fn rise_allowlist_rejects_paths_a_proxy_normalizes_out_of_rise() {
        for uri in [
            "/secret",
            "/.rise",
            "/.risesecret",
            "/x/.rise/auth/signin",
            // Dot-segments: nginx routes these by the resolved path.
            "/.rise/../secret",
            "/.rise/./../secret",
            "/.rise/auth/../../secret",
            "/.rise/..",
            "/.rise/.",
            "/.rise//../secret",
            "/.rise/../secret?x=/.rise/",
            // Percent-encoded dots and slashes: nginx decodes before resolving.
            "/.rise/%2e%2e/secret",
            "/.rise/%2E%2E/secret",
            "/.rise/.%2e/secret",
            "/.rise/%2e./secret",
            "/.rise/..%2fsecret",
            "/.rise/..%2Fsecret",
            "/.rise/%2e%2e%2fsecret",
            "/.rise/%252e%252e/secret",
            // Backslash separators.
            "/.rise/..\\secret",
            "/.rise/..%5csecret",
        ] {
            assert!(
                !is_rise_allowlisted_path(uri),
                "{uri} must not be allowlisted"
            );
        }
    }

    #[test]
    fn allowlist_path_ignores_spoofable_header_in_traefik_mode() {
        // In Traefik forwardAuth mode (signin_redirect=true) only the
        // authoritative `x-forwarded-uri` is trusted. A client spoofing
        // `x-auth-request-redirect: /.rise/x` while the real request is `/admin`
        // must NOT be allowlisted.
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-uri", "/admin".parse().unwrap());
        headers.insert("x-auth-request-redirect", "/.rise/x".parse().unwrap());

        let path = allowlist_path(&headers, true);
        assert_eq!(path, Some("/admin"));
        assert!(!path.unwrap().starts_with("/.rise/"));
    }

    #[test]
    fn allowlist_path_trusts_forwarded_uri_in_traefik_mode() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-uri", "/.rise/auth/signin".parse().unwrap());
        assert_eq!(allowlist_path(&headers, true), Some("/.rise/auth/signin"));
    }

    #[test]
    fn allowlist_path_uses_auth_request_redirect_in_nginx_mode() {
        // In nginx mode (signin_redirect=false) the server-set
        // `x-auth-request-redirect` is authoritative; `x-forwarded-uri` is ignored.
        let mut headers = HeaderMap::new();
        headers.insert("x-auth-request-redirect", "/.rise/login".parse().unwrap());
        headers.insert("x-forwarded-uri", "/admin".parse().unwrap());
        assert_eq!(allowlist_path(&headers, false), Some("/.rise/login"));
    }

    #[test]
    fn test_validate_redirect_url_relative_paths() {
        let public_url = "https://rise.dev";

        // Valid relative paths
        assert_eq!(validate_redirect_url("/", public_url, &[]), "/");
        assert_eq!(
            validate_redirect_url("/dashboard", public_url, &[]),
            "/dashboard"
        );
        assert_eq!(
            validate_redirect_url("/app/project/123", public_url, &[]),
            "/app/project/123"
        );

        // Protocol-relative URLs should be blocked
        assert_eq!(validate_redirect_url("//evil.com", public_url, &[]), "/");
        assert_eq!(
            validate_redirect_url("//evil.com/path", public_url, &[]),
            "/"
        );
    }

    #[test]
    fn test_validate_redirect_url_rejects_paths_browsers_resolve_off_site() {
        let public_url = "https://rise.dev";

        for redirect in [
            "/\\evil.com",
            "/\\/evil.com",
            "\\\\evil.com",
            "\\/evil.com",
            "/\t/evil.com",
            "/%2F%2Fevil.com",
            "/%5Cevil.com",
            "https://evil.com\\@rise.dev/",
            "https://rise.dev\t.evil.com/",
        ] {
            assert_eq!(
                validate_redirect_url(redirect, public_url, &[]),
                "/",
                "{redirect:?} must fall back to /"
            );
        }
    }

    #[test]
    fn logout_redirects_only_where_a_login_may() {
        let public_url = "https://rise.dev";

        assert_eq!(logout_redirect_target(None, public_url), "/");
        for (redirect, expected) in [
            ("https://evil.com/", "/"),
            ("//evil.com", "/"),
            ("/\\evil.com", "/"),
            ("javascript:alert(1)", "/"),
            ("/projects", "/projects"),
            ("https://rise.dev/projects", "https://rise.dev/projects"),
        ] {
            assert_eq!(
                logout_redirect_target(Some(redirect), public_url),
                expected,
                "logout redirect for {redirect:?}"
            );
        }
    }

    #[test]
    fn login_email_must_be_verified_by_the_idp() {
        use serde_json::json;

        let verified = json!({"email": "a@example.com", "email_verified": true});
        assert_eq!(
            verified_login_email(&verified, false).unwrap(),
            "a@example.com"
        );
        let verified_string = json!({"email": "a@example.com", "email_verified": "true"});
        assert_eq!(
            verified_login_email(&verified_string, false).unwrap(),
            "a@example.com"
        );

        for unverified in [
            json!({"email": "a@example.com", "email_verified": false}),
            json!({"email": "a@example.com", "email_verified": "false"}),
            json!({"email": "a@example.com", "email_verified": 1}),
            json!({"email": "a@example.com", "email_verified": null}),
        ] {
            for allow_missing in [false, true] {
                assert!(
                    matches!(
                        verified_login_email(&unverified, allow_missing),
                        Err(LoginFailure::UnverifiedEmail)
                    ),
                    "{unverified} must be refused (allow_missing={allow_missing})"
                );
            }
        }

        let missing = json!({"email": "a@example.com"});
        assert!(matches!(
            verified_login_email(&missing, false),
            Err(LoginFailure::UnverifiedEmail)
        ));
        assert_eq!(
            verified_login_email(&missing, true).unwrap(),
            "a@example.com"
        );

        assert!(matches!(
            verified_login_email(&json!({"email_verified": true}), true),
            Err(LoginFailure::MissingEmail)
        ));
    }

    fn login_claims(groups: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "sub": "user123",
            "email": "member@example.com",
            "email_verified": true,
            "iss": "https://issuer.example.com",
            "aud": "rise",
            "exp": 4102444800u64,
            "iat": 1700000000u64,
            "groups": groups,
        })
    }

    #[sqlx::test]
    async fn login_group_sync_reports_failure_so_the_login_is_refused(pool: sqlx::PgPool) {
        // A user id with no row: adding the membership violates its foreign
        // key, the way any database failure mid-sync would surface.
        let missing_user = uuid::Uuid::new_v4();
        let result = sync_login_groups(
            &pool,
            missing_user,
            &login_claims(serde_json::json!(["platform-admins"])),
            "groups",
            &[],
            crate::server::settings::MissingGroupClaimPolicy::RefuseLogin,
        )
        .await;
        assert!(result.is_err(), "a failed sync must fail the login");
    }

    #[sqlx::test]
    async fn login_group_sync_drops_groups_the_idp_no_longer_asserts(pool: sqlx::PgPool) {
        let user = users::create(&pool, "member@example.com").await.unwrap();
        sync_login_groups(
            &pool,
            user.id,
            &login_claims(serde_json::json!(["platform-admins"])),
            "groups",
            &[],
            crate::server::settings::MissingGroupClaimPolicy::RefuseLogin,
        )
        .await
        .unwrap();
        assert_eq!(
            crate::db::teams::get_team_names_for_user(&pool, user.id)
                .await
                .unwrap(),
            vec!["platform-admins".to_string()]
        );

        sync_login_groups(
            &pool,
            user.id,
            &login_claims(serde_json::json!([])),
            "groups",
            &[],
            crate::server::settings::MissingGroupClaimPolicy::RefuseLogin,
        )
        .await
        .unwrap();
        assert!(crate::db::teams::get_team_names_for_user(&pool, user.id)
            .await
            .unwrap()
            .is_empty());
    }

    #[sqlx::test]
    async fn missing_group_claim_follows_policy_without_preserving_a_new_privileged_login(
        pool: sqlx::PgPool,
    ) {
        use crate::server::settings::MissingGroupClaimPolicy::{RefuseLogin, TreatAsEmpty};
        let user = users::create(&pool, "groups@example.com").await.unwrap();
        for claim_name in ["groups", "custom:groups"] {
            for missing in [true, false] {
                for policy in [RefuseLogin, TreatAsEmpty] {
                    let mut claims = login_claims(serde_json::json!(["platform-admins"]));
                    claims[claim_name] = serde_json::json!(["platform-admins"]);
                    sync_login_groups(&pool, user.id, &claims, claim_name, &[], policy)
                        .await
                        .unwrap();
                    if missing {
                        claims.as_object_mut().unwrap().remove(claim_name);
                    } else {
                        claims[claim_name] = serde_json::Value::Null;
                    }
                    let result =
                        sync_login_groups(&pool, user.id, &claims, claim_name, &[], policy).await;
                    match policy {
                        RefuseLogin => assert!(result.is_err()),
                        TreatAsEmpty => {
                            result.unwrap();
                            assert!(crate::db::teams::get_team_names_for_user(&pool, user.id)
                                .await
                                .unwrap()
                                .is_empty());
                        }
                    }
                }
            }
        }
        for malformed in [
            serde_json::json!(42),
            serde_json::json!(["admins", 1]),
            serde_json::json!({}),
        ] {
            assert!(sync_login_groups(
                &pool,
                user.id,
                &login_claims(malformed),
                "groups",
                &[],
                TreatAsEmpty
            )
            .await
            .is_err());
        }
    }

    #[test]
    fn test_validate_redirect_url_dangerous_schemes() {
        let public_url = "https://rise.dev";

        // JavaScript URLs should be blocked
        assert_eq!(
            validate_redirect_url("javascript:alert('xss')", public_url, &[]),
            "/"
        );

        // Data URLs should be blocked
        assert_eq!(
            validate_redirect_url(
                "data:text/html,<script>alert('xss')</script>",
                public_url,
                &[]
            ),
            "/"
        );

        // vbscript URLs should be blocked
        assert_eq!(
            validate_redirect_url("vbscript:msgbox('xss')", public_url, &[]),
            "/"
        );
    }

    #[test]
    fn test_validate_redirect_url_same_domain() {
        let public_url = "https://rise.dev";

        // Same domain should be allowed
        assert_eq!(
            validate_redirect_url("https://rise.dev/dashboard", public_url, &[]),
            "https://rise.dev/dashboard"
        );

        // Same domain with port should be allowed
        assert_eq!(
            validate_redirect_url("https://rise.dev:8080/dashboard", public_url, &[]),
            "https://rise.dev:8080/dashboard"
        );
    }

    #[test]
    fn test_validate_redirect_url_subdomains() {
        let public_url = "https://rise.dev";

        // Subdomain should be allowed
        assert_eq!(
            validate_redirect_url("https://app.rise.dev/dashboard", public_url, &[]),
            "https://app.rise.dev/dashboard"
        );

        assert_eq!(
            validate_redirect_url("https://staging.rise.dev/dashboard", public_url, &[]),
            "https://staging.rise.dev/dashboard"
        );

        // Multi-level subdomain should be allowed
        assert_eq!(
            validate_redirect_url("https://my-project.app.rise.dev/", public_url, &[]),
            "https://my-project.app.rise.dev/"
        );
    }

    #[test]
    fn test_validate_redirect_url_external_domains() {
        let public_url = "https://rise.dev";

        // External domains should be blocked
        assert_eq!(
            validate_redirect_url("https://evil.com", public_url, &[]),
            "/"
        );

        assert_eq!(
            validate_redirect_url("https://phishing.site/login", public_url, &[]),
            "/"
        );

        // Domains that look similar but are not subdomains should be blocked
        assert_eq!(
            validate_redirect_url("https://rise.dev.evil.com", public_url, &[]),
            "/"
        );
    }

    #[test]
    fn test_validate_redirect_url_project_scoped() {
        // Control plane is rise.example.com; the app being authenticated lives at
        // secret.example.com (a sibling, not a subdomain). Only THIS project's
        // resolved hosts are trusted — not the whole parent domain.
        let public_url = "https://rise.example.com";
        let allowed = vec!["secret.example.com".to_string()];

        // The project's own host is accepted.
        assert_eq!(
            validate_redirect_url("https://secret.example.com/dashboard", public_url, &allowed),
            "https://secret.example.com/dashboard"
        );
        // Case-insensitive host match (url crate lowercases the parsed host).
        assert_eq!(
            validate_redirect_url("https://SECRET.EXAMPLE.COM/x", public_url, &allowed),
            "https://SECRET.EXAMPLE.COM/x"
        );
        // Without the project host in the allow-set it drops to "/".
        assert_eq!(
            validate_redirect_url("https://secret.example.com/dashboard", public_url, &[]),
            "/"
        );
        // A DIFFERENT project's host under the same parent domain is rejected:
        // redirects are gated by exact membership in the allow-set, not by a
        // shared parent domain.
        assert_eq!(
            validate_redirect_url("https://other.example.com/", public_url, &allowed),
            "/"
        );
        // Lookalikes remain blocked — exact host equality, not a string prefix.
        assert_eq!(
            validate_redirect_url("https://secret.example.com.evil.com/", public_url, &allowed),
            "/"
        );
        // Userinfo trick: the real host is evil.com, so it is rejected.
        assert_eq!(
            validate_redirect_url("https://secret.example.com@evil.com/", public_url, &allowed),
            "/"
        );
        assert_eq!(
            validate_redirect_url("https://notexample.com/", public_url, &allowed),
            "/"
        );
    }

    #[test]
    fn test_validate_redirect_url_localhost() {
        let public_url = "http://localhost:3000";

        // localhost to localhost should be allowed
        assert_eq!(
            validate_redirect_url("http://localhost:3000/dashboard", public_url, &[]),
            "http://localhost:3000/dashboard"
        );

        assert_eq!(
            validate_redirect_url("http://127.0.0.1:3000/dashboard", public_url, &[]),
            "http://127.0.0.1:3000/dashboard"
        );

        // Malicious localhost URLs with invalid ports should be rejected during parsing
        // The URL parser will fail to parse "localhost:evil.com" as a valid port
        assert_eq!(
            validate_redirect_url("http://localhost:evil.com/path", public_url, &[]),
            "/"
        );

        // But external URLs should still be blocked even when public_url is localhost
        assert_eq!(
            validate_redirect_url("https://evil.com", public_url, &[]),
            "/"
        );
    }

    #[test]
    fn test_validate_redirect_url_localhost_production_blocked() {
        let public_url = "https://rise.dev";

        // localhost should be blocked when public_url is not localhost
        assert_eq!(
            validate_redirect_url("http://localhost:3000/dashboard", public_url, &[]),
            "/"
        );

        assert_eq!(
            validate_redirect_url("http://127.0.0.1:3000/dashboard", public_url, &[]),
            "/"
        );
    }

    #[test]
    fn test_validate_redirect_url_empty_and_invalid() {
        let public_url = "https://rise.dev";

        // Empty string should return fallback
        assert_eq!(validate_redirect_url("", public_url, &[]), "/");

        // Whitespace only should return fallback
        assert_eq!(validate_redirect_url("   ", public_url, &[]), "/");

        // Invalid URLs should return fallback
        assert_eq!(validate_redirect_url("not a url", public_url, &[]), "/");
    }

    #[test]
    fn test_validate_redirect_url_http_vs_https() {
        let public_url = "https://rise.dev";

        // HTTP URLs should be allowed for same domain
        assert_eq!(
            validate_redirect_url("http://rise.dev/dashboard", public_url, &[]),
            "http://rise.dev/dashboard"
        );

        // HTTPS URLs should be allowed for same domain
        assert_eq!(
            validate_redirect_url("https://rise.dev/dashboard", public_url, &[]),
            "https://rise.dev/dashboard"
        );
    }
}
