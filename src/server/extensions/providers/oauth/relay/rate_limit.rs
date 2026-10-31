//! Rate limits for the relay's endpoints.
//!
//! Authorize, callback and token count against Rise's OAuth rate limit
//! (`server.oauth_rate_limit`, per project, client address and session).
//! Discovery and JWKS are unauthenticated and polled by every app's OAuth
//! library, so they get their own, generous per-address budget
//! ([`METADATA_PER_IP_MAX`] per minute).

use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};
use std::time::Duration;

use axum::{
    extract::{Request, State},
    http::{header, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};

use crate::server::client_ip::ClientIp;
use crate::server::rate_limit::{extract_session_key, rate_limit_response, OAuthRateLimiter};

/// Discovery and JWKS requests one client address may make per window.
pub const METADATA_PER_IP_MAX: u32 = 600;
/// The window of [`METADATA_PER_IP_MAX`].
pub const METADATA_WINDOW: Duration = Duration::from_secs(60);

/// A relay endpoint that counts against a rate limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitedEndpoint {
    Authorize,
    Callback,
    Token,
    /// Discovery or JWKS.
    Metadata,
}

/// The project and endpoint of a rate-limited request to `/oidc/{project}/{extension}/…`,
/// read from the raw path. CORS preflights and paths the relay does not serve are not
/// limited.
pub fn limited_endpoint<'a>(method: &Method, path: &'a str) -> Option<(&'a str, LimitedEndpoint)> {
    let rest = path.strip_prefix("/oidc/")?;
    let mut segments = rest.split('/');
    let (project, _extension, endpoint) = (segments.next()?, segments.next()?, segments.next()?);
    let tail = segments.next();
    if project.is_empty() {
        return None;
    }
    let endpoint = match (endpoint, tail, method) {
        ("authorize", None, _) => LimitedEndpoint::Authorize,
        ("callback", None, _) => LimitedEndpoint::Callback,
        ("token", None, m) if m != Method::OPTIONS => LimitedEndpoint::Token,
        ("jwks", None, _) => LimitedEndpoint::Metadata,
        (".well-known", Some("openid-configuration"), _) if segments.next().is_none() => {
            LimitedEndpoint::Metadata
        }
        _ => return None,
    };
    Some((project, endpoint))
}

/// Fixed-window counter of discovery and JWKS requests per client address.
pub struct MetadataRateLimiter {
    per_ip: moka::future::Cache<String, Arc<AtomicU32>>,
    max: u32,
    window: Duration,
}

impl MetadataRateLimiter {
    pub fn new(max: u32, window: Duration) -> Self {
        Self {
            per_ip: moka::future::Cache::builder()
                .max_capacity(50_000)
                .time_to_live(window)
                .build(),
            max,
            window,
        }
    }

    /// Counts one request; `Err(retry_after_secs)` once the budget is spent.
    pub async fn increment_and_check(&self, ip: &str) -> Result<(), u64> {
        let counter = self
            .per_ip
            .entry_by_ref(ip)
            .or_insert_with(std::future::ready(Arc::new(AtomicU32::new(0))))
            .await
            .into_value();
        if counter.fetch_add(1, Ordering::Relaxed) + 1 > self.max {
            return Err(self.window.as_secs());
        }
        Ok(())
    }
}

impl Default for MetadataRateLimiter {
    fn default() -> Self {
        Self::new(METADATA_PER_IP_MAX, METADATA_WINDOW)
    }
}

/// The rate limiters the relay's middleware applies.
#[derive(Clone)]
pub struct RelayLimits {
    pub oauth: Arc<OAuthRateLimiter>,
    pub metadata: Arc<MetadataRateLimiter>,
}

/// Applies the rate limits, answering `429` with `Retry-After` once a budget is
/// spent. The token endpoint answers in RFC 6749 error form.
pub async fn rate_limit(
    State(limits): State<RelayLimits>,
    client_ip: ClientIp,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_owned();
    let Some((project, endpoint)) = limited_endpoint(request.method(), &path) else {
        return next.run(request).await;
    };
    let ip = client_ip.rate_limit_key();
    let checked = if endpoint == LimitedEndpoint::Metadata {
        limits.metadata.increment_and_check(&ip).await
    } else {
        let session_key = extract_session_key(request.headers());
        limits
            .oauth
            .increment_and_check(&ip, session_key.as_deref(), project)
            .await
    };
    let Err(retry_after) = checked else {
        return next.run(request).await;
    };
    tracing::warn!(
        ip = %client_ip,
        project = %project,
        endpoint = ?endpoint,
        "OAuth relay endpoint rate limited"
    );
    if endpoint != LimitedEndpoint::Token {
        return rate_limit_response(retry_after);
    }
    let mut response = (
        StatusCode::TOO_MANY_REQUESTS,
        Json(serde_json::json!({
            "error": "rate_limit_exceeded",
            "error_description": "Rate limit exceeded. Please try again later.",
        })),
    )
        .into_response();
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Ok(value) = HeaderValue::from_str(&retry_after.to_string()) {
        headers.insert(header::RETRY_AFTER, value);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_the_relay_endpoints() {
        let get = Method::GET;
        let post = Method::POST;
        for (method, path, expected) in [
            (&get, "/oidc/demo/ext/authorize", LimitedEndpoint::Authorize),
            (&get, "/oidc/demo/ext/callback", LimitedEndpoint::Callback),
            (&post, "/oidc/demo/ext/token", LimitedEndpoint::Token),
            (&get, "/oidc/demo/ext/jwks", LimitedEndpoint::Metadata),
            (
                &get,
                "/oidc/demo/ext/.well-known/openid-configuration",
                LimitedEndpoint::Metadata,
            ),
        ] {
            assert_eq!(
                limited_endpoint(method, path),
                Some(("demo", expected)),
                "{method} {path}"
            );
        }
        for (method, path) in [
            (&Method::OPTIONS, "/oidc/demo/ext/token"),
            (&get, "/oidc/demo/ext/token/extra"),
            (&get, "/oidc/demo/ext/.well-known/openid-configuration/x"),
            (&get, "/oidc/demo/ext/.well-known"),
            (&get, "/oidc/demo/token"),
            (&get, "/oidc//ext/token"),
            (&get, "/api/v1/demo/ext/token"),
            (&get, "/oidc"),
        ] {
            assert_eq!(limited_endpoint(method, path), None, "{method} {path}");
        }
    }

    #[tokio::test]
    async fn metadata_budget_is_per_address() {
        let limiter = MetadataRateLimiter::new(2, Duration::from_secs(60));
        assert!(limiter.increment_and_check("a").await.is_ok());
        assert!(limiter.increment_and_check("a").await.is_ok());
        assert_eq!(limiter.increment_and_check("a").await, Err(60));
        assert!(limiter.increment_and_check("b").await.is_ok());
    }
}
