//! End-to-end flows through the mounted OAuth relay and the Rise UI test flow,
//! against an in-process mock upstream.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, Request, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;
use url::Url;

use super::models::OAuthExtensionSpec;
use super::oauth_client_secret_context;
use super::routes;
use crate::db::{extensions as db_extensions, projects as db_projects};
use crate::server::auth::browser_flow_tests::{fixture, rise_ui_session_headers, Fixture};

const PUBLIC_URL: &str = "https://rise.example.com";
const RISE_CLIENT_ID: &str = "demo-ext";
const RISE_CLIENT_SECRET: &str = "rise-client-secret";
const UPSTREAM_SECRET: &str = "upstream-secret";
const APP_REDIRECT: &str = "https://demo.apps.example.com/callback";
const PKCE_VERIFIER: &str = "a-pkce-verifier-that-is-long-enough-for-rfc-7636-0123456789";

// ---------------------------------------------------------------------------
// Mock upstream
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct UpstreamRequest {
    form: HashMap<String, String>,
    authorization: Option<String>,
}

#[derive(Default)]
struct MockState {
    issuer: String,
    oidc: bool,
    token_requests: Mutex<Vec<UpstreamRequest>>,
}

/// An OAuth provider on `127.0.0.1`: an OIDC one (discovery and JWKS), or a
/// GitHub-like OAuth2-only one without either.
struct MockUpstream {
    state: Arc<MockState>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for MockUpstream {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl MockUpstream {
    async fn start(oidc: bool) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(MockState {
            issuer,
            oidc,
            ..Default::default()
        });
        let app = Router::new()
            .route("/.well-known/openid-configuration", get(mock_discovery))
            .route("/jwks", get(mock_jwks))
            .route("/authorize", get(|| async { StatusCode::OK }))
            .route("/token", post(mock_token))
            .with_state(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { state, server }
    }

    fn issuer(&self) -> &str {
        &self.state.issuer
    }

    fn token_requests(&self) -> Vec<UpstreamRequest> {
        self.state.token_requests.lock().unwrap().clone()
    }
}

async fn mock_discovery(State(state): State<Arc<MockState>>) -> Response {
    if !state.oidc {
        return StatusCode::NOT_FOUND.into_response();
    }
    let issuer = &state.issuer;
    Json(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/authorize"),
        "token_endpoint": format!("{issuer}/token"),
        "jwks_uri": format!("{issuer}/jwks"),
        "userinfo_endpoint": format!("{issuer}/userinfo"),
        "id_token_signing_alg_values_supported": ["RS256"],
    }))
    .into_response()
}

async fn mock_jwks(State(state): State<Arc<MockState>>) -> Response {
    if !state.oidc {
        return StatusCode::NOT_FOUND.into_response();
    }
    Json(json!({"keys": []})).into_response()
}

async fn mock_token(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let form: HashMap<String, String> = url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect();
    state.token_requests.lock().unwrap().push(UpstreamRequest {
        form: form.clone(),
        authorization: headers
            .get(header::AUTHORIZATION)
            .map(|value| value.to_str().unwrap().to_owned()),
    });
    let invalid_grant = || {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "invalid_grant",
                "error_description": "the code is not valid",
                "trace": "internal stack trace",
            })),
        )
            .into_response()
    };
    match form.get("grant_type").map(String::as_str) {
        Some("authorization_code") if form.get("code").map(String::as_str) == Some("bad-code") => {
            invalid_grant()
        }
        Some("authorization_code") => Json(json!({
            "access_token": "upstream-access",
            "token_type": "Bearer",
            "expires_in": 3600,
            "refresh_token": "upstream-refresh",
        }))
        .into_response(),
        Some("refresh_token")
            if form.get("refresh_token").map(String::as_str) == Some("upstream-refresh") =>
        {
            Json(json!({
                "access_token": "upstream-access-refreshed",
                "token_type": "Bearer",
                "expires_in": 3600,
                "refresh_token": "upstream-refresh",
            }))
            .into_response()
        }
        _ => invalid_grant(),
    }
}

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Client {
    Confidential,
    Public,
}

/// Creates OAuth extension `ext` of project `demo` against `upstream`, with Rise
/// client credentials as `on_spec_updated` would set them.
async fn create_extension(
    fixture: &Fixture,
    upstream: &MockUpstream,
    client: Client,
    explicit_endpoints: bool,
) {
    let project = db_projects::find_by_name(&fixture.state.db_pool, "demo")
        .await
        .unwrap()
        .unwrap();
    let issuer = upstream.issuer();
    let mut spec = json!({
        "provider_name": "Mock",
        "client_id": "upstream-client",
        "issuer_url": issuer,
        "scopes": ["openid", "email"],
    });
    if explicit_endpoints {
        spec["authorization_endpoint"] = json!(format!("{issuer}/authorize"));
        spec["token_endpoint"] = json!(format!("{issuer}/token"));
    }
    if client == Client::Public {
        spec["client_type"] = json!("public");
    }
    let mut typed: OAuthExtensionSpec = serde_json::from_value(spec.clone()).unwrap();
    // Both discovery and explicit endpoints resolve to the same token endpoint.
    let token_endpoint = format!("{issuer}/token");
    let context = oauth_client_secret_context(project.id, "ext", &typed, &token_endpoint).unwrap();
    typed.client_secret_encrypted = Some(
        fixture
            .state
            .encryption_provider
            .as_ref()
            .unwrap()
            .encrypt(UPSTREAM_SECRET, &context)
            .await
            .unwrap(),
    );
    let spec = serde_json::to_value(&typed).unwrap();
    db_extensions::create(&fixture.state.db_pool, project.id, "ext", "oauth", &spec)
        .await
        .unwrap();
    let mut status = json!({"rise_client_id": RISE_CLIENT_ID});
    if client == Client::Confidential {
        status["rise_client_secret"] = json!(RISE_CLIENT_SECRET);
    }
    db_extensions::update_status(&fixture.state.db_pool, project.id, "ext", &status)
        .await
        .unwrap();
}

async fn project_id(fixture: &Fixture) -> uuid::Uuid {
    db_projects::find_by_name(&fixture.state.db_pool, "demo")
        .await
        .unwrap()
        .unwrap()
        .id
}

async fn extension_status(fixture: &Fixture) -> Value {
    db_extensions::find_by_project_and_name(
        &fixture.state.db_pool,
        project_id(fixture).await,
        "ext",
    )
    .await
    .unwrap()
    .unwrap()
    .status
}

/// Rise's router as `src/server/mod.rs` mounts the relay and the test flow.
fn app(fixture: &Fixture) -> Router {
    Router::new()
        .nest("/api/v1", routes::test_flow_routes())
        .merge(routes::relay_routes(&fixture.state))
        .with_state(fixture.state.clone())
}

async fn send(app: &Router, request: Request<Body>) -> Response {
    app.clone().oneshot(request).await.unwrap()
}

async fn get_path(app: &Router, uri: &str, headers: &HeaderMap) -> Response {
    let mut request = Request::get(uri).body(Body::empty()).unwrap();
    *request.headers_mut() = headers.clone();
    send(app, request).await
}

async fn token(app: &Router, form: &[(&str, &str)]) -> (StatusCode, Value) {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form)
        .finish();
    let response = send(
        app,
        Request::post("/oidc/demo/ext/token")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap(),
    )
    .await;
    let status = response.status();
    (status, body_json(response).await)
}

async fn body_json(response: Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

fn location(response: &Response) -> Url {
    Url::parse(response.headers()[header::LOCATION].to_str().unwrap()).unwrap()
}

fn query(url: &Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}

fn pkce_challenge(verifier: &str) -> String {
    use base64::Engine;
    use sha2::{Digest, Sha256};
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn authorize_uri(params: &[(&str, &str)]) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(params)
        .finish();
    format!("/oidc/demo/ext/authorize?{query}")
}

/// Plays the browser and the upstream for one sign-in: `/authorize` at the relay,
/// the provider's redirect back to the relay callback (with `upstream_params`,
/// e.g. a code), and the relay's redirect to the app. Returns the URL the relay
/// sends the browser to.
async fn sign_in(
    app: &Router,
    upstream: &MockUpstream,
    authorize_params: &[(&str, &str)],
    upstream_params: &[(&str, &str)],
    headers: &HeaderMap,
) -> Url {
    let response = get_path(app, &authorize_uri(authorize_params), headers).await;
    assert_eq!(response.status(), StatusCode::FOUND, "authorize");
    let to_upstream = location(&response);
    assert!(
        to_upstream
            .as_str()
            .starts_with(&format!("{}/authorize?", upstream.issuer())),
        "{to_upstream}"
    );
    let callback = query(&to_upstream, "redirect_uri").unwrap();
    assert_eq!(callback, format!("{PUBLIC_URL}/oidc/demo/ext/callback"));
    let mut back = Url::parse(&callback).unwrap();
    back.query_pairs_mut()
        .extend_pairs(upstream_params)
        .append_pair("state", &query(&to_upstream, "state").unwrap());
    let path = format!("{}?{}", back.path(), back.query().unwrap());
    let response = get_path(app, &path, headers).await;
    assert_eq!(response.status(), StatusCode::FOUND, "callback");
    location(&response)
}

/// A relay authorization code for the confidential/public app flow.
async fn app_code(app: &Router, upstream: &MockUpstream, pkce: bool) -> String {
    let challenge = pkce_challenge(PKCE_VERIFIER);
    let mut params = vec![("redirect_uri", APP_REDIRECT), ("state", "app-state")];
    if pkce {
        params.push(("code_challenge", &challenge));
        params.push(("code_challenge_method", "S256"));
    }
    let to_app = sign_in(
        app,
        upstream,
        &params,
        &[("code", "upstream-code")],
        &HeaderMap::new(),
    )
    .await;
    assert!(to_app.as_str().starts_with(APP_REDIRECT), "{to_app}");
    assert_eq!(query(&to_app, "state").as_deref(), Some("app-state"));
    query(&to_app, "code").unwrap()
}

// ---------------------------------------------------------------------------
// End-user flows
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn confidential_client_exchanges_and_refreshes_with_its_secret(pool: PgPool) {
    let fixture = fixture(pool).await;
    let upstream = MockUpstream::start(true).await;
    create_extension(&fixture, &upstream, Client::Confidential, false).await;
    let app = app(&fixture);
    let before = extension_status(&fixture).await;

    let code = app_code(&app, &upstream, false).await;
    // The relay redeemed the upstream code at the discovered token endpoint
    // with the upstream client credentials.
    let requests = upstream.token_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].form["code"], "upstream-code");
    assert_eq!(requests[0].form["client_id"], "upstream-client");
    assert_eq!(requests[0].form["client_secret"], UPSTREAM_SECRET);
    assert_eq!(requests[0].authorization, None, "client_secret_post");
    assert_eq!(
        requests[0].form["redirect_uri"],
        format!("{PUBLIC_URL}/oidc/demo/ext/callback")
    );

    let exchange = |secret: Option<&'static str>, code: String| {
        let app = app.clone();
        async move {
            let mut form = vec![
                ("grant_type", "authorization_code".to_owned()),
                ("code", code),
                ("redirect_uri", APP_REDIRECT.to_owned()),
                ("client_id", RISE_CLIENT_ID.to_owned()),
            ];
            if let Some(secret) = secret {
                form.push(("client_secret", secret.to_owned()));
            }
            let form: Vec<(&str, &str)> = form.iter().map(|(k, v)| (*k, v.as_str())).collect();
            token(&app, &form).await
        }
    };

    // Without the secret, or with a wrong one, the client is not authenticated.
    let (status, body) = exchange(None, code.clone()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "invalid_client");
    let (status, body) = exchange(Some("wrong"), code.clone()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "invalid_client");

    // A different redirect_uri than at authorize is refused.
    let (status, body) = token(
        &app,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", "https://demo.apps.example.com/other"),
            ("client_id", RISE_CLIENT_ID),
            ("client_secret", RISE_CLIENT_SECRET),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_grant");

    let (status, tokens) = exchange(Some(RISE_CLIENT_SECRET), code.clone()).await;
    assert_eq!(status, StatusCode::OK, "{tokens}");
    assert_eq!(tokens["access_token"], "upstream-access");
    let handle = tokens["refresh_token"].as_str().unwrap().to_owned();
    assert!(handle.starts_with("rrt1."), "{handle}");

    // A code is single use.
    let (status, body) = exchange(Some(RISE_CLIENT_SECRET), code).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_grant");

    // A code that is not a sealed relay code (64 hex characters) is refused.
    let (status, body) = exchange(Some(RISE_CLIENT_SECRET), "ab".repeat(32)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_grant");

    // Refresh needs the secret too.
    let (status, body) = token(
        &app,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", &handle),
            ("client_id", RISE_CLIENT_ID),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "invalid_client");

    // The sealed handle refreshes with the upstream refresh token it wraps.
    let refresh = |refresh_token: String| {
        let app = app.clone();
        async move {
            token(
                &app,
                &[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", &refresh_token),
                    ("client_id", RISE_CLIENT_ID),
                    ("client_secret", RISE_CLIENT_SECRET),
                ],
            )
            .await
        }
    };
    let (status, refreshed) = refresh(handle.clone()).await;
    assert_eq!(status, StatusCode::OK, "{refreshed}");
    assert_eq!(refreshed["access_token"], "upstream-access-refreshed");
    assert!(refreshed["refresh_token"]
        .as_str()
        .unwrap()
        .starts_with("rrt1."));
    let requests = upstream.token_requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].form["refresh_token"], "upstream-refresh");
    assert_eq!(requests[1].form["client_secret"], UPSTREAM_SECRET);

    // A sealed handle is reusable.
    let (status, _) = refresh(handle).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(upstream.token_requests().len(), 3);

    // A raw upstream refresh token is refused and never forwarded.
    let (status, body) = refresh("upstream-refresh".to_owned()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_grant");
    assert_eq!(upstream.token_requests().len(), 3);

    // End-user flows never touch the extension's status.
    assert_eq!(before, extension_status(&fixture).await);
}

#[sqlx::test]
async fn public_client_requires_pkce_has_no_secret_and_rotates_handles(pool: PgPool) {
    let fixture = fixture(pool).await;
    let upstream = MockUpstream::start(true).await;
    create_extension(&fixture, &upstream, Client::Public, false).await;
    let app = app(&fixture);

    // PKCE is required at authorize.
    let response = get_path(
        &app,
        &authorize_uri(&[("redirect_uri", APP_REDIRECT)]),
        &HeaderMap::new(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let code = app_code(&app, &upstream, true).await;
    let exchange = |extra: Vec<(&'static str, &'static str)>, code: String| {
        let app = app.clone();
        async move {
            let mut form = vec![
                ("grant_type", "authorization_code"),
                ("code", code.as_str()),
                ("redirect_uri", APP_REDIRECT),
                ("client_id", RISE_CLIENT_ID),
            ];
            form.extend(extra);
            token(&app, &form).await
        }
    };

    // Presenting a client secret is refused: a public client has none.
    let (status, body) = exchange(
        vec![
            ("client_secret", RISE_CLIENT_SECRET),
            ("code_verifier", PKCE_VERIFIER),
        ],
        code.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "invalid_client");

    let (status, body) = exchange(
        vec![(
            "code_verifier",
            "wrong-verifier-wrong-verifier-wrong-verifier-00",
        )],
        code.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_grant");

    let code = app_code(&app, &upstream, true).await;
    let (status, tokens) = exchange(vec![("code_verifier", PKCE_VERIFIER)], code).await;
    assert_eq!(status, StatusCode::OK, "{tokens}");
    let first = tokens["refresh_token"].as_str().unwrap().to_owned();
    assert!(first.starts_with("rrt"), "{first}");

    let refresh = |handle: String| {
        let app = app.clone();
        async move {
            token(
                &app,
                &[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", &handle),
                    ("client_id", RISE_CLIENT_ID),
                ],
            )
            .await
        }
    };
    let (status, rotated) = refresh(first.clone()).await;
    assert_eq!(status, StatusCode::OK, "{rotated}");
    let second = rotated["refresh_token"].as_str().unwrap().to_owned();
    assert_ne!(first, second, "every refresh returns a successor handle");
    let upstream_refreshes = upstream.token_requests().len();

    // A duplicate within the grace period gets the same successor without a
    // second upstream refresh.
    let (status, duplicate) = refresh(first).await;
    assert_eq!(status, StatusCode::OK, "{duplicate}");
    assert_eq!(duplicate["refresh_token"], second.as_str());
    assert_eq!(upstream.token_requests().len(), upstream_refreshes);

    // The successor works in turn.
    let (status, third) = refresh(second.clone()).await;
    assert_eq!(status, StatusCode::OK, "{third}");
    assert_ne!(third["refresh_token"], second.as_str());
}

#[sqlx::test]
async fn redirect_uri_is_required_and_limited_to_deployments_and_loopback(pool: PgPool) {
    let fixture = fixture(pool).await;
    let upstream = MockUpstream::start(true).await;
    create_extension(&fixture, &upstream, Client::Confidential, false).await;
    let app = app(&fixture);

    let status = |params: Vec<(&'static str, &'static str)>| {
        let app = app.clone();
        async move {
            get_path(&app, &authorize_uri(&params), &HeaderMap::new())
                .await
                .status()
        }
    };
    assert_eq!(status(vec![]).await, StatusCode::BAD_REQUEST);
    for allowed in [
        APP_REDIRECT,
        "https://custom.example.com/oauth/callback",
        "http://127.0.0.1:5173/cb",
        "http://[::1]:8080/cb",
        "http://localhost:3000/cb",
        "http://foo.localhost:3000/",
    ] {
        assert_eq!(
            status(vec![("redirect_uri", allowed)]).await,
            StatusCode::FOUND,
            "{allowed}"
        );
    }
    for refused in [
        // The Rise origin is not an app redirect target.
        "https://rise.example.com/projects/demo",
        "https://rise.example.com/api/v1/projects/demo/extensions/other/oauth/test/callback",
        // http deployment origins only on an http Rise.
        "http://demo.apps.example.com/callback",
        "https://evil.example.com/callback",
        "https://demo.apps.example.com.evil.com/callback",
        "javascript://localhost/%0Aalert(1)",
    ] {
        assert_eq!(
            status(vec![("redirect_uri", refused)]).await,
            StatusCode::BAD_REQUEST,
            "{refused}"
        );
    }
}

#[sqlx::test]
async fn deleted_and_foreign_extensions_are_not_served(pool: PgPool) {
    let fixture = fixture(pool).await;
    let upstream = MockUpstream::start(true).await;
    create_extension(&fixture, &upstream, Client::Confidential, false).await;
    let app = app(&fixture);
    let project = project_id(&fixture).await;

    // A handle issued before the extension is deleted and recreated.
    let code = app_code(&app, &upstream, false).await;
    let (_, tokens) = token(
        &app,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", APP_REDIRECT),
            ("client_id", RISE_CLIENT_ID),
            ("client_secret", RISE_CLIENT_SECRET),
        ],
    )
    .await;
    let handle = tokens["refresh_token"].as_str().unwrap().to_owned();

    db_extensions::mark_deleted(&fixture.state.db_pool, project, "ext")
        .await
        .unwrap();
    for path in [
        "/oidc/demo/ext/.well-known/openid-configuration",
        "/oidc/demo/ext/jwks",
        "/oidc/demo/ext/callback?code=x&state=y",
        &authorize_uri(&[("redirect_uri", APP_REDIRECT)]),
    ] {
        assert_eq!(
            get_path(&app, path, &HeaderMap::new()).await.status(),
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
    let (status, _) = token(&app, &[("grant_type", "refresh_token")]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A recreated extension does not accept its predecessor's handles.
    db_extensions::delete_permanently(&fixture.state.db_pool, project, "ext")
        .await
        .unwrap();
    create_extension(&fixture, &upstream, Client::Confidential, false).await;
    let requests = upstream.token_requests().len();
    let (status, body) = token(
        &app,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", &handle),
            ("client_id", RISE_CLIENT_ID),
            ("client_secret", RISE_CLIENT_SECRET),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_grant");
    assert_eq!(upstream.token_requests().len(), requests);

    // Unknown projects and extensions, and other extension types, are 404s.
    db_extensions::create(
        &fixture.state.db_pool,
        project,
        "bucket",
        "aws-s3-provisioner",
        &json!({}),
    )
    .await
    .unwrap();
    for path in [
        "/oidc/demo/bucket/.well-known/openid-configuration",
        "/oidc/demo/missing/.well-known/openid-configuration",
        "/oidc/missing/ext/.well-known/openid-configuration",
        "/oidc/demo/ext/unknown",
        "/oidc/demo/%2E%2E/.well-known/openid-configuration",
    ] {
        assert_eq!(
            get_path(&app, path, &HeaderMap::new()).await.status(),
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
}

#[sqlx::test]
async fn discovery_names_the_upstream_issuer_and_the_relay_endpoints(pool: PgPool) {
    let fixture = fixture(pool).await;
    let upstream = MockUpstream::start(true).await;
    create_extension(&fixture, &upstream, Client::Confidential, false).await;
    let app = app(&fixture);

    let response = get_path(
        &app,
        "/oidc/demo/ext/.well-known/openid-configuration",
        &HeaderMap::new(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let doc = body_json(response).await;
    assert_eq!(doc["issuer"], upstream.issuer());
    assert_eq!(
        doc["authorization_endpoint"],
        format!("{PUBLIC_URL}/oidc/demo/ext/authorize")
    );
    assert_eq!(
        doc["token_endpoint"],
        format!("{PUBLIC_URL}/oidc/demo/ext/token")
    );
    assert_eq!(doc["jwks_uri"], format!("{}/jwks", upstream.issuer()));
    assert_eq!(
        doc["id_token_signing_alg_values_supported"],
        json!(["RS256"])
    );

    let response = get_path(&app, "/oidc/demo/ext/jwks", &HeaderMap::new()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await, json!({"keys": []}));
}

#[sqlx::test]
async fn oauth2_only_upstream_serves_discovery_without_a_jwks(pool: PgPool) {
    let fixture = fixture(pool).await;
    let upstream = MockUpstream::start(false).await;
    create_extension(&fixture, &upstream, Client::Confidential, true).await;
    let app = app(&fixture);

    let response = get_path(
        &app,
        "/oidc/demo/ext/.well-known/openid-configuration",
        &HeaderMap::new(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let doc = body_json(response).await;
    assert_eq!(doc["issuer"], upstream.issuer());
    assert_eq!(
        doc["token_endpoint"],
        format!("{PUBLIC_URL}/oidc/demo/ext/token")
    );
    assert!(doc.get("jwks_uri").is_none(), "{doc}");
    assert_eq!(
        get_path(&app, "/oidc/demo/ext/jwks", &HeaderMap::new())
            .await
            .status(),
        StatusCode::NOT_FOUND
    );

    // The code flow works without discovery.
    let code = app_code(&app, &upstream, false).await;
    let (status, tokens) = token(
        &app,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", APP_REDIRECT),
            ("client_id", RISE_CLIENT_ID),
            ("client_secret", RISE_CLIENT_SECRET),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{tokens}");
}

#[sqlx::test]
async fn the_rate_limit_covers_authorize_callback_and_token(pool: PgPool) {
    let fixture = fixture(pool).await;
    let upstream = MockUpstream::start(true).await;
    create_extension(&fixture, &upstream, Client::Confidential, false).await;
    let settings: crate::server::settings::OAuthRateLimitSettings =
        serde_json::from_value(json!({"per_project_max": 3})).unwrap();
    let limiter = Arc::new(crate::server::rate_limit::OAuthRateLimiter::new(&settings));
    let app: Router =
        Router::new().route_service("/oidc/{*path}", fixture.state.oauth_relay.service(limiter));

    // Discovery and JWKS are not counted.
    for _ in 0..5 {
        assert_eq!(
            get_path(&app, "/oidc/demo/ext/jwks", &HeaderMap::new())
                .await
                .status(),
            StatusCode::OK
        );
    }
    assert_eq!(
        get_path(&app, &authorize_uri(&[]), &HeaderMap::new())
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        get_path(&app, "/oidc/demo/ext/callback", &HeaderMap::new())
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    let (status, _) = token(&app, &[("grant_type", "refresh_token")]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, body) = token(&app, &[("grant_type", "refresh_token")]).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["error"], "rate_limit_exceeded");
    let response = get_path(&app, &authorize_uri(&[]), &HeaderMap::new()).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(response.headers().contains_key(header::RETRY_AFTER));

    // In-process exchanges are not rate limited.
    let response = fixture
        .state
        .oauth_relay
        .token("demo", "ext", &[("grant_type", "refresh_token")])
        .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// The Rise UI test flow
// ---------------------------------------------------------------------------

const RETURN_TO: &str = "https://rise.example.com/projects/demo#extensions";

fn start_uri(return_to: &str) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("return_to", return_to)
        .finish();
    format!("/api/v1/projects/demo/extensions/ext/oauth/test?{query}")
}

/// The `rise_oauth_test` cookie a test-flow response sets.
fn flow_cookie(response: &Response) -> Option<String> {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|value| value.to_str().unwrap())
        .find_map(|cookie| cookie.strip_prefix("rise_oauth_test="))
        .map(|cookie| cookie.split(';').next().unwrap().to_owned())
}

/// `headers` plus the test-flow cookie.
fn with_flow_cookie(headers: &HeaderMap, cookie: &str) -> HeaderMap {
    let mut headers = headers.clone();
    let session = headers
        .get(header::COOKIE)
        .map(|value| format!("{}; ", value.to_str().unwrap()))
        .unwrap_or_default();
    headers.insert(
        header::COOKIE,
        format!("{session}rise_oauth_test={cookie}")
            .parse()
            .unwrap(),
    );
    headers
}

struct StartedTest {
    cookie: String,
    /// The relay authorize URL the test flow redirected to.
    authorize: Url,
}

async fn start_test(app: &Router, headers: &HeaderMap) -> Result<StartedTest, Response> {
    let response = get_path(app, &start_uri(RETURN_TO), headers).await;
    if response.status() != StatusCode::SEE_OTHER {
        return Err(response);
    }
    let cookie = flow_cookie(&response).expect("the test flow cookie");
    let set_cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
    assert!(set_cookie.contains("HttpOnly"), "{set_cookie}");
    assert!(set_cookie.contains("SameSite=Lax"), "{set_cookie}");
    assert!(set_cookie.contains("Secure"), "{set_cookie}");
    assert!(
        set_cookie.contains("Path=/api/v1/projects/demo/extensions/ext/oauth/test;"),
        "{set_cookie}"
    );
    Ok(StartedTest {
        cookie,
        authorize: location(&response),
    })
}

/// Runs a started test through the relay and the mock upstream and returns the
/// path and query of the test callback the relay redirects to.
async fn through_relay(
    app: &Router,
    upstream: &MockUpstream,
    started: &StartedTest,
    upstream_params: &[(&str, &str)],
) -> String {
    assert!(started
        .authorize
        .as_str()
        .starts_with(&format!("{PUBLIC_URL}/oidc/demo/ext/authorize?")));
    let params: Vec<(String, String)> = started.authorize.query_pairs().into_owned().collect();
    let params: Vec<(&str, &str)> = params
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let to_callback = sign_in(app, upstream, &params, upstream_params, &HeaderMap::new()).await;
    assert!(
        to_callback.as_str().starts_with(&format!(
            "{PUBLIC_URL}/api/v1/projects/demo/extensions/ext/oauth/test/callback?"
        )),
        "{to_callback}"
    );
    format!("{}?{}", to_callback.path(), to_callback.query().unwrap())
}

/// The session cookie of `headers`, re-signed as another kind of session for
/// the same user.
fn resigned_session(
    fixture: &Fixture,
    headers: &HeaderMap,
    client: rise_backend_auth::SessionClient,
    authorization_details: Option<Vec<Value>>,
) -> HeaderMap {
    let token = crate::server::auth::cookie_helpers::extract_rise_jwt_cookie(headers).unwrap();
    let claims = fixture
        .state
        .jwt_signer
        .verify_user_jwt(&token, PUBLIC_URL)
        .unwrap();
    let resigned = fixture
        .state
        .jwt_signer
        .sign_user_jwt(
            &json!({"sub": claims.sub, "email": claims.email}),
            &rise_backend_auth::SessionUser {
                subject: claims.sub.clone(),
                rise_uid: claims.rise_uid.unwrap(),
                identity_uid: claims.rise_identity_uid.unwrap(),
                client,
                authorization_details,
            },
            None,
            PUBLIC_URL,
            None,
        )
        .unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        header::COOKIE,
        format!("rise_jwt={resigned}").parse().unwrap(),
    );
    headers
}

#[sqlx::test]
async fn the_test_flow_marks_the_extension_configured(pool: PgPool) {
    let fixture = fixture(pool).await;
    let upstream = MockUpstream::start(true).await;
    create_extension(&fixture, &upstream, Client::Confidential, false).await;
    let app = app(&fixture);
    let session = rise_ui_session_headers(&fixture).await;

    let started = start_test(&app, &session).await.unwrap();
    // The test always uses PKCE and the exact Rise test callback.
    assert_eq!(
        query(&started.authorize, "redirect_uri").unwrap(),
        format!("{PUBLIC_URL}/api/v1/projects/demo/extensions/ext/oauth/test/callback")
    );
    assert_eq!(
        query(&started.authorize, "code_challenge_method").as_deref(),
        Some("S256")
    );
    let callback = through_relay(&app, &upstream, &started, &[("code", "upstream-code")]).await;

    let response = get_path(
        &app,
        &callback,
        &with_flow_cookie(&session, &started.cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&response).as_str(), RETURN_TO);
    // The cookie is cleared.
    assert_eq!(flow_cookie(&response).as_deref(), Some(""));

    let status = extension_status(&fixture).await;
    assert_eq!(status["auth_verified"], true, "{status}");
    assert!(status.get("error").is_none(), "{status}");
    assert!(status["configured_at"].is_string());
    assert_eq!(
        status["redirect_uri"],
        format!("{PUBLIC_URL}/oidc/demo/ext/callback")
    );
    // Credentials survive.
    assert_eq!(status["rise_client_secret"], RISE_CLIENT_SECRET);

    // The callback is single use.
    let response = get_path(
        &app,
        &callback,
        &with_flow_cookie(&session, &started.cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[sqlx::test]
async fn the_test_flow_works_for_a_public_client(pool: PgPool) {
    let fixture = fixture(pool).await;
    let upstream = MockUpstream::start(true).await;
    create_extension(&fixture, &upstream, Client::Public, false).await;
    let app = app(&fixture);
    let session = rise_ui_session_headers(&fixture).await;

    let started = start_test(&app, &session).await.unwrap();
    let callback = through_relay(&app, &upstream, &started, &[("code", "upstream-code")]).await;
    let response = get_path(
        &app,
        &callback,
        &with_flow_cookie(&session, &started.cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(extension_status(&fixture).await["auth_verified"], true);
}

#[sqlx::test]
async fn the_test_flow_stores_only_bounded_upstream_errors(pool: PgPool) {
    let fixture = fixture(pool).await;
    let upstream = MockUpstream::start(true).await;
    create_extension(&fixture, &upstream, Client::Confidential, false).await;
    let app = app(&fixture);
    let session = rise_ui_session_headers(&fixture).await;

    // The provider refuses the authorization.
    let long = "x".repeat(10_000);
    let started = start_test(&app, &session).await.unwrap();
    let callback = through_relay(
        &app,
        &upstream,
        &started,
        &[
            ("error", "access_denied"),
            ("error_description", &format!("denied\n{long}")),
            ("extra", "attacker text"),
        ],
    )
    .await;
    let response = get_path(
        &app,
        &callback,
        &with_flow_cookie(&session, &started.cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        query(&location(&response), "error").as_deref(),
        Some("oauth_authorization_failed")
    );
    let status = extension_status(&fixture).await;
    assert_eq!(status["auth_verified"], false);
    let error = status["error"].as_str().unwrap();
    assert!(
        error.starts_with("Authorization failed: access_denied (denied "),
        "{error}"
    );
    assert!(error.chars().count() <= 40 + 100 + 200, "{error}");
    assert!(!error.contains("attacker"));
    assert!(!error.chars().any(char::is_control));

    // The provider rejects the code: only its error fields are kept.
    let started = start_test(&app, &session).await.unwrap();
    let callback = through_relay(&app, &upstream, &started, &[("code", "bad-code")]).await;
    let response = get_path(
        &app,
        &callback,
        &with_flow_cookie(&session, &started.cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        query(&location(&response), "error").as_deref(),
        Some("oauth_token_exchange_failed")
    );
    let status = extension_status(&fixture).await;
    assert_eq!(status["auth_verified"], false);
    assert_eq!(
        status["error"],
        "Token exchange failed with status 400 Bad Request: invalid_grant (the code is not valid)"
    );
}

#[sqlx::test]
async fn anonymous_and_unauthorized_users_cannot_start_the_test_flow(pool: PgPool) {
    let mut fixture = fixture(pool).await;
    let upstream = MockUpstream::start(true).await;
    create_extension(&fixture, &upstream, Client::Confidential, false).await;
    let session = rise_ui_session_headers(&fixture).await;

    let refused = |app: Router, headers: HeaderMap| async move {
        let response = start_test(&app, &headers).await.err().expect("refused");
        assert!(flow_cookie(&response).is_none());
        response.status()
    };

    let app_ = app(&fixture);
    assert_eq!(
        refused(app_.clone(), HeaderMap::new()).await,
        StatusCode::UNAUTHORIZED
    );
    let mut forged = HeaderMap::new();
    forged.insert(header::COOKIE, "rise_jwt=not.a.jwt".parse().unwrap());
    assert_eq!(
        refused(app_.clone(), forged).await,
        StatusCode::UNAUTHORIZED
    );

    // Only an unscoped browser session may start it.
    use rise_backend_auth::SessionClient;
    let browser = resigned_session(&fixture, &session, SessionClient::Browser, None);
    assert!(start_test(&app_, &browser).await.is_ok());
    let cli = resigned_session(&fixture, &session, SessionClient::Cli, None);
    assert_eq!(refused(app_.clone(), cli).await, StatusCode::FORBIDDEN);
    let scoped = resigned_session(
        &fixture,
        &session,
        SessionClient::Cli,
        Some(vec![json!({"type": "rise.dev/rbac"})]),
    );
    assert_eq!(refused(app_.clone(), scoped).await, StatusCode::FORBIDDEN);

    // return_to must be the Rise UI.
    for return_to in [
        "https://evil.example.com/",
        "https://rise.example.com.evil.com/",
        APP_REDIRECT,
    ] {
        let response = get_path(&app_, &start_uri(return_to), &session).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{return_to}");
    }

    // Without the admin bypass the user is neither owner nor member.
    fixture.state.admin_users = Arc::new(Vec::new());
    assert_eq!(
        refused(app(&fixture), session.clone()).await,
        StatusCode::FORBIDDEN
    );

    // Nor without platform access.
    use crate::server::settings::PlatformAccessPolicy;
    let mut auth_settings = (*fixture.state.auth_settings).clone();
    auth_settings.platform_access.policy = PlatformAccessPolicy::Restrictive;
    fixture.state.auth_settings = Arc::new(auth_settings);
    let response = start_test(&app(&fixture), &session).await.err().unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = axum::body::to_bytes(response.into_body(), 1 << 16)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&body).contains("platform"));
}

#[sqlx::test]
async fn an_inactive_user_cannot_start_the_test_flow(pool: PgPool) {
    let fixture = fixture(pool).await;
    let upstream = MockUpstream::start(true).await;
    create_extension(&fixture, &upstream, Client::Confidential, false).await;
    let session = rise_ui_session_headers(&fixture).await;
    let token = crate::server::auth::cookie_helpers::extract_rise_jwt_cookie(&session).unwrap();
    let uid = fixture
        .state
        .jwt_signer
        .verify_user_jwt(&token, PUBLIC_URL)
        .unwrap()
        .rise_uid
        .unwrap();
    let row = fixture
        .state
        .resource_store
        .get(uid)
        .await
        .unwrap()
        .unwrap();
    let mut spec = row.spec.clone();
    spec["active"] = json!(false);
    fixture
        .state
        .resource_store
        .update(
            uid,
            rise_resource_api::UpdateResourceParams {
                api_version: None,
                revision: row.revision,
                labels: row.labels.clone(),
                annotations: Default::default(),
                finalizers: row.finalizers.clone(),
                owner_references: Vec::new(),
                spec,
                validator: None,
            },
        )
        .await
        .unwrap();

    let response = start_test(&app(&fixture), &session).await.err().unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn the_test_callback_is_bound_to_the_initiating_browser_and_user(pool: PgPool) {
    let fixture = fixture(pool).await;
    let upstream = MockUpstream::start(true).await;
    create_extension(&fixture, &upstream, Client::Confidential, false).await;
    let app = app(&fixture);
    let session = rise_ui_session_headers(&fixture).await;
    let before = extension_status(&fixture).await;

    let started = start_test(&app, &session).await.unwrap();
    let callback = through_relay(&app, &upstream, &started, &[("code", "upstream-code")]).await;

    // Without a session.
    let response = get_path(
        &app,
        &callback,
        &with_flow_cookie(&HeaderMap::new(), &started.cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    // Without the flow cookie: another browser of the same user.
    let response = get_path(&app, &callback, &session).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    // With another flow's cookie.
    let other = start_test(&app, &session).await.unwrap();
    let response = get_path(&app, &callback, &with_flow_cookie(&session, &other.cookie)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // A flow started by another user cannot be completed by this one.
    let foreign_state = "foreign-state";
    let record = json!({
        "user_id": uuid::Uuid::new_v4(),
        "project_id": project_id(&fixture).await,
        "extension": "ext",
        "generation": "x",
        "verifier": "v",
        "return_to": RETURN_TO,
    });
    let key = {
        use base64::Engine;
        use sha2::{Digest, Sha256};
        format!(
            "rise:oauth-test:{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(Sha256::digest(foreign_state.as_bytes()))
        )
    };
    assert!(fixture
        .state
        .oauth_relay
        .store()
        .put_if_absent(
            &key,
            record.to_string().as_bytes(),
            std::time::Duration::from_secs(60)
        )
        .await
        .unwrap());
    let response = get_path(
        &app,
        &format!(
            "/api/v1/projects/demo/extensions/ext/oauth/test/callback?code=c&state={foreign_state}"
        ),
        &with_flow_cookie(&session, foreign_state),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // None of the refusals touched the status, and the flow still completes
    // in the browser that started it.
    assert_eq!(before, extension_status(&fixture).await);
    let response = get_path(
        &app,
        &callback,
        &with_flow_cookie(&session, &started.cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(extension_status(&fixture).await["auth_verified"], true);
}

#[sqlx::test]
async fn the_test_flow_refuses_an_extension_recreated_meanwhile(pool: PgPool) {
    let fixture = fixture(pool).await;
    let upstream = MockUpstream::start(true).await;
    create_extension(&fixture, &upstream, Client::Confidential, false).await;
    let app = app(&fixture);
    let session = rise_ui_session_headers(&fixture).await;

    let started = start_test(&app, &session).await.unwrap();
    let callback = through_relay(&app, &upstream, &started, &[("code", "upstream-code")]).await;
    let project = project_id(&fixture).await;
    db_extensions::delete_permanently(&fixture.state.db_pool, project, "ext")
        .await
        .unwrap();
    create_extension(&fixture, &upstream, Client::Confidential, false).await;
    let before = extension_status(&fixture).await;

    let response = get_path(
        &app,
        &callback,
        &with_flow_cookie(&session, &started.cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(before, extension_status(&fixture).await);
}

// ---------------------------------------------------------------------------
// Production store and sealer
// ---------------------------------------------------------------------------

/// Replaces the fixture's in-process store and sealer with the production
/// `PgTransientStore` and `RiseSealer` over `LocalEncryptionProvider`.
fn use_production_store_and_sealer(fixture: &mut Fixture) {
    use super::relay::{sealer::RiseSealer, store::PgTransientStore, OAuthRelay, OAuthRelayDeps};
    let state = &mut fixture.state;
    let encryption_provider = state.encryption_provider.clone().unwrap();
    state.oauth_relay = Arc::new(
        OAuthRelay::new(OAuthRelayDeps {
            pool: state.db_pool.clone(),
            deployment_backend: state.deployment_backend.clone(),
            sealer: Arc::new(RiseSealer::new(encryption_provider.clone())),
            store: Arc::new(PgTransientStore::new(state.db_pool.clone())),
            encryption_provider,
            public_url: state.public_url.clone(),
            ssrf: state.server_settings.ssrf.clone(),
        })
        .unwrap(),
    );
}

async fn relay_store_rows(fixture: &Fixture) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM oauth_relay_store")
        .fetch_one(&fixture.state.db_pool)
        .await
        .unwrap()
}

#[sqlx::test]
async fn flows_run_through_the_postgres_store_and_the_rise_sealer(pool: PgPool) {
    let mut fixture = fixture(pool).await;
    use_production_store_and_sealer(&mut fixture);
    let upstream = MockUpstream::start(true).await;
    create_extension(&fixture, &upstream, Client::Confidential, false).await;
    let app = app(&fixture);

    // Confidential: a sealed single-use code and a sealed handle.
    let code = app_code(&app, &upstream, false).await;
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", APP_REDIRECT),
        ("client_id", RISE_CLIENT_ID),
        ("client_secret", RISE_CLIENT_SECRET),
    ];
    let (status, tokens) = token(&app, &form).await;
    assert_eq!(status, StatusCode::OK, "{tokens}");
    let handle = tokens["refresh_token"].as_str().unwrap().to_owned();
    assert!(handle.starts_with("rrt1."), "{handle}");
    // The code's replay marker lives in Postgres.
    assert!(relay_store_rows(&fixture).await >= 1);
    let (status, body) = token(&app, &form).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_grant");
    let (status, refreshed) = token(
        &app,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", &handle),
            ("client_id", RISE_CLIENT_ID),
            ("client_secret", RISE_CLIENT_SECRET),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{refreshed}");
    assert_eq!(refreshed["access_token"], "upstream-access-refreshed");

    // The test flow keeps its single-use record in the same store.
    let session = rise_ui_session_headers(&fixture).await;
    let started = start_test(&app, &session).await.unwrap();
    let callback = through_relay(&app, &upstream, &started, &[("code", "upstream-code")]).await;
    let headers = with_flow_cookie(&session, &started.cookie);
    let response = get_path(&app, &callback, &headers).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(extension_status(&fixture).await["auth_verified"], true);
    assert_eq!(
        get_path(&app, &callback, &headers).await.status(),
        StatusCode::BAD_REQUEST
    );

    // Public: rotating handles are families in Postgres.
    let project = project_id(&fixture).await;
    db_extensions::delete_permanently(&fixture.state.db_pool, project, "ext")
        .await
        .unwrap();
    create_extension(&fixture, &upstream, Client::Public, false).await;
    let code = app_code(&app, &upstream, true).await;
    let (status, tokens) = token(
        &app,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", APP_REDIRECT),
            ("client_id", RISE_CLIENT_ID),
            ("code_verifier", PKCE_VERIFIER),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{tokens}");
    let first = tokens["refresh_token"].as_str().unwrap().to_owned();
    let refresh = |handle: String| {
        let app = app.clone();
        async move {
            token(
                &app,
                &[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", &handle),
                    ("client_id", RISE_CLIENT_ID),
                ],
            )
            .await
        }
    };
    let (status, rotated) = refresh(first.clone()).await;
    assert_eq!(status, StatusCode::OK, "{rotated}");
    let second = rotated["refresh_token"].as_str().unwrap().to_owned();
    assert_ne!(first, second);
    let (status, duplicate) = refresh(first).await;
    assert_eq!(status, StatusCode::OK, "{duplicate}");
    assert_eq!(duplicate["refresh_token"], second.as_str());
}

#[sqlx::test]
async fn a_public_url_with_a_path_serves_the_relay_below_it(pool: PgPool) {
    let mut fixture = fixture(pool).await;
    let upstream = MockUpstream::start(true).await;
    create_extension(&fixture, &upstream, Client::Confidential, false).await;
    let state = &mut fixture.state;
    state.oauth_relay = Arc::new(
        super::relay::OAuthRelay::new(super::relay::OAuthRelayDeps {
            pool: state.db_pool.clone(),
            deployment_backend: state.deployment_backend.clone(),
            encryption_provider: state.encryption_provider.clone().unwrap(),
            public_url: format!("{PUBLIC_URL}/rise/"),
            ssrf: state.server_settings.ssrf.clone(),
            sealer: Arc::new(oauthrelay_core::XChaChaSealer::new(&[7; 32], None).unwrap()),
            store: Arc::new(oauthrelay_core::MemoryStore::default()),
        })
        .unwrap(),
    );
    let app = app(&fixture);

    let response = get_path(
        &app,
        "/oidc/demo/ext/.well-known/openid-configuration",
        &HeaderMap::new(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let doc = body_json(response).await;
    assert_eq!(
        doc["authorization_endpoint"],
        format!("{PUBLIC_URL}/rise/oidc/demo/ext/authorize")
    );
    // In-process calls address the relay's own path.
    let response = fixture
        .state
        .oauth_relay
        .token("demo", "ext", &[("grant_type", "refresh_token")])
        .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
