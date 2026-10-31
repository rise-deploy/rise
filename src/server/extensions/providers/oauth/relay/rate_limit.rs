//! Rate-limit layer for the relay's authorize, callback and token endpoints.

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{header, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};

use crate::server::client_ip::ClientIp;
use crate::server::rate_limit::{extract_session_key, rate_limit_response, OAuthRateLimiter};

/// A relay endpoint that counts against the OAuth rate limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitedEndpoint {
    Authorize,
    Callback,
    Token,
}

/// The project and endpoint of a rate-limited request to `/oidc/{project}/{extension}/…`,
/// read from the raw path. Discovery, JWKS, CORS preflights and paths the relay does not
/// serve are not limited.
pub fn limited_endpoint<'a>(method: &Method, path: &'a str) -> Option<(&'a str, LimitedEndpoint)> {
    let rest = path.strip_prefix("/oidc/")?;
    let mut segments = rest.split('/');
    let (project, _extension, endpoint) = (segments.next()?, segments.next()?, segments.next()?);
    if segments.next().is_some() || project.is_empty() {
        return None;
    }
    let endpoint = match (endpoint, method) {
        ("authorize", _) => LimitedEndpoint::Authorize,
        ("callback", _) => LimitedEndpoint::Callback,
        ("token", m) if m != Method::OPTIONS => LimitedEndpoint::Token,
        _ => return None,
    };
    Some((project, endpoint))
}

/// Counts authorize, callback and token requests per client address, session and
/// project, answering `429` with `Retry-After` once a budget is spent. The token
/// endpoint answers in RFC 6749 error form.
pub async fn rate_limit(
    State(limiter): State<Arc<OAuthRateLimiter>>,
    client_ip: ClientIp,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_owned();
    let Some((project, endpoint)) = limited_endpoint(request.method(), &path) else {
        return next.run(request).await;
    };
    let session_key = extract_session_key(request.headers());
    let Err(retry_after) = limiter
        .increment_and_check(&client_ip.rate_limit_key(), session_key.as_deref(), project)
        .await
    else {
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
    fn limits_authorize_callback_and_token_only() {
        let get = Method::GET;
        let post = Method::POST;
        assert_eq!(
            limited_endpoint(&get, "/oidc/demo/ext/authorize"),
            Some(("demo", LimitedEndpoint::Authorize))
        );
        assert_eq!(
            limited_endpoint(&get, "/oidc/demo/ext/callback"),
            Some(("demo", LimitedEndpoint::Callback))
        );
        assert_eq!(
            limited_endpoint(&post, "/oidc/demo/ext/token"),
            Some(("demo", LimitedEndpoint::Token))
        );
        for (method, path) in [
            (&Method::OPTIONS, "/oidc/demo/ext/token"),
            (&get, "/oidc/demo/ext/jwks"),
            (&get, "/oidc/demo/ext/.well-known/openid-configuration"),
            (&get, "/oidc/demo/ext/token/extra"),
            (&get, "/oidc/demo/token"),
            (&get, "/oidc//ext/token"),
            (&get, "/api/v1/demo/ext/token"),
            (&get, "/oidc"),
        ] {
            assert_eq!(limited_endpoint(method, path), None, "{method} {path}");
        }
    }
}
