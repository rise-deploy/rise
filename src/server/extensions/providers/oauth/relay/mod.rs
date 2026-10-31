//! Embeds `oauthrelay-core` to serve the OAuth extension's `/oidc/{project}/{extension}/*` endpoints.
//!
//! Every active `oauth` extension is one oauthrelay relay and upstream under the
//! key `{project}/{extension}` ([`resolver::ExtensionResolver`]), served at the
//! paths of [`layout::RiseLayout`]. Sealed values (flow state, authorization
//! codes, refresh-token handles) are sealed by the configured
//! [`oauthrelay_core::Sealer`]; single-use codes and rotating refresh-token
//! families live in the [`oauthrelay_core::TransientStore`].

pub mod http;
pub mod layout;
pub mod rate_limit;
pub mod resolver;
// Allow the placeholder sealer and store until their implementations land;
// drop both attributes at integration.
#[allow(dead_code)]
pub mod sealer;
#[allow(dead_code)]
pub mod store;

use std::sync::Arc;

use anyhow::Context;
use axum::{
    body::Body,
    extract::Request,
    http::{header, Method},
    response::{IntoResponse, Response},
    Router,
};
use oauthrelay_core::{RedirectOptions, RelayConfig, Sealer, TransientStore};
use sqlx::PgPool;
use tower::{Layer, ServiceExt};
use url::Url;

use crate::server::deployment::controller::DeploymentBackend;
use crate::server::encryption::EncryptionProvider;
use crate::server::rate_limit::OAuthRateLimiter;
use crate::server::ssrf::SsrfConfig;

/// The relay as a mountable service.
pub type RelayService =
    tower::util::BoxCloneSyncService<Request, Response, std::convert::Infallible>;

/// The path every relay endpoint is served below, relative to Rise's root.
pub const MOUNT_PREFIX: &str = "/oidc";

/// What the relay needs from Rise.
pub struct OAuthRelayDeps {
    pub pool: PgPool,
    pub deployment_backend: Arc<dyn DeploymentBackend>,
    /// Decrypts the extensions' upstream client secrets.
    pub encryption_provider: Arc<dyn EncryptionProvider>,
    /// Rise's public URL (`server.public_url`).
    pub public_url: String,
    pub ssrf: SsrfConfig,
    pub sealer: Arc<dyn Sealer>,
    pub store: Arc<dyn TransientStore>,
}

/// The OAuth extension relay: the oauthrelay router over Rise's extensions.
pub struct OAuthRelay {
    /// The oauthrelay router, without Rise's rate limit.
    router: Router,
    /// The path of `{public_url}/oidc`, which the oauthrelay router serves below.
    route_prefix: String,
    /// The path of Rise's public URL when it is not served at the root; requests
    /// arriving at `/oidc/…` are rewritten below it before routing.
    public_path: Option<String>,
    store: Arc<dyn TransientStore>,
}

impl OAuthRelay {
    pub fn new(deps: OAuthRelayDeps) -> anyhow::Result<Self> {
        let public_url = Url::parse(&deps.public_url).context("invalid server.public_url")?;
        let public_path = public_url.path().trim_end_matches('/').to_owned();
        let relay_url = Url::parse(&format!(
            "{}{MOUNT_PREFIX}",
            public_url.as_str().trim_end_matches('/')
        ))
        .context("invalid OAuth relay URL")?;
        let route_prefix = format!("{public_path}{MOUNT_PREFIX}");
        let public_path = if public_path.is_empty() {
            None
        } else {
            tracing::warn!(
                public_url = %deps.public_url,
                "server.public_url has a path; OAuth relay requests to /oidc/... are served below it"
            );
            Some(public_path)
        };
        // http app origins are a development convenience: accept them only when
        // Rise itself is served over http.
        // oauthrelay asks for `..Default::default()` so flags it adds later keep
        // their off default.
        #[allow(clippy::needless_update)]
        let redirect = RedirectOptions {
            allow_localhost_loopback: true,
            allow_http_origins: public_url.scheme() == "http",
            ..Default::default()
        };
        let resolver = resolver::ExtensionResolver::new(
            deps.pool,
            deps.deployment_backend,
            deps.encryption_provider,
            public_url,
            redirect,
            deps.ssrf.clone(),
        );
        let config = RelayConfig {
            public_url: relay_url,
            sealer: deps.sealer,
            store: Some(deps.store.clone()),
            http: http::relay_http_client(&deps.ssrf),
            client_assertion_http: http::client_assertion_http_client(&deps.ssrf)
                .context("failed to build the OAuth relay's client assertion HTTP client")?,
            redirect,
        };
        let router =
            oauthrelay_core::router(Arc::new(resolver), config, Arc::new(layout::RiseLayout));
        Ok(Self {
            router,
            route_prefix,
            public_path,
            store: deps.store,
        })
    }

    /// The relay endpoints for mounting at `/oidc/{*path}`, behind the OAuth rate
    /// limit on authorize, callback and token. A plain service rather than a
    /// `Router`, so it mounts with `route_service` and sees the unstripped path.
    pub fn service(&self, limiter: Arc<OAuthRateLimiter>) -> RelayService {
        let relay: RelayService =
            match &self.public_path {
                None => RelayService::new(self.router.clone()),
                Some(public_path) => {
                    let public_path = public_path.clone();
                    RelayService::new(self.router.clone().map_request(
                        move |mut request: Request| {
                            let rewritten = request.uri().path_and_query().and_then(|path| {
                                format!("{public_path}{path}")
                                    .parse::<axum::http::Uri>()
                                    .ok()
                            });
                            if let Some(uri) = rewritten {
                                *request.uri_mut() = uri;
                            }
                            request
                        },
                    ))
                }
            };
        RelayService::new(
            axum::middleware::from_fn_with_state(limiter, rate_limit::rate_limit).layer(relay),
        )
    }

    /// Calls the relay's token endpoint for `project`/`extension` in process, with
    /// `form` as the `application/x-www-form-urlencoded` body. In-process calls are
    /// not rate limited.
    pub async fn token(&self, project: &str, extension: &str, form: &[(&str, &str)]) -> Response {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(form)
            .finish();
        let request = Request::builder()
            .method(Method::POST)
            .uri(format!("{}/{project}/{extension}/token", self.route_prefix))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body));
        let request = match request {
            Ok(request) => request,
            Err(e) => {
                tracing::error!("Failed to build an in-process OAuth token request: {:?}", e);
                return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };
        match self.router.clone().oneshot(request).await {
            Ok(response) => response,
            Err(never) => match never {},
        }
    }

    /// The transient store shared with the relay.
    pub fn store(&self) -> &Arc<dyn TransientStore> {
        &self.store
    }
}
