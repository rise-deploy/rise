//! Handler-level sign-in regressions with independent host-scoped cookie jars.

use std::collections::{BTreeMap, HashMap};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use axum::{
    extract::State,
    http::{header, HeaderMap, Uri},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::json;
use sqlx::PgPool;

use super::{handlers::*, token_storage::DbTokenStore};
use crate::server::{settings::Settings, state::AppState};

#[derive(Default)]
struct Browser {
    hosts: HashMap<String, BTreeMap<String, String>>,
}

impl Browser {
    fn headers(&self, host: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, host.parse().unwrap());
        if let Some(cookies) = self.hosts.get(host) {
            let value = cookies
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>()
                .join("; ");
            headers.insert(header::COOKIE, value.parse().unwrap());
        }
        headers
    }

    fn receive(&mut self, host: &str, response: &Response) {
        for header in response.headers().get_all(header::SET_COOKIE) {
            let (name, value) = header
                .to_str()
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .split_once('=')
                .unwrap();
            let cookies = self.hosts.entry(host.to_string()).or_default();
            if value.is_empty() {
                cookies.remove(name);
            } else {
                cookies.insert(name.to_string(), value.to_string());
            }
        }
    }
}

struct Backend;
#[async_trait::async_trait]
impl rise_backend_core::DeploymentBackend for Backend {
    async fn get_project_urls(
        &self,
        project: &crate::db::models::Project,
        _: &str,
    ) -> anyhow::Result<rise_backend_core::DeploymentUrls> {
        let url = format!("https://{}.apps.example.com", project.name);
        Ok(rise_backend_core::DeploymentUrls {
            default_url: url.clone(),
            primary_url: Some(url.clone()),
            custom_domain_urls: vec!["https://custom.example.com".into()],
            all_urls: vec![url, "https://custom.example.com".into()],
        })
    }
}

pub(crate) struct Fixture {
    pub(crate) state: AppState,
    exchanges: Arc<AtomicUsize>,
    idp_server: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.idp_server.abort();
    }
}

pub(crate) async fn fixture(pool: PgPool) -> Fixture {
    use crate::server::{
        auth::{jwt::JwtValidator, oauth::OAuthClient, user_identity::UserLogins},
        authz::{OperatorSelectors, ResourceAuthorizer},
    };
    use rise_backend_auth::RiseTokenSigner;
    const KEY: &str = "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=";
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("http://{}", listener.local_addr().unwrap());
    let idp_signer = RiseTokenSigner::new(
        KEY,
        issuer.clone(),
        3600,
        vec!["email".into(), "sub".into()],
        None,
        None,
    )
    .unwrap();
    let jwks = idp_signer.generate_jwks().unwrap();
    let id_token = idp_signer
        .sign_ingress_jwt(
            &json!({"sub":"member", "email":"member@example.com"}),
            Some(vec![]),
            "rise-test",
            None,
        )
        .unwrap();
    let discovery = json!({"issuer":issuer, "jwks_uri":format!("{issuer}/jwks"), "authorization_endpoint":format!("{issuer}/authorize"), "token_endpoint":format!("{issuer}/token")});
    let exchanges = Arc::new(AtomicUsize::new(0));
    let exchange_count = exchanges.clone();
    let mock = Router::new()
        .route("/.well-known/openid-configuration", get(move || { let body=discovery.clone(); async move { Json(body) } }))
        .route("/jwks", get(move || { let body=jwks.clone(); async move { Json(body) } }))
        .route("/token", post(move || { let token=id_token.clone(); let count=exchange_count.clone(); async move { count.fetch_add(1, Ordering::SeqCst); Json(json!({"access_token":"test", "id_token":token,"token_type":"Bearer","expires_in":3600})) } }));
    let idp_server = tokio::spawn(async move {
        axum::serve(listener, mock).await.unwrap();
    });
    let settings: Settings = serde_json::from_value(json!({
        "server":{"host":"127.0.0.1","port":3000,"public_url":"https://rise.example.com","jwt_signing_secret":KEY,"static_dir":format!("{}/static",env!("CARGO_MANIFEST_DIR")),"ssrf":{"allow_http":true,"allow_private_networks":true}},
        "auth":{"issuer":issuer,"client_id":"rise-test","client_secret":"secret","authorize_url":format!("{issuer}/authorize"),"token_url":format!("{issuer}/token"),"allow_missing_email_verified":true,"admin_users":["member@example.com"]},
        "deployment_logs":{"type":"none"}
    })).unwrap();
    rise_resource_store_postgres::run_migrations(&pool)
        .await
        .unwrap();
    let store = Arc::new(rise_resource_store_postgres::PgResourceStore::new(
        pool.clone(),
    ));
    let bootstrap = crate::server::bootstrap::run(&pool, store.as_ref(), &settings)
        .await
        .unwrap();
    let deployment_store = Arc::new(crate::db::deployment_store::PgDeploymentStore::new(
        pool.clone(),
    ));
    let empty = Arc::new(Vec::new());
    let state = AppState {
        db_pool: pool.clone(),
        shutdown: tokio_util::sync::CancellationToken::new(),
        extension_handles: Arc::new(std::sync::Mutex::new(Vec::new())),
        jwt_validator: Arc::new(JwtValidator::new(settings.server.ssrf.clone())),
        jwt_signer: Arc::new(
            RiseTokenSigner::new(
                KEY,
                settings.server.public_url.clone(),
                3600,
                vec!["email".into(), "sub".into()],
                None,
                None,
            )
            .unwrap(),
        ),
        oauth_client: Arc::new(
            OAuthClient::new(
                issuer.clone(),
                "rise-test".into(),
                "secret".into(),
                settings.auth.authorize_url.clone(),
                settings.auth.token_url.clone(),
                settings.auth.scopes.clone(),
            )
            .await
            .unwrap(),
        ),
        registry_provider: Arc::new(
            crate::server::registry::providers::docker::OciClientAuthProvider::new(
                crate::server::registry::models::OciClientAuthConfig {
                    registry_url: "registry.example.com".into(),
                    namespace: "rise".into(),
                    client_registry_url: None,
                    username: String::new(),
                    password: String::new(),
                },
            )
            .unwrap(),
        ),
        oci_client: Arc::new(crate::server::oci::OciClient::new().unwrap()),
        admin_users: Arc::new(settings.auth.admin_users.clone()),
        admin_idp_groups: empty.clone(),
        operator_users: empty.clone(),
        operator_idp_groups: empty.clone(),
        resource_store: store.clone(),
        resource_authorizer: ResourceAuthorizer::new(
            store.clone(),
            pool.clone(),
            OperatorSelectors {
                users: empty.clone(),
                idp_groups: empty,
            },
        ),
        user_logins: UserLogins::new(store.clone(), pool.clone(), &issuer).unwrap(),
        default_organization_uid: bootstrap.default_organization_uid,
        default_organization_name: settings.default_organization.name.clone(),
        deployment_controller_class_name: None,
        traefik_http_provider: None,
        org_view: Arc::new(crate::server::organizations::CachedOrgView::new(
            store,
            deployment_store.clone(),
        )),
        auth_settings: Arc::new(settings.auth.clone()),
        server_settings: Arc::new(settings.server.clone()),
        token_store: Arc::new(DbTokenStore::new(pool.clone())),
        cookie_settings: super::cookie_helpers::CookieSettings {
            secure: true,
            cookie_domain: None,
        },
        public_url: settings.server.public_url.clone(),
        encryption_provider: None,
        deployment_backend: Arc::new(Backend),
        deployment_store,
        runtime_log_backend: crate::server::deployment::logs::init_runtime_log_backend(
            &settings.deployment_logs,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap(),
        extension_registry: Arc::new(crate::server::extensions::registry::ExtensionRegistry::new()),
        encrypt_rate_limiter: Arc::new(moka::future::Cache::new(10)),
        oauth_rate_limiter: Arc::new(crate::server::rate_limit::OAuthRateLimiter::new(
            &settings.server.oauth_rate_limit,
        )),
        workload_token_rate_limiter: Arc::new(
            crate::server::rate_limit::WorkloadTokenRateLimiter::new(
                &settings.server.workload_token_rate_limit,
            ),
        ),
        policy_audit_rate_limiter: Arc::new(
            crate::server::rate_limit::PolicyAuditRateLimiter::new(),
        ),
        access_classes: Arc::new(HashMap::new()),
        reserved_project_names: Arc::new(Default::default()),
        control_plane_hosts: Arc::new(Vec::new()),
        signin_base_url: settings.server.public_url,
        production_ingress_url_template: Some("{project_name}.apps.example.com".into()),
        staging_ingress_url_template: None,
        environment_ingress_url_template: None,
        ingress_schema: "https".into(),
        ingress_port: None,
        runtime_arch: None,
        kube_client: None,
        metacontroller_webhook_port: None,
        webhook_ctx: None,
        deployment_defaults: None,
        deployment_constraints: None,
        identity_token_ttl_seconds: 3600,
        quickstart_templates: Arc::new(Vec::new()),
    };
    let owner = crate::db::users::create(&pool, "owner@example.com")
        .await
        .unwrap();
    let project = crate::db::projects::create(
        &pool,
        "demo",
        crate::db::models::ProjectStatus::Running,
        "public".into(),
        Some(owner.id),
        None,
        None,
    )
    .await
    .unwrap();
    let env = crate::db::environments::test_environment_id(&pool, project.id).await;
    crate::db::deployments::create(
        &pool,
        crate::db::deployments::CreateDeploymentParams {
            deployment_id: "20261027-100000",
            project_id: project.id,
            created_by_id: owner.id,
            status: crate::db::models::DeploymentStatus::Healthy,
            image: None,
            image_digest: None,
            rolled_back_from_deployment_id: None,
            deployment_group: "default",
            environment_id: env,
            expires_at: None,
            http_port: 8080,
            is_active: true,
            job_url: None,
            pull_request_url: None,
            git_repository_url: None,
            replicas: 1,
            cpu: "100m",
            memory: "128Mi",
            identity_audiences: json!({}),
            containers: None,
            routes: None,
            expiration_cap: None,
        },
    )
    .await
    .unwrap();
    Fixture {
        state,
        exchanges,
        idp_server,
    }
}

fn location(response: &Response) -> String {
    response.headers()[header::LOCATION]
        .to_str()
        .unwrap()
        .into()
}
fn query(url: &str, name: &str) -> String {
    url::Url::parse(url)
        .unwrap()
        .query_pairs()
        .find(|(key, _)| key == name)
        .unwrap()
        .1
        .into_owned()
}
fn signin(project: Option<&str>) -> SigninQuery {
    SigninQuery {
        project: project.map(str::to_string),
        redirect: Some("/dashboard".into()),
        rd: None,
    }
}

/// Request headers of a browser signed in to the Rise UI as the fixture's
/// IdP user (`member@example.com`, a configured admin).
pub(crate) async fn rise_ui_session_headers(fixture: &Fixture) -> HeaderMap {
    let mut browser = Browser::default();
    let response = oauth_signin_start(
        State(fixture.state.clone()),
        browser.headers("rise.example.com"),
        Uri::from_static("/api/v1/auth/signin/start"),
        axum::extract::Query(signin(None)),
    )
    .await
    .unwrap();
    browser.receive("rise.example.com", &response);
    let state = query(&location(&response), "state");
    let response = oauth_callback(
        State(fixture.state.clone()),
        browser.headers("rise.example.com"),
        axum::extract::Query(CallbackQuery {
            code: "test-code".into(),
            state,
        }),
    )
    .await
    .unwrap();
    browser.receive("rise.example.com", &response);
    browser.headers("rise.example.com")
}

async fn start_app(fixture: &Fixture, browser: &mut Browser, host: &str) -> String {
    let response = oauth_signin_start(
        State(fixture.state.clone()),
        browser.headers(host),
        Uri::from_static("/.rise/auth/signin/start"),
        axum::extract::Query(signin(Some("demo"))),
    )
    .await
    .unwrap();
    browser.receive(host, &response);
    let handle = query(&location(&response), "transaction");
    let response = oauth_signin_begin(
        State(fixture.state.clone()),
        browser.headers("rise.example.com"),
        axum::extract::Query(BeginQuery {
            transaction: handle,
        }),
    )
    .await
    .unwrap();
    browser.receive("rise.example.com", &response);
    query(&location(&response), "state")
}

#[sqlx::test]
async fn browser_binding_handlers_reject_transferred_callback_and_completion_urls(pool: PgPool) {
    let fixture = fixture(pool).await;
    for host in ["demo.apps.example.com", "custom.example.com"] {
        let mut browser = Browser::default();
        let mut stranger = Browser::default();
        let stranger_start = oauth_signin_start(
            State(fixture.state.clone()),
            stranger.headers("rise.example.com"),
            Uri::from_static("/api/v1/auth/signin/start"),
            axum::extract::Query(signin(None)),
        )
        .await
        .unwrap();
        stranger.receive("rise.example.com", &stranger_start);
        let state = start_app(&fixture, &mut browser, host).await;
        let before = fixture.exchanges.load(Ordering::SeqCst);
        assert!(oauth_callback(
            State(fixture.state.clone()),
            Browser::default().headers("rise.example.com"),
            axum::extract::Query(CallbackQuery {
                code: "test-code".into(),
                state: state.clone()
            })
        )
        .await
        .is_err());
        let mut wrong_origin = browser.headers("rise.example.com");
        wrong_origin.insert(header::HOST, "wrong.example.com".parse().unwrap());
        assert!(oauth_callback(
            State(fixture.state.clone()),
            wrong_origin,
            axum::extract::Query(CallbackQuery {
                code: "test-code".into(),
                state: state.clone()
            })
        )
        .await
        .is_err());
        let wrong = oauth_callback(
            State(fixture.state.clone()),
            stranger.headers("rise.example.com"),
            axum::extract::Query(CallbackQuery {
                code: "test-code".into(),
                state: state.clone(),
            }),
        )
        .await;
        assert!(wrong.is_err());
        assert_eq!(
            fixture.exchanges.load(Ordering::SeqCst),
            before,
            "wrong browser cannot exchange the code"
        );
        let response = oauth_callback(
            State(fixture.state.clone()),
            browser.headers("rise.example.com"),
            axum::extract::Query(CallbackQuery {
                code: "test-code".into(),
                state: state.clone(),
            }),
        )
        .await
        .unwrap();
        browser.receive("rise.example.com", &response);
        let complete = query(&location(&response), "token");
        assert!(oauth_callback(
            State(fixture.state.clone()),
            browser.headers("rise.example.com"),
            axum::extract::Query(CallbackQuery {
                code: "test-code".into(),
                state
            })
        )
        .await
        .is_err());
        assert!(oauth_complete(
            State(fixture.state.clone()),
            stranger.headers(host),
            axum::extract::Query(CompleteQuery {
                token: complete.clone()
            })
        )
        .await
        .is_err());
        assert!(oauth_complete(
            State(fixture.state.clone()),
            browser.headers("other.apps.example.com"),
            axum::extract::Query(CompleteQuery {
                token: complete.clone()
            })
        )
        .await
        .is_err());
        let mut wrong_origin = browser.headers(host);
        wrong_origin.insert(header::HOST, "other.apps.example.com".parse().unwrap());
        assert!(oauth_complete(
            State(fixture.state.clone()),
            wrong_origin,
            axum::extract::Query(CompleteQuery {
                token: complete.clone()
            })
        )
        .await
        .is_err());
        let response = oauth_complete(
            State(fixture.state.clone()),
            browser.headers(host),
            axum::extract::Query(CompleteQuery {
                token: complete.clone(),
            }),
        )
        .await
        .unwrap();
        assert!(response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .any(|value| value.to_str().unwrap().starts_with("rise_jwt=")));
        browser.receive(host, &response);
        assert!(oauth_complete(
            State(fixture.state.clone()),
            browser.headers(host),
            axum::extract::Query(CompleteQuery { token: complete })
        )
        .await
        .is_err());
    }
}

#[sqlx::test]
async fn browser_binding_handlers_support_ui_parallel_tabs_and_public_project_entry(pool: PgPool) {
    let fixture = fixture(pool).await;
    let mut browser = Browser::default();
    let mut states = Vec::new();
    for _ in 0..2 {
        let response = oauth_signin_start(
            State(fixture.state.clone()),
            browser.headers("rise.example.com"),
            Uri::from_static("/api/v1/auth/signin/start"),
            axum::extract::Query(signin(None)),
        )
        .await
        .unwrap();
        browser.receive("rise.example.com", &response);
        states.push(query(&location(&response), "state"));
    }
    for state in states.into_iter().rev() {
        let response = oauth_callback(
            State(fixture.state.clone()),
            browser.headers("rise.example.com"),
            axum::extract::Query(CallbackQuery {
                code: "test-code".into(),
                state,
            }),
        )
        .await
        .unwrap();
        assert!(response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .any(|value| value.to_str().unwrap().starts_with("rise_jwt=")));
        browser.receive("rise.example.com", &response);
    }
    let response = oauth_signin_start(
        State(fixture.state.clone()),
        browser.headers("rise.example.com"),
        Uri::from_static("/api/v1/auth/signin/start"),
        axum::extract::Query(signin(Some("demo"))),
    )
    .await
    .unwrap();
    assert!(
        location(&response).starts_with("https://demo.apps.example.com/.rise/auth/signin/start?")
    );
    assert!(!response.headers().contains_key(header::SET_COOKIE));
    let first = start_app(&fixture, &mut browser, "demo.apps.example.com").await;
    let second = start_app(&fixture, &mut browser, "demo.apps.example.com").await;
    for state in [second, first] {
        let response = oauth_callback(
            State(fixture.state.clone()),
            browser.headers("rise.example.com"),
            axum::extract::Query(CallbackQuery {
                code: "test-code".into(),
                state,
            }),
        )
        .await
        .unwrap();
        browser.receive("rise.example.com", &response);
        let destination = location(&response);
        let host = url::Url::parse(&destination)
            .unwrap()
            .host_str()
            .unwrap()
            .to_string();
        let response = oauth_complete(
            State(fixture.state.clone()),
            browser.headers(&host),
            axum::extract::Query(CompleteQuery {
                token: query(&destination, "token"),
            }),
        )
        .await
        .unwrap();
        browser.receive(&host, &response);
    }
}

#[sqlx::test]
async fn browser_binding_handlers_reject_unbound_and_expired_transactions(pool: PgPool) {
    use super::token_storage::{CompletedAuthSession, OAuth2State};
    let fixture = fixture(pool).await;
    let browser = Browser::default();
    let unbound: OAuth2State = serde_json::from_value(json!({"code_verifier":"legacy", "redirect_url":"/", "project_name":null, "custom_domain_base_url":null})).unwrap();
    fixture
        .state
        .token_store
        .save("unbound".into(), unbound.clone())
        .await
        .unwrap();
    assert!(oauth_callback(
        State(fixture.state.clone()),
        browser.headers("rise.example.com"),
        axum::extract::Query(CallbackQuery {
            code: "test-code".into(),
            state: "unbound".into()
        })
    )
    .await
    .is_err());
    crate::db::oauth_transient_state::insert(
        &fixture.state.db_pool,
        "expired",
        &unbound,
        std::time::Duration::ZERO,
    )
    .await
    .unwrap();
    assert!(oauth_callback(
        State(fixture.state.clone()),
        browser.headers("rise.example.com"),
        axum::extract::Query(CallbackQuery {
            code: "test-code".into(),
            state: "expired".into()
        })
    )
    .await
    .is_err());
    let unbound: CompletedAuthSession = serde_json::from_value(
        json!({"rise_jwt":"legacy", "max_age":3600, "redirect_url":"/", "project_name":"demo"}),
    )
    .unwrap();
    fixture
        .state
        .token_store
        .save_completed_session("unbound-completion".into(), unbound)
        .await
        .unwrap();
    assert!(oauth_complete(
        State(fixture.state.clone()),
        browser.headers("demo.apps.example.com"),
        axum::extract::Query(CompleteQuery {
            token: "unbound-completion".into()
        })
    )
    .await
    .is_err());
    assert_eq!(fixture.exchanges.load(Ordering::SeqCst), 0);
}

#[sqlx::test]
async fn browser_binding_handlers_bound_cookies_across_abandoned_starts(pool: PgPool) {
    let fixture = fixture(pool).await;
    let mut browser = Browser::default();
    let mut retained = Vec::new();
    for index in 0..300 {
        let response = oauth_signin_start(
            State(fixture.state.clone()),
            browser.headers("rise.example.com"),
            Uri::from_static("/api/v1/auth/signin/start"),
            axum::extract::Query(signin(None)),
        )
        .await
        .unwrap();
        browser.receive("rise.example.com", &response);
        let state = start_app(&fixture, &mut browser, "demo.apps.example.com").await;
        if index < 2 {
            retained.push(state);
        }
        for host in ["rise.example.com", "demo.apps.example.com"] {
            assert_eq!(browser.hosts[host].len(), 1, "one proof cookie per host");
            assert!(browser.headers(host)[header::COOKIE].as_bytes().len() < 100);
        }
    }
    let mut completions = Vec::new();
    for state in retained {
        let response = oauth_callback(
            State(fixture.state.clone()),
            browser.headers("rise.example.com"),
            axum::extract::Query(CallbackQuery {
                code: "test-code".into(),
                state,
            }),
        )
        .await
        .unwrap();
        browser.receive("rise.example.com", &response);
        completions.push(query(&location(&response), "token"));
    }
    for token in completions.into_iter().rev() {
        let response = oauth_complete(
            State(fixture.state.clone()),
            browser.headers("demo.apps.example.com"),
            axum::extract::Query(CompleteQuery { token }),
        )
        .await
        .unwrap();
        browser.receive("demo.apps.example.com", &response);
        assert_eq!(browser.hosts["rise.example.com"].len(), 1);
        assert_eq!(
            browser.hosts["demo.apps.example.com"].len(),
            2,
            "proof plus session"
        );
    }
    assert_eq!(fixture.exchanges.load(Ordering::SeqCst), 2);
}

#[sqlx::test]
async fn browser_binding_first_cookie_race_fails_closed_and_restart_recovers(pool: PgPool) {
    let fixture = fixture(pool).await;
    let mut browser = Browser::default();
    let mut starts = Vec::new();
    for _ in 0..2 {
        starts.push(
            oauth_signin_start(
                State(fixture.state.clone()),
                browser.headers("rise.example.com"),
                Uri::from_static("/api/v1/auth/signin/start"),
                axum::extract::Query(signin(None)),
            )
            .await
            .unwrap(),
        );
    }
    for start in &starts {
        browser.receive("rise.example.com", start);
    }
    assert_eq!(browser.hosts["rise.example.com"].len(), 1);
    assert!(oauth_callback(
        State(fixture.state.clone()),
        browser.headers("rise.example.com"),
        axum::extract::Query(CallbackQuery {
            code: "test-code".into(),
            state: query(&location(&starts[0]), "state")
        }),
    )
    .await
    .is_err());
    assert_eq!(fixture.exchanges.load(Ordering::SeqCst), 0);
    let restart = oauth_signin_start(
        State(fixture.state.clone()),
        browser.headers("rise.example.com"),
        Uri::from_static("/api/v1/auth/signin/start"),
        axum::extract::Query(signin(None)),
    )
    .await
    .unwrap();
    browser.receive("rise.example.com", &restart);
    for start in [&restart, &starts[1]] {
        let response = oauth_callback(
            State(fixture.state.clone()),
            browser.headers("rise.example.com"),
            axum::extract::Query(CallbackQuery {
                code: "test-code".into(),
                state: query(&location(start), "state"),
            }),
        )
        .await
        .unwrap();
        browser.receive("rise.example.com", &response);
    }
    assert_eq!(fixture.exchanges.load(Ordering::SeqCst), 2);
}

#[sqlx::test]
async fn logout_span_omits_credential_bearing_redirect(pool: PgPool) {
    use std::sync::Mutex;
    use tracing::{field::Visit, instrument::WithSubscriber};
    use tracing_subscriber::{layer::Context, prelude::*, Layer};

    #[derive(Clone)]
    struct Captured(Arc<Mutex<String>>);
    impl Visit for Captured {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0
                .lock()
                .unwrap()
                .push_str(&format!("{field}={value:?}"));
        }
    }
    impl<S: tracing::Subscriber> Layer<S> for Captured {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            _: &tracing::Id,
            _: Context<'_, S>,
        ) {
            self.0.lock().unwrap().push_str(attrs.metadata().name());
            attrs.record(&mut self.clone());
        }
        fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
            event.record(&mut self.clone());
        }
    }
    let fixture = fixture(pool).await;
    let captured = Captured(Arc::new(Mutex::new(String::new())));
    let subscriber = tracing_subscriber::registry().with(captured.clone());
    oauth_logout(
        State(fixture.state.clone()),
        axum::extract::Query(LogoutQuery {
            redirect: Some("https://rise.example.com/?token=logout-secret-marker".into()),
        }),
    )
    .with_subscriber(subscriber)
    .await
    .unwrap();
    let logs = captured.0.lock().unwrap();
    assert!(logs.contains("oauth_logout"), "logout span is captured");
    assert!(
        !logs.contains("logout-secret-marker"),
        "redirect credentials must not enter events or span fields: {logs}"
    );
}

fn assert_consumption_failure_does_not_expose_session(
    result: Result<Response, (axum::http::StatusCode, String)>,
) {
    let error = result.expect_err("failed state consumption must reject the prepared response");
    let response = error.into_response();
    assert_eq!(
        response.status(),
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    );
    assert!(
        !response.headers().contains_key(header::SET_COOKIE),
        "no session or proof cookie escapes the failure"
    );
    assert!(
        !response.headers().contains_key(header::LOCATION),
        "no success or completion redirect escapes the failure"
    );
}

#[sqlx::test]
async fn browser_completion_requires_successful_state_consumption(pool: PgPool) {
    use crate::db::oauth_transient_state::test_faults;
    let fixture = fixture(pool).await;
    let mut browser = Browser::default();
    let state = start_app(&fixture, &mut browser, "demo.apps.example.com").await;
    let response = oauth_callback(
        State(fixture.state.clone()),
        browser.headers("rise.example.com"),
        axum::extract::Query(CallbackQuery {
            code: "test-code".into(),
            state,
        }),
    )
    .await
    .unwrap();
    browser.receive("rise.example.com", &response);
    let token = query(&location(&response), "token");
    test_faults::reject_deletes(&fixture.state.db_pool)
        .await
        .unwrap();
    assert_consumption_failure_does_not_expose_session(
        oauth_complete(
            State(fixture.state.clone()),
            browser.headers("demo.apps.example.com"),
            axum::extract::Query(CompleteQuery {
                token: token.clone(),
            }),
        )
        .await,
    );
    test_faults::allow_deletes(&fixture.state.db_pool)
        .await
        .unwrap();
    test_faults::wait_until_released(&fixture.state.db_pool, &token)
        .await
        .unwrap();
    let response = oauth_complete(
        State(fixture.state.clone()),
        browser.headers("demo.apps.example.com"),
        axum::extract::Query(CompleteQuery {
            token: token.clone(),
        }),
    )
    .await
    .unwrap();
    assert!(response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .any(|cookie| cookie.to_str().unwrap().starts_with("rise_jwt=")));
    browser.receive("demo.apps.example.com", &response);
    assert!(
        oauth_complete(
            State(fixture.state.clone()),
            browser.headers("demo.apps.example.com"),
            axum::extract::Query(CompleteQuery { token }),
        )
        .await
        .is_err(),
        "successful consumption prevents replay with the shared nonce"
    );
}

#[sqlx::test]
async fn browser_callbacks_require_successful_state_consumption(pool: PgPool) {
    use crate::db::oauth_transient_state::test_faults;
    let fixture = fixture(pool).await;
    let mut browser = Browser::default();
    for app in [false, true] {
        let state = if app {
            start_app(&fixture, &mut browser, "demo.apps.example.com").await
        } else {
            let response = oauth_signin_start(
                State(fixture.state.clone()),
                browser.headers("rise.example.com"),
                Uri::from_static("/api/v1/auth/signin/start"),
                axum::extract::Query(signin(None)),
            )
            .await
            .unwrap();
            browser.receive("rise.example.com", &response);
            query(&location(&response), "state")
        };
        test_faults::reject_deletes(&fixture.state.db_pool)
            .await
            .unwrap();
        assert_consumption_failure_does_not_expose_session(
            oauth_callback(
                State(fixture.state.clone()),
                browser.headers("rise.example.com"),
                axum::extract::Query(CallbackQuery {
                    code: "test-code".into(),
                    state: state.clone(),
                }),
            )
            .await,
        );
        test_faults::allow_deletes(&fixture.state.db_pool)
            .await
            .unwrap();
        test_faults::wait_until_released(&fixture.state.db_pool, &state)
            .await
            .unwrap();
        let response = oauth_callback(
            State(fixture.state.clone()),
            browser.headers("rise.example.com"),
            axum::extract::Query(CallbackQuery {
                code: "test-code".into(),
                state: state.clone(),
            }),
        )
        .await
        .unwrap();
        if app {
            assert!(location(&response)
                .starts_with("https://demo.apps.example.com/.rise/auth/complete?token="));
        } else {
            assert!(response
                .headers()
                .get_all(header::SET_COOKIE)
                .iter()
                .any(|cookie| cookie.to_str().unwrap().starts_with("rise_jwt=")));
        }
        browser.receive("rise.example.com", &response);
        assert!(oauth_callback(
            State(fixture.state.clone()),
            browser.headers("rise.example.com"),
            axum::extract::Query(CallbackQuery {
                code: "test-code".into(),
                state
            }),
        )
        .await
        .is_err());
    }
}
