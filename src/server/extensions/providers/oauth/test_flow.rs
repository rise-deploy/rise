//! The Rise UI's "Test OAuth" flow.
//!
//! A signed-in project member signs in through the extension's relay, with the
//! Rise test callback as the redirect URI, and the callback redeems the code in
//! process. Only this flow writes the extension's status:
//!
//! 1. only an unscoped Rise UI browser session of an active user with platform
//!    and project access can start it;
//! 2. the callback must complete in the same user's session, in the browser
//!    that started it (a path-scoped cookie carries the flow's `state`);
//! 3. the stored failure text is limited to bounded upstream `error` /
//!    `error_description` values;
//! 4. end-user logins through the relay never touch the status.

use std::collections::HashMap;
use std::time::Duration;

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Redirect, Response},
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{error, info, warn};
use url::Url;
use uuid::Uuid;

use super::models::{ClientType, OAuthExtensionStatus};
use super::relay::resolver::{load_active_extension, test_callback_url, ActiveExtension};
use crate::db::{extensions as db_extensions, projects as db_projects};
use crate::server::auth::redirect::origins_match;
use crate::server::client_ip::ClientIp;
use crate::server::state::AppState;

/// Whether a redirect target is on the Rise public origin, i.e. the Rise UI's
/// "test OAuth" flow rather than an end-user app flow.
///
/// Compared by parsed origin; a string prefix would accept lookalike hosts
/// (`https://rise.example.com.evil.com`) and userinfo tricks
/// (`https://rise.example.com@evil.com`). An unparseable URL is never a match.
pub(crate) fn redirect_targets_rise_origin(redirect_uri: &str, public_url: &str) -> bool {
    match (Url::parse(redirect_uri), Url::parse(public_url)) {
        (Ok(redirect), Ok(public)) => origins_match(&redirect, &public),
        _ => false,
    }
}

/// Authenticate the Rise UI browser session behind a test flow.
///
/// The test flow writes the extension's stored status, so it is reserved for
/// a signed-in user. The Rise UI starts it as a navigation carrying the
/// `rise_jwt` session cookie, and the callback is served on the same host, so
/// both steps read it. Only that cookie is accepted (never a Bearer token),
/// and only a browser session: a CLI session placed in a cookie, or a scoped
/// session, is refused. The session is re-checked against the user store like
/// on every API request.
pub(crate) async fn authenticate_ui_session(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<crate::db::User, (StatusCode, String)> {
    use crate::server::auth::{cookie_helpers, user_identity::TokenStanding};
    use rise_backend_auth::SessionClient;

    let unauthenticated = || {
        (
            StatusCode::UNAUTHORIZED,
            "Redirecting to the Rise UI requires signing in to Rise as a user with access to this project"
                .to_string(),
        )
    };

    let token = cookie_helpers::extract_rise_jwt_cookie(headers).ok_or_else(unauthenticated)?;
    let claims = state
        .jwt_signer
        .verify_user_jwt(&token, &state.public_url)
        .map_err(|_| unauthenticated())?;

    if claims.rise_client != Some(SessionClient::Browser) {
        return Err((
            StatusCode::FORBIDDEN,
            "Only a Rise UI browser session can run an OAuth test flow".to_string(),
        ));
    }
    if claims.authorization_details.is_some() {
        return Err((
            StatusCode::FORBIDDEN,
            "A scoped session cannot run an OAuth test flow".to_string(),
        ));
    }

    let uids = claims.rise_uid.zip(claims.rise_identity_uid);
    match state.user_logins.check_token(&claims.sub, uids).await {
        Ok(TokenStanding::Active(_)) => {}
        Ok(TokenStanding::Rejected(_)) => return Err(unauthenticated()),
        Err(error) => {
            error!("Failed to re-resolve the session's User: {:?}", error);
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to verify session".to_string(),
            ));
        }
    }

    crate::db::users::find_or_create_with_default_organization(
        &state.db_pool,
        &claims.email,
        state.default_organization_uid,
    )
    .await
    .map_err(|error| {
        error!("Failed to find/create user: {:?}", error);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to verify session".to_string(),
        )
    })
}

/// Authenticate the user running a test flow and return their id.
///
/// On top of [`authenticate_ui_session`] the user must have platform access
/// and access to the project, the same gates the typed extension API applies.
pub(crate) async fn authenticate_test_flow_initiator(
    state: &AppState,
    headers: &HeaderMap,
    project: &crate::db::models::Project,
) -> Result<Uuid, (StatusCode, String)> {
    let user = authenticate_ui_session(state, headers).await?;

    if !crate::server::auth::middleware::user_has_platform_access(state, &user).await {
        return Err((
            StatusCode::FORBIDDEN,
            "You do not have access to Rise platform features".to_string(),
        ));
    }

    crate::server::project::handlers::ensure_project_access_or_admin(state, &user, project)
        .await
        .map_err(|e| (e.status, e.message))?;

    Ok(user.id)
}

/// Longest stored upstream error code / description, in characters.
const MAX_STORED_ERROR_CODE_CHARS: usize = 100;
const MAX_STORED_ERROR_DESCRIPTION_CHARS: usize = 200;

pub(crate) fn bounded_text(text: &str, max_chars: usize) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(max_chars)
        .collect()
}

/// The status text stored when a test flow's token exchange fails.
///
/// Only the RFC 6749 §5.2 `error` / `error_description` fields of a JSON body
/// are kept, each bounded; anything else in the upstream response (HTML pages,
/// stack traces, echoed request data) is dropped.
pub(crate) fn token_exchange_failure_summary(status: StatusCode, body: &str) -> String {
    let mut summary = format!("Token exchange failed with status {status}");
    let Ok(serde_json::Value::Object(fields)) = serde_json::from_str::<serde_json::Value>(body)
    else {
        return summary;
    };
    let field = |name: &str, max_chars: usize| {
        fields
            .get(name)
            .and_then(|value| value.as_str())
            .map(|text| bounded_text(text, max_chars))
            .filter(|text| !text.trim().is_empty())
    };
    if let Some(code) = field("error", MAX_STORED_ERROR_CODE_CHARS) {
        summary.push_str(&format!(": {code}"));
        if let Some(description) = field("error_description", MAX_STORED_ERROR_DESCRIPTION_CHARS) {
            summary.push_str(&format!(" ({description})"));
        }
    }
    summary
}

/// The status text stored when the provider answers the test authorization
/// with an error: its bounded `error` and `error_description`, nothing else.
pub(crate) fn authorization_failure_summary(
    error: Option<&str>,
    description: Option<&str>,
) -> String {
    let mut summary = "Authorization failed".to_string();
    let bounded = |text: Option<&str>, max_chars: usize| {
        text.map(|text| bounded_text(text, max_chars))
            .filter(|text| !text.trim().is_empty())
    };
    if let Some(code) = bounded(error, MAX_STORED_ERROR_CODE_CHARS) {
        summary.push_str(&format!(": {code}"));
        if let Some(description) = bounded(description, MAX_STORED_ERROR_DESCRIPTION_CHARS) {
            summary.push_str(&format!(" ({description})"));
        }
    }
    summary
}

/// The cookie binding a test flow to the browser that started it.
const TEST_FLOW_COOKIE: &str = "rise_oauth_test";
/// How long a started test flow can be completed.
const TEST_FLOW_TTL: Duration = Duration::from_secs(10 * 60);
/// Transient-store namespace of test-flow records.
const TEST_FLOW_KEY_PREFIX: &str = "rise:oauth-test:";
/// Longest token response body read back from the relay.
const MAX_TOKEN_RESPONSE_BYTES: usize = 64 * 1024;

/// A started test flow, stored under a hash of its `state`.
#[derive(Debug, Serialize, Deserialize)]
struct TestFlowRecord {
    user_id: Uuid,
    project_id: Uuid,
    extension: String,
    generation: String,
    verifier: String,
    return_to: String,
}

fn random_token() -> String {
    use rand::Rng;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn record_key(state: &str) -> String {
    format!(
        "{TEST_FLOW_KEY_PREFIX}{}",
        URL_SAFE_NO_PAD.encode(Sha256::digest(state.as_bytes()))
    )
}

fn cookie_path(project: &str, extension: &str) -> String {
    format!("/api/v1/projects/{project}/extensions/{extension}/oauth/test")
}

fn flow_cookie(
    state: &AppState,
    project: &str,
    extension: &str,
    value: &str,
    max_age: u64,
) -> String {
    let mut cookie = format!(
        "{TEST_FLOW_COOKIE}={value}; Path={}; Max-Age={max_age}; HttpOnly; SameSite=Lax",
        cookie_path(project, extension)
    );
    if state.cookie_settings.secure {
        cookie.push_str("; Secure");
    }
    cookie
}

fn flow_cookie_value(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, value)| *name == TEST_FLOW_COOKIE && !value.is_empty())
        .map(|(_, value)| value.to_owned())
}

fn bad_request(message: &str) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, message.to_string())
}

fn internal(message: &str) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, message.to_string())
}

async fn rate_limited(
    state: &AppState,
    client_ip: &ClientIp,
    headers: &HeaderMap,
    project: &str,
) -> Option<Response> {
    let session_key = crate::server::rate_limit::extract_session_key(headers);
    let retry_after = state
        .oauth_rate_limiter
        .increment_and_check(&client_ip.rate_limit_key(), session_key.as_deref(), project)
        .await
        .err()?;
    warn!(ip = %client_ip, %project, "OAuth test flow rate limited");
    Some(crate::server::rate_limit::rate_limit_response(retry_after))
}

async fn find_project(
    state: &AppState,
    project: &str,
) -> Result<crate::db::models::Project, (StatusCode, String)> {
    db_projects::find_by_name(&state.db_pool, project)
        .await
        .map_err(|e| {
            error!("Failed to load project {project}: {:?}", e);
            internal("Failed to load project")
        })?
        .ok_or((StatusCode::NOT_FOUND, "Project not found".to_string()))
}

async fn find_extension(
    state: &AppState,
    project: &str,
    extension: &str,
) -> Result<ActiveExtension, (StatusCode, String)> {
    load_active_extension(&state.db_pool, project, extension)
        .await
        .map_err(|e| {
            error!(
                "Failed to load OAuth extension {project}/{extension}: {:?}",
                e
            );
            internal("Failed to load the OAuth extension")
        })?
        .ok_or((
            StatusCode::NOT_FOUND,
            "OAuth extension not found".to_string(),
        ))
}

#[derive(Debug, Deserialize)]
pub struct TestFlowStartQuery {
    /// The Rise UI page to return to; it must be on Rise's public origin.
    pub return_to: String,
}

/// Starts the test flow.
///
/// `GET /api/v1/projects/{project}/extensions/{extension}/oauth/test?return_to=…`
pub async fn start(
    State(state): State<AppState>,
    Path((project_name, extension_name)): Path<(String, String)>,
    client_ip: ClientIp,
    headers: HeaderMap,
    Query(query): Query<TestFlowStartQuery>,
) -> Result<Response, (StatusCode, String)> {
    if let Some(response) = rate_limited(&state, &client_ip, &headers, &project_name).await {
        return Ok(response);
    }
    let project = find_project(&state, &project_name).await?;
    let user_id = authenticate_test_flow_initiator(&state, &headers, &project).await?;
    let active = find_extension(&state, &project_name, &extension_name).await?;
    if !redirect_targets_rise_origin(&query.return_to, &state.public_url) {
        return Err(bad_request("return_to must be a Rise UI URL"));
    }
    let public_url = Url::parse(&state.public_url)
        .map_err(|_| internal("Invalid Rise public URL configuration"))?;

    let flow_state = random_token();
    let verifier = random_token();
    let record = TestFlowRecord {
        user_id,
        project_id: project.id,
        extension: extension_name.clone(),
        generation: active.generation(),
        verifier: verifier.clone(),
        return_to: query.return_to,
    };
    let value = serde_json::to_vec(&record).map_err(|_| internal("Failed to start the test"))?;
    match state
        .oauth_relay
        .store()
        .put_if_absent(&record_key(&flow_state), &value, TEST_FLOW_TTL)
        .await
    {
        Ok(true) => {}
        Ok(false) => return Err(internal("Failed to start the test")),
        Err(e) => {
            error!("Failed to store an OAuth test flow: {:?}", e);
            return Err(internal("Failed to start the test"));
        }
    }

    let mut authorize = Url::parse(&format!(
        "{}/oidc/{project_name}/{extension_name}/authorize",
        state.public_url.trim_end_matches('/')
    ))
    .map_err(|_| internal("Invalid OAuth relay URL"))?;
    {
        let mut params = authorize.query_pairs_mut();
        params
            .append_pair("response_type", "code")
            .append_pair(
                "redirect_uri",
                &test_callback_url(&public_url, &project_name, &extension_name),
            )
            .append_pair("state", &flow_state)
            .append_pair(
                "code_challenge",
                &URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
            )
            .append_pair("code_challenge_method", "S256");
        if let Some(client_id) = &active.status.rise_client_id {
            params.append_pair("client_id", client_id);
        }
    }
    info!(
        user = %user_id,
        project = %project_name,
        extension = %extension_name,
        "Starting OAuth test flow"
    );
    let mut response = Redirect::to(authorize.as_str()).into_response();
    let cookie = flow_cookie(
        &state,
        &project_name,
        &extension_name,
        &flow_state,
        TEST_FLOW_TTL.as_secs(),
    );
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).map_err(|_| internal("Failed to start the test"))?,
    );
    Ok(response)
}

/// Completes the test flow: redeems the code in process and records the result
/// in the extension's status.
///
/// `GET /api/v1/projects/{project}/extensions/{extension}/oauth/test/callback`
pub async fn callback(
    State(state): State<AppState>,
    Path((project_name, extension_name)): Path<(String, String)>,
    client_ip: ClientIp,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, (StatusCode, String)> {
    use subtle::ConstantTimeEq;

    if let Some(response) = rate_limited(&state, &client_ip, &headers, &project_name).await {
        return Ok(response);
    }
    let project = find_project(&state, &project_name).await?;
    let user_id = authenticate_test_flow_initiator(&state, &headers, &project).await?;

    // The state must be the one this browser started: a test callback URL
    // carried to another browser, or forged for one, is refused.
    let (Some(flow_state), Some(cookie)) = (query.get("state"), flow_cookie_value(&headers)) else {
        return Err(bad_request(
            "This OAuth test was not started in this browser",
        ));
    };
    if !bool::from(flow_state.as_bytes().ct_eq(cookie.as_bytes())) {
        return Err(bad_request(
            "This OAuth test was not started in this browser",
        ));
    }

    let store = state.oauth_relay.store();
    let key = record_key(flow_state);
    let entry = store
        .get(&key)
        .await
        .map_err(|e| {
            error!("Failed to read an OAuth test flow: {:?}", e);
            internal("Failed to complete the test")
        })?
        .ok_or_else(|| bad_request("This OAuth test has expired; start it again"))?;
    let record: TestFlowRecord = serde_json::from_slice(&entry.value)
        .map_err(|_| bad_request("This OAuth test was already completed"))?;
    if record.user_id != user_id {
        warn!(
            project = %project_name,
            extension = %extension_name,
            initiator = %record.user_id,
            "Refusing OAuth test callback: the session is not the one that started the flow"
        );
        return Err((
            StatusCode::FORBIDDEN,
            "This OAuth test was started by a different user".to_string(),
        ));
    }
    // Single use: only the caller that swaps the record for a consumed marker
    // continues.
    let consumed = format!("consumed:{}", random_token());
    let claimed = store
        .compare_and_swap(&key, &entry.value, consumed.as_bytes(), TEST_FLOW_TTL)
        .await
        .map_err(|e| {
            error!("Failed to claim an OAuth test flow: {:?}", e);
            internal("Failed to complete the test")
        })?;
    if !claimed {
        return Err(bad_request("This OAuth test was already completed"));
    }

    let active = find_extension(&state, &project_name, &extension_name).await?;
    if record.project_id != project.id
        || record.extension != extension_name
        || record.generation != active.generation()
    {
        return Err(bad_request(
            "The OAuth extension changed during the test; start it again",
        ));
    }
    if !redirect_targets_rise_origin(&record.return_to, &state.public_url) {
        return Err(bad_request("return_to must be a Rise UI URL"));
    }

    let mut status = active.status.clone();
    let failure = if query.contains_key("error") {
        Some((
            authorization_failure_summary(
                query.get("error").map(String::as_str),
                query.get("error_description").map(String::as_str),
            ),
            "oauth_authorization_failed",
        ))
    } else {
        exchange(
            &state,
            &project_name,
            &extension_name,
            &active,
            &record,
            &query,
        )
        .await
        .err()
        .map(|summary| (summary, "oauth_token_exchange_failed"))
    };
    let mut return_to = Url::parse(&record.return_to)
        .map_err(|_| bad_request("return_to must be a Rise UI URL"))?;
    match failure {
        Some((summary, code)) => {
            info!(
                project = %project_name,
                extension = %extension_name,
                "OAuth test flow failed: {summary}"
            );
            status.error = Some(summary);
            status.auth_verified = false;
            return_to.query_pairs_mut().append_pair("error", code);
        }
        None => {
            info!(
                project = %project_name,
                extension = %extension_name,
                "OAuth test flow succeeded"
            );
            status.redirect_uri = Some(format!(
                "{}/oidc/{project_name}/{extension_name}/callback",
                state.public_url.trim_end_matches('/')
            ));
            status.configured_at = Some(Utc::now());
            status.auth_verified = true;
            status.error = None;
        }
    }
    write_status(&state, project.id, &extension_name, &status).await?;

    let mut response = Redirect::to(return_to.as_str()).into_response();
    let cookie = flow_cookie(&state, &project_name, &extension_name, "", 0);
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).map_err(|_| internal("Failed to complete the test"))?,
    );
    Ok(response)
}

/// Redeems the test code at the relay's token endpoint, in process. The tokens
/// are discarded; the failure summary is returned on error.
async fn exchange(
    state: &AppState,
    project: &str,
    extension: &str,
    active: &ActiveExtension,
    record: &TestFlowRecord,
    query: &HashMap<String, String>,
) -> Result<(), String> {
    let Some(code) = query.get("code") else {
        return Err("Authorization failed: the provider returned no code".to_string());
    };
    let public_url = Url::parse(&state.public_url)
        .map_err(|_| "Invalid Rise public URL configuration".to_string())?;
    let redirect_uri = test_callback_url(&public_url, project, extension);
    let client_id = active.status.rise_client_id.clone().unwrap_or_default();
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", redirect_uri.as_str()),
        ("code_verifier", record.verifier.as_str()),
        ("client_id", client_id.as_str()),
    ];
    if active.spec.client_type == ClientType::Confidential {
        let Some(secret) = active.status.rise_client_secret.as_deref() else {
            return Err("The extension has no Rise client secret".to_string());
        };
        form.push(("client_secret", secret));
    }
    let response = state.oauth_relay.token(project, extension, &form).await;
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let body = axum::body::to_bytes(response.into_body(), MAX_TOKEN_RESPONSE_BYTES)
        .await
        .unwrap_or_default();
    Err(token_exchange_failure_summary(
        status,
        &String::from_utf8_lossy(&body),
    ))
}

async fn write_status(
    state: &AppState,
    project_id: Uuid,
    extension: &str,
    status: &OAuthExtensionStatus,
) -> Result<(), (StatusCode, String)> {
    let value = serde_json::to_value(status).map_err(|_| internal("Failed to update status"))?;
    db_extensions::update_status(&state.db_pool, project_id, extension, &value)
        .await
        .map_err(|e| {
            error!("Failed to update OAuth extension status: {:?}", e);
            internal("Failed to update status")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PUBLIC_URL: &str = "https://rise.example.com";

    #[test]
    fn rise_origin_is_matched_by_parsed_origin() {
        for redirect in [
            "https://rise.example.com/",
            "https://rise.example.com/projects/demo#extensions",
            "https://rise.example.com:443/x",
            "https://RISE.EXAMPLE.COM/x",
            "https://rise.example.com./x",
        ] {
            assert!(
                redirect_targets_rise_origin(redirect, PUBLIC_URL),
                "{redirect} should be the Rise origin"
            );
        }
    }

    #[test]
    fn lookalike_origins_are_not_the_rise_origin() {
        for redirect in [
            // String-prefix lookalikes.
            "https://rise.example.com.evil.com/x",
            "https://rise.example.comevil.com/x",
            "https://rise.example.com-evil.com/x",
            // Userinfo makes the string start with the public URL.
            "https://rise.example.com@evil.com/x",
            "https://rise.example.com:pw@evil.com/x",
            // Different subdomain, parent domain, scheme, or port.
            "https://app.rise.example.com/x",
            "https://example.com/x",
            "http://rise.example.com/x",
            "https://rise.example.com:8443/x",
            // Not an HTTP(S) origin, or not a URL at all.
            "javascript://rise.example.com/%0Aalert(1)",
            "ftp://rise.example.com/x",
            "//rise.example.com/x",
            "/projects/demo",
            "",
        ] {
            assert!(
                !redirect_targets_rise_origin(redirect, PUBLIC_URL),
                "{redirect} must not be the Rise origin"
            );
        }
        assert!(!redirect_targets_rise_origin(
            "https://rise.example.com/x",
            "not a url"
        ));
    }

    #[test]
    fn rise_origin_with_explicit_port_and_http_scheme() {
        let public = "http://rise.localhost:3000";
        assert!(redirect_targets_rise_origin(
            "http://rise.localhost:3000/x",
            public
        ));
        assert!(!redirect_targets_rise_origin(
            "http://rise.localhost:3001/x",
            public
        ));
        assert!(!redirect_targets_rise_origin(
            "http://rise.localhost/x",
            public
        ));
        assert!(!redirect_targets_rise_origin(
            "https://rise.localhost:3000/x",
            public
        ));
    }

    #[test]
    fn token_exchange_failure_summary_keeps_only_bounded_oauth_error_fields() {
        let status = StatusCode::BAD_REQUEST;

        assert_eq!(
            token_exchange_failure_summary(
                status,
                r#"{"error":"invalid_grant","error_description":"code expired","trace":"secret"}"#
            ),
            "Token exchange failed with status 400 Bad Request: invalid_grant (code expired)"
        );
        // Non-JSON bodies and JSON without an `error` code contribute nothing.
        for body in [
            "<html><body>attacker text</body></html>",
            r#"{"message":"attacker text"}"#,
            r#"["error"]"#,
            "",
        ] {
            assert_eq!(
                token_exchange_failure_summary(status, body),
                "Token exchange failed with status 400 Bad Request"
            );
        }

        let long = "x".repeat(10_000);
        let summary = token_exchange_failure_summary(
            status,
            &json!({"error": long, "error_description": long}).to_string(),
        );
        assert!(
            summary.chars().count()
                <= "Token exchange failed with status 400 Bad Request: ".len()
                    + MAX_STORED_ERROR_CODE_CHARS
                    + MAX_STORED_ERROR_DESCRIPTION_CHARS
                    + 4
        );

        let summary =
            token_exchange_failure_summary(status, &json!({"error": "bad\nline\u{7}"}).to_string());
        assert!(!summary.chars().any(char::is_control));
    }

    #[test]
    fn authorization_failure_summary_keeps_bounded_error_fields() {
        assert_eq!(
            authorization_failure_summary(Some("access_denied"), Some("user said no")),
            "Authorization failed: access_denied (user said no)"
        );
        assert_eq!(
            authorization_failure_summary(Some("access_denied"), None),
            "Authorization failed: access_denied"
        );
        assert_eq!(
            authorization_failure_summary(None, Some("description without a code")),
            "Authorization failed"
        );
        let long = "x".repeat(10_000);
        let summary = authorization_failure_summary(Some(&long), Some(&format!("a\nb{long}")));
        assert!(
            summary.chars().count()
                <= "Authorization failed: ".len()
                    + MAX_STORED_ERROR_CODE_CHARS
                    + MAX_STORED_ERROR_DESCRIPTION_CHARS
                    + 4
        );
        assert!(!summary.chars().any(char::is_control));
    }

    #[test]
    fn the_flow_cookie_is_read_from_any_cookie_header() {
        let mut headers = HeaderMap::new();
        headers.append(header::COOKIE, "rise_jwt=a; other=b".parse().unwrap());
        assert_eq!(flow_cookie_value(&headers), None);
        headers.append(
            header::COOKIE,
            "x=1; rise_oauth_test=state".parse().unwrap(),
        );
        assert_eq!(flow_cookie_value(&headers).as_deref(), Some("state"));
        let mut empty = HeaderMap::new();
        empty.append(header::COOKIE, "rise_oauth_test=".parse().unwrap());
        assert_eq!(flow_cookie_value(&empty), None);
    }
}
