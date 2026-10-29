use axum::{extract::Request, http::header, middleware::Next, response::Response};
use uuid::Uuid;

/// Request ID stored in request extensions for correlation and debugging
#[derive(Clone, Debug)]
pub struct RequestId(pub Uuid);

/// Middleware that generates and injects a unique request ID for each request.
///
/// The request ID is:
/// - Generated as a UUID v4
/// - Stored in request extensions for use by handlers
/// - Added to response headers as `x-request-id` for client-side debugging
///
/// # Example
///
/// ```rust,ignore
/// use axum::{Router, middleware};
/// use crate::server::middleware::request_id_middleware;
///
/// let app = Router::new()
///     .layer(middleware::from_fn(request_id_middleware));
/// ```
pub async fn request_id_middleware(mut request: Request, next: Next) -> Response {
    // Generate a unique request ID
    let request_id = RequestId(Uuid::new_v4());

    // Store in request extensions for handlers to access
    request.extensions_mut().insert(request_id.clone());

    // Process the request
    let mut response = next.run(request).await;

    // Add request ID to response headers
    response.headers_mut().insert(
        header::HeaderName::from_static("x-request-id"),
        header::HeaderValue::from_str(&request_id.0.to_string())
            .unwrap_or_else(|_| header::HeaderValue::from_static("invalid")),
    );

    response
}

/// Middleware that forbids every page Rise serves from being framed.
///
/// The UI, the sign-in and device-approval pages, and the `/.rise/auth/*`
/// pages on app hosts all act on the viewer's session with a click, so a page
/// that frames them can trick a signed-in user into approving something.
/// `frame-ancestors 'none'` is the CSP control; `X-Frame-Options: DENY` covers
/// browsers that predate it. A `Content-Security-Policy` a handler already set
/// is extended rather than replaced.
pub async fn frame_protection_middleware(request: Request, next: Next) -> Response {
    const FRAME_ANCESTORS: &str = "frame-ancestors 'none'";

    let mut response = next.run(request).await;
    let headers = response.headers_mut();

    let csp = match headers
        .get(header::CONTENT_SECURITY_POLICY)
        .and_then(|value| value.to_str().ok())
    {
        Some(existing) if existing.contains("frame-ancestors") => None,
        Some(existing) if !existing.trim().is_empty() => header::HeaderValue::from_str(&format!(
            "{}; {FRAME_ANCESTORS}",
            existing.trim().trim_end_matches(';')
        ))
        .ok(),
        _ => Some(header::HeaderValue::from_static(FRAME_ANCESTORS)),
    };
    if let Some(csp) = csp {
        headers.insert(header::CONTENT_SECURITY_POLICY, csp);
    }
    headers.insert(
        header::X_FRAME_OPTIONS,
        header::HeaderValue::from_static("DENY"),
    );

    response
}

/// Render `url` for a log line without its query string or fragment.
///
/// Redirect URLs carry one-time credentials there (OAuth authorization codes,
/// `state`, ingress completion tokens), so logs keep only scheme, host and path.
pub fn url_for_log(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(mut parsed) => {
            let had_query = parsed.query().is_some() || parsed.fragment().is_some();
            parsed.set_query(None);
            parsed.set_fragment(None);
            if had_query {
                format!("{parsed}?<redacted>")
            } else {
                parsed.to_string()
            }
        }
        Err(_) => match url.find(['?', '#']) {
            Some(index) => format!("{}?<redacted>", &url[..index]),
            None => url.to_string(),
        },
    }
}

/// Helper for extracting request metadata for logging context
///
/// # Example
///
/// ```rust,ignore
/// use axum::extract::Request;
/// use crate::server::middleware::RequestMeta;
///
/// async fn handler(request: Request) {
///     let meta = RequestMeta::from_request(&request);
///     tracing::info!(
///         request_id = %meta.request_id,
///         uri = %meta.uri,
///         "Processing request"
///     );
/// }
/// ```
#[allow(dead_code)]
#[derive(Debug)]
pub struct RequestMeta {
    pub request_id: Option<Uuid>,
    pub uri: String,
    pub user_email: Option<String>,
}

#[allow(dead_code)]
impl RequestMeta {
    /// Extract request metadata from an Axum request
    pub fn from_request(request: &Request) -> Self {
        let request_id = request.extensions().get::<RequestId>().map(|rid| rid.0);

        let uri = request.uri().to_string();

        // Try to extract user email from extensions (if authenticated)
        let user_email = request
            .extensions()
            .get::<crate::db::models::User>()
            .map(|user| user.email.clone());

        Self {
            request_id,
            uri,
            user_email,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_for_log_withholds_query_and_fragment() {
        assert_eq!(
            url_for_log("https://app.example.com/callback?code=s3cr3t&state=xyz"),
            "https://app.example.com/callback?<redacted>"
        );
        assert_eq!(
            url_for_log("https://app.example.com/cb#access_token=s3cr3t"),
            "https://app.example.com/cb?<redacted>"
        );
        assert_eq!(
            url_for_log("https://app.example.com/path"),
            "https://app.example.com/path"
        );
        assert_eq!(
            url_for_log("/relative/path?token=s3cr3t"),
            "/relative/path?<redacted>"
        );
    }
    use axum::{
        body::Body,
        http::{Request as HttpRequest, StatusCode},
        routing::get,
        Router,
    };
    use tower::ServiceExt; // for `oneshot`

    async fn test_handler() -> &'static str {
        "Hello, World!"
    }

    #[tokio::test]
    async fn test_request_id_middleware_adds_header() {
        // Create a simple app with request ID middleware
        let app = Router::new()
            .route("/", get(test_handler))
            .layer(axum::middleware::from_fn(request_id_middleware));

        // Make a request
        let request = HttpRequest::builder().uri("/").body(Body::empty()).unwrap();

        let response = app.oneshot(request).await.unwrap();

        // Check that x-request-id header is present
        assert!(response.headers().contains_key("x-request-id"));

        let request_id_header = response.headers().get("x-request-id").unwrap();
        let request_id_str = request_id_header.to_str().unwrap();

        // Verify it's a valid UUID
        assert!(uuid::Uuid::parse_str(request_id_str).is_ok());

        // Verify response is successful
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn frame_protection_denies_framing_on_every_response() {
        async fn with_csp() -> impl axum::response::IntoResponse {
            (
                [(header::CONTENT_SECURITY_POLICY, "default-src 'self';")],
                "page",
            )
        }

        let app = Router::new()
            .route("/", get(test_handler))
            .route("/csp", get(with_csp))
            .layer(axum::middleware::from_fn(frame_protection_middleware));

        for (uri, expected_csp) in [
            ("/", "frame-ancestors 'none'"),
            ("/csp", "default-src 'self'; frame-ancestors 'none'"),
            ("/missing", "frame-ancestors 'none'"),
        ] {
            let response = app
                .clone()
                .oneshot(HttpRequest::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                response.headers().get(header::X_FRAME_OPTIONS).unwrap(),
                "DENY",
                "{uri}"
            );
            assert_eq!(
                response
                    .headers()
                    .get(header::CONTENT_SECURITY_POLICY)
                    .unwrap(),
                expected_csp,
                "{uri}"
            );
        }
    }

    #[tokio::test]
    async fn test_request_id_stored_in_extensions() {
        use axum::extract::Extension;

        // Handler that extracts request ID from extensions
        async fn handler_with_extension(Extension(request_id): Extension<RequestId>) -> String {
            request_id.0.to_string()
        }

        let app = Router::new()
            .route("/", get(handler_with_extension))
            .layer(axum::middleware::from_fn(request_id_middleware));

        let request = HttpRequest::builder().uri("/").body(Body::empty()).unwrap();

        let response = app.oneshot(request).await.unwrap();

        // Verify the extension was set by checking that handler didn't panic
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_request_meta_from_request() {
        // Create a request with RequestId extension
        let request_id = RequestId(Uuid::new_v4());
        let mut request = HttpRequest::builder()
            .uri("/test/path")
            .body(Body::empty())
            .unwrap();

        request.extensions_mut().insert(request_id.clone());

        // Extract metadata
        let meta = RequestMeta::from_request(&request);

        assert_eq!(meta.request_id, Some(request_id.0));
        assert_eq!(meta.uri, "/test/path");
        assert_eq!(meta.user_email, None);
    }
}
