//! Handler-level tests of the token endpoint against a local mock upstream.
//!
//! They drive `token_endpoint` with a real `AppState` (the sign-in test
//! fixture) and a mock upstream token endpoint on 127.0.0.1, which the
//! fixture's SSRF settings allow.

use super::handlers::token_endpoint;
use super::models::{OAuthCodeState, OAuthExtensionSpec, OAuthExtensionStatus};
use super::provider::oauth_client_secret_context;
use super::refresh_handle;
use crate::db::{extensions, oauth_transient_state};
use crate::server::auth::browser_flow_tests::{fixture, Fixture};
use crate::server::encryption::providers::local::LocalEncryptionProvider;
use crate::server::encryption::{EncryptionContext, EncryptionProvider};
use crate::server::state::AppState;
use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
    routing::post,
    Router,
};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

pub(super) const KEY: &str = "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=";
pub(super) const CLIENT_ID: &str = "demo-login";
pub(super) const CLIENT_SECRET: &str = "rise-client-secret";

/// What the mock upstream answers, and what it received.
#[derive(Clone)]
pub(super) struct Upstream {
    pub url: String,
    pub requests: Arc<Mutex<Vec<String>>>,
    /// Requests for the OIDC discovery document.
    pub discovery_hits: Arc<std::sync::atomic::AtomicUsize>,
    reply: Arc<Mutex<(u16, String, String)>>,
    server: Arc<tokio::task::JoinHandle<()>>,
}

impl Upstream {
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let reply = Arc::new(Mutex::new((
            200u16,
            "application/json".to_string(),
            "{}".to_string(),
        )));
        let (seen, answer) = (requests.clone(), reply.clone());
        let discovery_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (hits, base) = (discovery_hits.clone(), url.clone());
        let app = Router::new()
            .route(
                "/.well-known/openid-configuration",
                axum::routing::get(move || {
                    hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let base = base.clone();
                    async move {
                        axum::Json(json!({
                            "authorization_endpoint": format!("{base}/authorize"),
                            "token_endpoint": format!("{base}/token"),
                        }))
                    }
                }),
            )
            .route(
                "/token",
                post(move |headers: HeaderMap, body: String| {
                    let (seen, answer) = (seen.clone(), answer.clone());
                    async move {
                        assert_eq!(
                            headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()),
                            Some("application/json")
                        );
                        seen.lock().unwrap().push(body);
                        let (status, content_type, body) = answer.lock().unwrap().clone();
                        Response::builder()
                            .status(status)
                            .header(header::CONTENT_TYPE, content_type)
                            .body(Body::from(body))
                            .unwrap()
                    }
                }),
            );
        let server = Arc::new(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        Self {
            url,
            requests,
            discovery_hits,
            reply,
            server,
        }
    }

    pub fn answer(&self, status: u16, content_type: &str, body: &str) {
        *self.reply.lock().unwrap() = (status, content_type.into(), body.into());
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    pub fn last_request(&self) -> String {
        self.requests.lock().unwrap().last().cloned().unwrap()
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        if Arc::strong_count(&self.server) == 1 {
            self.server.abort();
        }
    }
}

pub(super) struct Harness {
    pub fixture: Fixture,
    pub project: Uuid,
    pub upstream: Upstream,
    pub encryption: Arc<LocalEncryptionProvider>,
}

impl Harness {
    /// An OAuth extension `login` of project `demo` whose token endpoint is
    /// the mock upstream, with extra spec members from `spec_extra`.
    pub async fn new(pool: PgPool, spec_extra: Value) -> Self {
        Self::build(pool, spec_extra, true).await
    }

    /// Like [`Harness::new`], but the extension has no explicit endpoints, so
    /// every call to the upstream first needs OIDC discovery.
    pub async fn with_discovery(pool: PgPool, spec_extra: Value) -> Self {
        Self::build(pool, spec_extra, false).await
    }

    async fn build(pool: PgPool, spec_extra: Value, explicit_endpoints: bool) -> Self {
        let mut fixture = fixture(pool.clone()).await;
        let encryption = Arc::new(LocalEncryptionProvider::new(KEY).unwrap());
        fixture.state.encryption_provider = Some(encryption.clone());
        let project = crate::db::projects::find_by_name(&pool, "demo")
            .await
            .unwrap()
            .unwrap()
            .id;
        let upstream = Upstream::start().await;
        let token_endpoint = format!("{}/token", upstream.url);
        let mut spec = json!({
            "provider_name": "Mock",
            "client_id": "upstream-client",
            "issuer_url": upstream.url,
        });
        if explicit_endpoints {
            spec["authorization_endpoint"] = json!(format!("{}/authorize", upstream.url));
            spec["token_endpoint"] = json!(token_endpoint);
        }
        for (name, value) in spec_extra.as_object().unwrap() {
            spec[name] = value.clone();
        }
        let parsed: OAuthExtensionSpec = serde_json::from_value(spec.clone()).unwrap();
        spec["client_secret_encrypted"] = json!(encryption
            .encrypt(
                "upstream-secret",
                &oauth_client_secret_context(project, "login", &parsed, &token_endpoint).unwrap(),
            )
            .await
            .unwrap());
        extensions::create(&pool, project, "login", "oauth", &spec)
            .await
            .unwrap();
        extensions::update_status(
            &pool,
            project,
            "login",
            &serde_json::to_value(OAuthExtensionStatus {
                rise_client_id: Some(CLIENT_ID.into()),
                rise_client_secret: Some(CLIENT_SECRET.into()),
                ..Default::default()
            })
            .unwrap(),
        )
        .await
        .unwrap();
        Self {
            fixture,
            project,
            upstream,
            encryption,
        }
    }

    pub fn state(&self) -> AppState {
        self.fixture.state.clone()
    }

    pub fn pool(&self) -> &PgPool {
        &self.fixture.state.db_pool
    }

    /// POST a form to the Rise token endpoint of `login`.
    pub async fn token(&self, form: &[(&str, &str)]) -> (StatusCode, HeaderMap, String) {
        self.token_with(self.state(), form).await
    }

    pub async fn token_with(
        &self,
        state: AppState,
        form: &[(&str, &str)],
    ) -> (StatusCode, HeaderMap, String) {
        let mut form = form.to_vec();
        form.push(("client_id", CLIENT_ID));
        form.push(("client_secret", CLIENT_SECRET));
        let response = token_endpoint(
            State(state),
            Path(("demo".to_string(), "login".to_string())),
            HeaderMap::new(),
            serde_urlencoded::to_string(&form).unwrap(),
        )
        .await;
        let (parts, body) = response.into_parts();
        let body = axum::body::to_bytes(body, 1 << 20).await.unwrap();
        (
            parts.status,
            parts.headers,
            String::from_utf8(body.to_vec()).unwrap(),
        )
    }

    pub async fn refresh(&self, token: &str) -> (StatusCode, HeaderMap, String) {
        self.token(&[("grant_type", "refresh_token"), ("refresh_token", token)])
            .await
    }

    /// A sealed handle for `upstream_token`, as an earlier login would hold.
    pub async fn sealed_handle(&self, upstream_token: &str) -> String {
        refresh_handle::seal(
            self.encryption.as_ref(),
            self.project,
            "login",
            upstream_token,
            1,
        )
        .await
        .unwrap()
    }

    /// Store an upstream code-grant response as the callback does and return the code.
    pub async fn stored_code_response(
        &self,
        status: u16,
        content_type: &str,
        body: &str,
    ) -> String {
        let code = format!("code-{}", Uuid::new_v4());
        let encrypted = self
            .encryption
            .encrypt(
                body,
                &EncryptionContext::extension(
                    self.project,
                    "oauth",
                    "login",
                    &format!("/tokens/response/{code}"),
                ),
            )
            .await
            .unwrap();
        oauth_transient_state::insert(
            self.pool(),
            &code,
            &OAuthCodeState {
                project_id: self.project,
                extension_name: "login".into(),
                created_at: chrono::Utc::now(),
                redirect_uri: None,
                code_challenge: None,
                code_challenge_method: None,
                token_response_encrypted: encrypted,
                content_type: content_type.into(),
                status_code: status,
            },
            std::time::Duration::from_secs(60),
        )
        .await
        .unwrap();
        code
    }

    pub async fn exchange_code(&self, code: &str) -> (StatusCode, HeaderMap, String) {
        self.token(&[("grant_type", "authorization_code"), ("code", code)])
            .await
    }
}

pub(super) fn json_body(body: &str) -> Value {
    serde_json::from_str(body).unwrap()
}

pub(super) fn content_type(headers: &HeaderMap) -> &str {
    headers.get(header::CONTENT_TYPE).unwrap().to_str().unwrap()
}

/// Positions of `names` in `text`; ascending when members appear in that order.
pub(super) fn member_positions(text: &str, names: &[&str]) -> Vec<usize> {
    names
        .iter()
        .map(|name| text.find(&format!("\"{name}\"")).unwrap())
        .collect()
}

async fn unseal(h: &Harness, handle: &str) -> String {
    refresh_handle::unseal(h.encryption.as_ref(), h.project, "login", handle)
        .await
        .unwrap()
        .upstream_refresh_token
}

#[sqlx::test]
async fn code_grant_replaces_only_the_refresh_token_and_keeps_member_order(pool: PgPool) {
    let h = Harness::new(pool, json!({})).await;
    let upstream_body = r#"{"zeta":{"b":1,"a":[1,2.50]},"access_token":"at","token_type":"bearer","refresh_token":"upstream-rt","scope":"x y","expires_in":3600,"alpha":null}"#;
    let code = h
        .stored_code_response(200, "application/json; charset=utf-8", upstream_body)
        .await;

    let (status, headers, body) = h.exchange_code(&code).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type(&headers), "application/json; charset=utf-8");
    assert!(!body.contains("upstream-rt"));
    let positions = member_positions(
        &body,
        &[
            "zeta",
            "access_token",
            "token_type",
            "refresh_token",
            "scope",
            "expires_in",
            "alpha",
        ],
    );
    assert!(positions.windows(2).all(|w| w[0] < w[1]), "{body}");
    assert!(body.contains(r#""zeta":{"b":1,"a":[1,2.50]}"#), "{body}");
    let handle = json_body(&body)["refresh_token"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(handle.starts_with("rrt1."));
    assert_eq!(unseal(&h, &handle).await, "upstream-rt");
    assert_eq!(h.upstream.request_count(), 0);
}

#[sqlx::test]
async fn code_grant_fails_closed_for_a_form_encoded_2xx_body_and_relays_errors(pool: PgPool) {
    let h = Harness::new(pool, json!({})).await;
    let code = h
        .stored_code_response(
            200,
            "application/x-www-form-urlencoded",
            "access_token=at&refresh_token=leaky-rt&token_type=bearer",
        )
        .await;
    let (status, _, body) = h.exchange_code(&code).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(!body.contains("leaky-rt"));
    assert_eq!(json_body(&body)["error"], "server_error");

    // A non-2xx cached response is relayed untouched.
    let error = r#"{"error":"invalid_client","refresh_token":"not-a-token"}"#;
    let code = h.stored_code_response(401, "application/json", error).await;
    let (status, _, body) = h.exchange_code(&code).await;
    assert_eq!((status, body.as_str()), (StatusCode::UNAUTHORIZED, error));
}

#[sqlx::test]
async fn refresh_passes_the_upstream_response_through_with_only_the_token_replaced(pool: PgPool) {
    let h = Harness::new(pool, json!({"scopes": ["rise-scope"]})).await;
    h.upstream.answer(
        200,
        "application/json;charset=UTF-8",
        r#"{"id_token":"idt","access_token":"at","extra":{"k":[true]},"refresh_token":"rotated-rt","scope":"upstream scope","token_type":"Bearer"}"#,
    );
    let handle = h.sealed_handle("old-rt").await;

    let (status, headers, body) = h.refresh(&handle).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type(&headers), "application/json;charset=UTF-8");
    let positions = member_positions(
        &body,
        &[
            "id_token",
            "access_token",
            "extra",
            "refresh_token",
            "scope",
            "token_type",
        ],
    );
    assert!(positions.windows(2).all(|w| w[0] < w[1]), "{body}");
    let value = json_body(&body);
    assert_eq!(
        value["scope"], "upstream scope",
        "the extension's scopes do not override it"
    );
    assert_eq!(value["extra"], json!({"k": [true]}));
    assert!(!body.contains("rotated-rt"));
    assert_eq!(
        unseal(&h, value["refresh_token"].as_str().unwrap()).await,
        "rotated-rt"
    );
    let request = h.upstream.last_request();
    assert!(
        request.contains("grant_type=refresh_token") && request.contains("refresh_token=old-rt")
    );

    // An upstream that does not rotate returns no refresh token: the client keeps its handle.
    h.upstream.answer(
        200,
        "application/json",
        r#"{"access_token":"at2","token_type":"Bearer"}"#,
    );
    let (status, _, body) = h.refresh(&handle).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, r#"{"access_token":"at2","token_type":"Bearer"}"#);
}

#[sqlx::test]
async fn a_legacy_raw_token_is_refreshed_and_sealed_into_the_response(pool: PgPool) {
    let h = Harness::new(pool, json!({})).await;
    h.upstream.answer(
        200,
        "application/json",
        r#"{"access_token":"at","token_type":"Bearer"}"#,
    );
    let (status, _, body) = h.refresh("legacy-raw-rt").await;
    assert_eq!(status, StatusCode::OK);
    assert!(h
        .upstream
        .last_request()
        .contains("refresh_token=legacy-raw-rt"));
    assert!(!body.contains("legacy-raw-rt"));
    let handle = json_body(&body)["refresh_token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(unseal(&h, &handle).await, "legacy-raw-rt");
}

#[sqlx::test]
async fn upstream_failures_pass_through_and_only_invalid_grant_is_a_verdict(pool: PgPool) {
    let h = Harness::new(pool, json!({})).await;
    let handle = h.sealed_handle("rt").await;

    let outage = r#"{"error":"temporarily_unavailable","error_description":"try later"}"#;
    h.upstream.answer(503, "application/json", outage);
    let (status, headers, body) = h.refresh(&handle).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(content_type(&headers), "application/json");
    assert_eq!(body, outage);

    let gateway = "<html>bad gateway</html>";
    h.upstream.answer(502, "text/html", gateway);
    let (status, headers, body) = h.refresh(&handle).await;
    assert_eq!(
        (status, content_type(&headers), body.as_str()),
        (StatusCode::BAD_GATEWAY, "text/html", gateway)
    );

    let invalid_client = r#"{"error":"invalid_client"}"#;
    h.upstream.answer(401, "application/json", invalid_client);
    let (status, _, body) = h.refresh(&handle).await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::UNAUTHORIZED, invalid_client)
    );

    // invalid_grant is only recognised as JSON with a 4xx status.
    h.upstream.answer(400, "text/plain", "error=invalid_grant");
    let (status, _, body) = h.refresh(&handle).await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::BAD_REQUEST, "error=invalid_grant")
    );
    h.upstream
        .answer(500, "application/json", r#"{"error":"invalid_grant"}"#);
    assert_eq!(
        h.refresh(&handle).await.0,
        StatusCode::INTERNAL_SERVER_ERROR
    );

    for status in [400, 401] {
        h.upstream.answer(
            status,
            "application/json",
            r#"{"error":"invalid_grant","error_description":"expired"}"#,
        );
        let (code, _, body) = h.refresh(&handle).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(json_body(&body)["error"], "invalid_grant");
        assert!(
            !body.contains("expired"),
            "Rise's own error, not the upstream's"
        );
    }
}

#[sqlx::test]
async fn a_2xx_form_encoded_response_with_a_refresh_token_fails_closed(pool: PgPool) {
    let h = Harness::new(pool, json!({})).await;
    h.upstream.answer(
        200,
        "application/x-www-form-urlencoded",
        "access_token=at&refresh_token=leaky-rt&token_type=bearer",
    );
    for token in [h.sealed_handle("rt").await, "legacy-raw".to_string()] {
        let (status, _, body) = h.refresh(&token).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(!body.contains("leaky-rt"));
        assert_eq!(json_body(&body)["error"], "server_error");
    }
    // Without any refresh token in it, an opaque 2xx body is relayed.
    h.upstream.answer(200, "text/plain", "ok");
    let (status, _, body) = h.refresh(&h.sealed_handle("rt").await).await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "ok"));
}

#[sqlx::test]
async fn an_unreachable_upstream_is_a_502(pool: PgPool) {
    let h = Harness::new(pool, json!({})).await;
    // Nothing listens on the port once the mock is gone.
    h.upstream.server.abort();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let (status, _, body) = h.refresh(&h.sealed_handle("rt").await).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(json_body(&body)["error"], "server_error");
}

#[sqlx::test]
async fn handles_of_other_schemes_or_scopes_are_never_forwarded(pool: PgPool) {
    let h = Harness::new(pool, json!({})).await;
    for token in [
        "rrt2.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_string(),
        "rrt9.whatever".to_string(),
        "rrt1.not-a-ciphertext".to_string(),
        refresh_handle::seal(h.encryption.as_ref(), h.project, "other", "rt", 1)
            .await
            .unwrap(),
    ] {
        let (status, _, body) = h.refresh(&token).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{token}");
        assert_eq!(json_body(&body)["error"], "invalid_grant");
    }
    assert_eq!(h.upstream.request_count(), 0);
}

struct Throttled;

#[async_trait::async_trait]
impl EncryptionProvider for Throttled {
    async fn encrypt(&self, _: &str, _: &EncryptionContext) -> anyhow::Result<String> {
        Err(anyhow::anyhow!("ThrottlingException"))
    }
    async fn decrypt(&self, _: &str, _: &EncryptionContext) -> anyhow::Result<String> {
        Err(anyhow::anyhow!("ThrottlingException"))
    }
}

#[sqlx::test]
async fn transient_encryption_failures_are_503_and_not_invalid_grant(pool: PgPool) {
    let h = Harness::new(pool, json!({})).await;
    let handle = h.sealed_handle("rt").await;
    let mut state = h.state();
    state.encryption_provider = Some(Arc::new(Throttled));
    let (status, _, body) = h
        .token_with(
            state,
            &[("grant_type", "refresh_token"), ("refresh_token", &handle)],
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json_body(&body)["error"], "server_error");
    assert_eq!(h.upstream.request_count(), 0);
}

fn rotating() -> Value {
    json!({"refresh_token": {"mode": "rotating", "grace_period_seconds": 30}})
}

fn handle_of(body: &str) -> String {
    json_body(body)["refresh_token"]
        .as_str()
        .unwrap()
        .to_string()
}

fn handle_hash(handle: &str) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    Sha256::digest(handle.as_bytes()).to_vec()
}

/// Log a user in through the code grant of a rotating extension.
async fn rotating_login(h: &Harness, upstream_rt: &str) -> String {
    let code = h
        .stored_code_response(
            200,
            "application/json",
            &json!({"access_token": "at0", "token_type": "Bearer", "refresh_token": upstream_rt})
                .to_string(),
        )
        .await;
    let (status, _, body) = h.exchange_code(&code).await;
    assert_eq!(status, StatusCode::OK);
    let handle = handle_of(&body);
    assert!(handle.starts_with("rrt2."), "{handle}");
    handle
}

#[sqlx::test]
async fn rotating_code_grant_relays_the_response_with_a_stored_handle(pool: PgPool) {
    let h = Harness::new(pool, rotating()).await;
    let upstream_body = r#"{"zeta":{"b":1},"access_token":"at","refresh_token":"upstream-rt","scope":"x y","alpha":null}"#;
    let code = h
        .stored_code_response(200, "application/json; charset=utf-8", upstream_body)
        .await;
    let (status, headers, body) = h.exchange_code(&code).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type(&headers), "application/json; charset=utf-8");
    assert!(!body.contains("upstream-rt"));
    let positions = member_positions(
        &body,
        &["zeta", "access_token", "refresh_token", "scope", "alpha"],
    );
    assert!(positions.windows(2).all(|w| w[0] < w[1]), "{body}");
    assert!(handle_of(&body).starts_with("rrt2."));

    let code = h
        .stored_code_response(
            200,
            "application/x-www-form-urlencoded",
            "refresh_token=leaky&access_token=at",
        )
        .await;
    let (status, _, body) = h.exchange_code(&code).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(!body.contains("leaky"));
}

#[sqlx::test]
async fn rotating_refresh_relays_the_upstream_response_and_replays_it_within_grace(pool: PgPool) {
    let h = Harness::new(pool, rotating()).await;
    let first = rotating_login(&h, "upstream-1").await;
    h.upstream.answer(
        200,
        "application/json;charset=UTF-8",
        r#"{"id_token":"idt","access_token":"at1","extra":{"k":[true]},"scope":"upstream scope","token_type":"Bearer"}"#,
    );

    let (status, headers, body) = h.refresh(&first).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type(&headers), "application/json;charset=UTF-8");
    let second = handle_of(&body);
    assert!(second.starts_with("rrt2.") && second != first);
    let value = json_body(&body);
    assert_eq!(value["scope"], "upstream scope");
    assert_eq!(value["extra"], json!({"k": [true]}));
    let positions = member_positions(
        &body,
        &[
            "id_token",
            "access_token",
            "extra",
            "scope",
            "token_type",
            "refresh_token",
        ],
    );
    assert!(
        positions.windows(2).all(|w| w[0] < w[1]),
        "the handle is appended: {body}"
    );
    assert!(h
        .upstream
        .last_request()
        .contains("refresh_token=upstream-1"));

    // The same handle within the grace period replays the same response; the
    // upstream is not asked again.
    let (status, replay_headers, replay) = h.refresh(&first).await;
    assert_eq!((status, replay.as_str()), (StatusCode::OK, body.as_str()));
    assert_eq!(
        content_type(&replay_headers),
        "application/json;charset=UTF-8"
    );
    assert_eq!(h.upstream.request_count(), 1);

    // The successor works, and wraps the same upstream token (not rotated).
    h.upstream.answer(
        200,
        "application/json",
        r#"{"access_token":"at2","refresh_token":"upstream-2"}"#,
    );
    let (status, _, body) = h.refresh(&second).await;
    assert_eq!(status, StatusCode::OK);
    assert!(h
        .upstream
        .last_request()
        .contains("refresh_token=upstream-1"));
    assert!(!body.contains("upstream-2"));
    let third = handle_of(&body);
    h.upstream
        .answer(200, "application/json", r#"{"access_token":"at3"}"#);
    assert_eq!(h.refresh(&third).await.0, StatusCode::OK);
    assert!(h
        .upstream
        .last_request()
        .contains("refresh_token=upstream-2"));
}

#[sqlx::test]
async fn rotating_replays_and_waiting_requests_do_no_discovery_or_upstream_work(pool: PgPool) {
    let h = Harness::with_discovery(pool, rotating()).await;
    let first = rotating_login(&h, "upstream-1").await;
    h.upstream
        .answer(200, "application/json", r#"{"access_token":"at1"}"#);
    let hits = || {
        h.upstream
            .discovery_hits
            .load(std::sync::atomic::Ordering::SeqCst)
    };

    let (status, _, body) = h.refresh(&first).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!((hits(), h.upstream.request_count()), (1, 1));
    for _ in 0..3 {
        let (status, _, replay) = h.refresh(&first).await;
        assert_eq!((status, replay.as_str()), (StatusCode::OK, body.as_str()));
    }
    assert_eq!((hits(), h.upstream.request_count()), (1, 1));

    // A request that waits for a claim holder never touches the upstream setup.
    let second = handle_of(&body);
    crate::db::oauth_refresh_tokens::claim(
        h.pool(),
        &handle_hash(&second),
        h.project,
        "login",
        Uuid::new_v4(),
        crate::db::oauth_refresh_tokens::CLAIM_LEASE,
    )
    .await
    .unwrap()
    .unwrap();
    let (status, headers, body) = h.refresh(&second).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(headers.get(header::RETRY_AFTER).unwrap(), "1");
    assert_eq!(json_body(&body)["error"], "temporarily_unavailable");
    assert_eq!((hits(), h.upstream.request_count()), (1, 1));
}

#[sqlx::test]
async fn rotating_upstream_failures_pass_through_and_only_invalid_grant_revokes(pool: PgPool) {
    let h = Harness::new(pool, rotating()).await;
    let first = rotating_login(&h, "upstream-1").await;

    let outage = r#"{"error":"temporarily_unavailable"}"#;
    h.upstream.answer(503, "application/json", outage);
    let (status, headers, body) = h.refresh(&first).await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, outage)
    );
    assert_eq!(content_type(&headers), "application/json");

    h.upstream.answer(
        200,
        "application/x-www-form-urlencoded",
        "access_token=at&refresh_token=leaky",
    );
    let (status, _, body) = h.refresh(&first).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(!body.contains("leaky"));

    // Neither consumed the handle.
    h.upstream
        .answer(200, "application/json", r#"{"access_token":"at1"}"#);
    let (status, _, body) = h.refresh(&first).await;
    assert_eq!(status, StatusCode::OK);
    let second = handle_of(&body);

    h.upstream
        .answer(400, "application/json", r#"{"error":"invalid_grant"}"#);
    let (status, _, body) = h.refresh(&second).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&body)["error"], "invalid_grant");
    // The login is gone, and so are its stored upstream tokens.
    let (status, _, _) = h.refresh(&second).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let secrets: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM oauth_refresh_tokens WHERE upstream_token_encrypted IS NOT NULL",
    )
    .fetch_one(h.pool())
    .await
    .unwrap();
    assert_eq!(secrets, 0);

    let first = rotating_login(&h, "upstream-2").await;
    h.upstream.server.abort();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let (status, _, body) = h.refresh(&first).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(json_body(&body)["error"], "server_error");
}

#[sqlx::test]
async fn rotating_mode_refuses_raw_tokens_and_accepts_fresh_rrt1_handles(pool: PgPool) {
    let h = Harness::new(pool, rotating()).await;
    h.upstream.answer(
        200,
        "application/json",
        r#"{"access_token":"at1","token_type":"Bearer"}"#,
    );

    let (status, _, body) = h.refresh("raw-upstream-token").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&body)["error"], "invalid_grant");
    assert_eq!(h.upstream.request_count(), 0);

    // A sealed handle from before the switch starts a rotating login.
    let sealed = refresh_handle::seal(
        h.encryption.as_ref(),
        h.project,
        "login",
        "upstream-sealed",
        chrono::Utc::now().timestamp() - 60,
    )
    .await
    .unwrap();
    let (status, _, body) = h.refresh(&sealed).await;
    assert_eq!(status, StatusCode::OK);
    assert!(h
        .upstream
        .last_request()
        .contains("refresh_token=upstream-sealed"));
    let handle = handle_of(&body);
    assert!(handle.starts_with("rrt2."));

    // One older than the maximum login lifetime (30 days by default) is refused.
    let old = refresh_handle::seal(
        h.encryption.as_ref(),
        h.project,
        "login",
        "upstream-old",
        chrono::Utc::now().timestamp() - 31 * 24 * 3600,
    )
    .await
    .unwrap();
    let (status, _, body) = h.refresh(&old).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&body)["error"], "invalid_grant");
    assert_eq!(h.upstream.request_count(), 1);
}

#[sqlx::test]
async fn rotating_mode_answers_503_for_transient_encryption_failures(pool: PgPool) {
    let h = Harness::new(pool, rotating()).await;
    let first = rotating_login(&h, "upstream-1").await;
    let mut state = h.state();
    state.encryption_provider = Some(Arc::new(Throttled));
    let (status, _, body) = h
        .token_with(
            state,
            &[("grant_type", "refresh_token"), ("refresh_token", &first)],
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json_body(&body)["error"], "server_error");
    assert_eq!(h.upstream.request_count(), 0);

    // The handle was not consumed, nor left claimed.
    h.upstream
        .answer(200, "application/json", r#"{"access_token":"at1"}"#);
    assert_eq!(h.refresh(&first).await.0, StatusCode::OK);
}

#[sqlx::test]
async fn sealed_extensions_reject_rrt2_handles_and_switching_to_sealed_deletes_the_families(
    pool: PgPool,
) {
    use super::provider::{OAuthProvider, OAuthProviderConfig};
    use crate::server::extensions::Extension;

    let h = Harness::new(pool, rotating()).await;
    let first = rotating_login(&h, "upstream-1").await;
    let families = || async {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM oauth_refresh_families")
            .fetch_one(h.pool())
            .await
            .unwrap()
    };
    assert_eq!(families().await, 1);

    // Switch the extension to sealed through the provider's spec-update hook.
    let stored = extensions::find_by_project_and_name(h.pool(), h.project, "login")
        .await
        .unwrap()
        .unwrap();
    let mut sealed_spec = stored.spec.clone();
    sealed_spec.as_object_mut().unwrap().remove("refresh_token");
    let provider = OAuthProvider::new(OAuthProviderConfig {
        db_pool: h.pool().clone(),
        encryption_provider: h.encryption.clone(),
        ssrf_config: h.state().server_settings.ssrf.clone(),
        api_domain: h.state().public_url.clone(),
    });
    provider
        .on_spec_updated(&stored.spec, &sealed_spec, h.project, "login", h.pool())
        .await
        .unwrap();
    assert_eq!(families().await, 0);
    sqlx::query(
        "UPDATE project_extensions SET spec = $1 WHERE project_id = $2 AND extension = 'login'",
    )
    .bind(&sealed_spec)
    .bind(h.project)
    .execute(h.pool())
    .await
    .unwrap();

    let (status, _, body) = h.refresh(&first).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&body)["error"], "invalid_grant");
    assert_eq!(
        h.upstream.request_count(),
        0,
        "an rrt2. handle is never forwarded upstream"
    );

    // Spec updates that keep the mode do not touch the families.
    sqlx::query(
        "UPDATE project_extensions SET spec = $1 WHERE project_id = $2 AND extension = 'login'",
    )
    .bind(&stored.spec)
    .bind(h.project)
    .execute(h.pool())
    .await
    .unwrap();
    rotating_login(&h, "upstream-2").await;
    provider
        .on_spec_updated(&stored.spec, &stored.spec, h.project, "login", h.pool())
        .await
        .unwrap();
    assert_eq!(families().await, 1);
}
