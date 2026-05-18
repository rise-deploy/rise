use axum::{
    body::{to_bytes, Body},
    extract::{Path, Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri},
    middleware::Next,
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
    Router,
};
use serde_json::json;

use crate::server::settings::ServerSettings;
use crate::server::state::AppState;

use super::load_static_file;

pub fn frontend_routes() -> Router<AppState> {
    Router::new()
        .route("/", get(serve_index))
        .fallback(fallback_handler)
}

pub fn docs_routes() -> Router<AppState> {
    Router::new()
        .route("/docs", get(serve_docs_index))
        .route("/docs/", get(serve_docs_index))
        .route("/docs/{*path}", get(serve_docs_path))
}

pub(crate) async fn docs_auth_middleware(
    State(state): State<AppState>,
    headers: HeaderMap,
    req: Request,
    next: Next,
) -> Response {
    let requested = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| "/docs/".to_string());

    match crate::server::auth::middleware::auth_middleware(State(state.clone()), headers, req, next)
        .await
    {
        Ok(response) => response,
        Err((StatusCode::UNAUTHORIZED, _)) => {
            let redirect_to = format!(
                "{}/api/v1/auth/signin/start?rd={}",
                state.public_url.trim_end_matches('/'),
                urlencoding::encode(&requested),
            );
            Redirect::temporary(&redirect_to).into_response()
        }
        Err((status, message)) => (status, message).into_response(),
    }
}

async fn serve_index(State(state): State<AppState>) -> Response {
    if state.server_settings.frontend_dev_proxy_url.is_some() {
        return proxy_to_vite(
            &state,
            Method::GET,
            Uri::from_static("/"),
            HeaderMap::new(),
            Body::empty(),
        )
        .await;
    }
    render_index(&state).await
}

async fn fallback_handler(State(state): State<AppState>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let path = parts.uri.path().trim_start_matches('/');

    // API route that wasn't matched - return 404
    if path == "api" || path.starts_with("api/") {
        return (StatusCode::NOT_FOUND, "Not found").into_response();
    }

    // Try to serve as static file first
    if let Some(ref static_dir) = state.server_settings.static_dir {
        if let Some(data) = load_static_file(static_dir, path).await {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            return Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime.as_ref())
                .header(header::CACHE_CONTROL, "public, max-age=3600")
                .body(Body::from(data))
                .unwrap();
        }
    }

    // In development, proxy frontend requests to Vite dev server.
    if state.server_settings.frontend_dev_proxy_url.is_some() {
        return proxy_to_vite(&state, parts.method, parts.uri, parts.headers, body).await;
    }

    // If not a static file, serve SPA index.html
    render_index(&state).await
}

async fn serve_docs_index(State(state): State<AppState>) -> Response {
    serve_docs_file(&state.server_settings, "").await
}

async fn serve_docs_path(State(state): State<AppState>, Path(path): Path<String>) -> Response {
    serve_docs_file(&state.server_settings, &path).await
}

async fn serve_docs_file(settings: &ServerSettings, rel: &str) -> Response {
    match load_docs_static_file(settings, rel).await {
        Some((bytes, served_path)) => {
            let mime = mime_guess::from_path(&served_path).first_or_octet_stream();
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime.as_ref())
                .header(header::CACHE_CONTROL, docs_cache_control(&served_path))
                .body(Body::from(bytes))
                .unwrap()
        }
        None => (StatusCode::NOT_FOUND, "Documentation content not found").into_response(),
    }
}

async fn load_docs_static_file(settings: &ServerSettings, rel: &str) -> Option<(Vec<u8>, String)> {
    let docs_dir = settings.docs_dir.as_deref()?;
    let rel = rel.trim_start_matches('/');

    let candidates = if rel.is_empty() {
        vec!["index.html".to_string()]
    } else if rel.ends_with('/') {
        vec![format!("{rel}index.html")]
    } else {
        vec![rel.to_string(), format!("{rel}/index.html")]
    };

    for candidate in candidates {
        if let Some(bytes) = load_static_file(docs_dir, &candidate).await {
            return Some((bytes, candidate));
        }
    }

    None
}

fn docs_cache_control(path: &str) -> HeaderValue {
    if path.starts_with("_astro/") {
        HeaderValue::from_static("public, max-age=31536000, immutable")
    } else {
        HeaderValue::from_static("no-cache")
    }
}

async fn render_index(state: &AppState) -> Response {
    // Load the Vite-generated index.html from the static directory
    let static_dir = match state.server_settings.static_dir.as_deref() {
        Some(dir) => dir,
        None => {
            tracing::error!("static_dir not configured, cannot serve index.html");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Static dir not configured",
            )
                .into_response();
        }
    };

    let html_content = match load_static_file(static_dir, "index.html").await {
        Some(data) => match std::str::from_utf8(&data) {
            Ok(s) => s.to_string(),
            Err(e) => {
                tracing::error!("Failed to parse index.html as UTF-8: {:?}", e);
                return (StatusCode::INTERNAL_SERVER_ERROR, "HTML encoding error").into_response();
            }
        },
        None => {
            tracing::error!("index.html not found in static_dir: {}", static_dir);
            return (StatusCode::INTERNAL_SERVER_ERROR, "HTML not found").into_response();
        }
    };

    // Build config object from backend settings
    let config = json!({
        "backendUrl": state.server_settings.public_url.trim_end_matches('/'),
        "issuerUrl": state.auth_settings.issuer,
        "authorizeUrl": state.oauth_client.authorize_url(),
        "clientId": state.auth_settings.client_id,
        "redirectUri": format!("{}/", state.server_settings.public_url.trim_end_matches('/')),
        "productionIngressUrlTemplate": state.production_ingress_url_template,
        "stagingIngressUrlTemplate": state.staging_ingress_url_template,
    });

    // Inject config by replacing the placeholder comment
    let config_injection = format!("window.CONFIG = {};", config);
    let html_with_config = html_content.replace("/*__RISE_CONFIG_INJECTION__*/", &config_injection);

    Html(html_with_config).into_response()
}

async fn proxy_to_vite(
    state: &AppState,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let vite_base = match state.server_settings.frontend_dev_proxy_url.as_deref() {
        Some(url) => url.trim_end_matches('/'),
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Frontend dev proxy is not configured",
            )
                .into_response();
        }
    };

    let mut target_url = format!("{vite_base}{}", uri.path());
    if let Some(query) = uri.query() {
        target_url.push('?');
        target_url.push_str(query);
    }

    let client = reqwest::Client::new();
    let mut upstream = client.request(method, target_url);

    for (name, value) in &headers {
        let name_str = name.as_str();
        if is_hop_by_hop_header(name_str) || name == header::HOST {
            continue;
        }
        upstream = upstream.header(name, value);
    }

    let body_bytes = match to_bytes(body, 10 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::warn!("Failed to read proxied request body: {:#}", e);
            return (
                StatusCode::BAD_REQUEST,
                "Invalid request body for frontend proxy",
            )
                .into_response();
        }
    };
    upstream = upstream.body(body_bytes);

    let upstream_response = match upstream.send().await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::warn!("Failed to reach Vite dev server: {:#}", e);
            return (
                StatusCode::BAD_GATEWAY,
                "Vite dev server is not reachable. Start it with `mise frontend:dev`.",
            )
                .into_response();
        }
    };

    let status = StatusCode::from_u16(upstream_response.status().as_u16())
        .unwrap_or(StatusCode::BAD_GATEWAY);
    let response_headers = upstream_response.headers().clone();
    let response_body = match upstream_response.bytes().await {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::warn!("Failed to read Vite proxy response body: {:#}", e);
            return (
                StatusCode::BAD_GATEWAY,
                "Invalid response from Vite dev server",
            )
                .into_response();
        }
    };

    let mut response_builder = Response::builder().status(status);
    for (name, value) in &response_headers {
        let name_str = name.as_str();
        if is_hop_by_hop_header(name_str) {
            continue;
        }
        response_builder = response_builder.header(name, value);
    }

    response_builder
        .body(Body::from(response_body))
        .unwrap_or_else(|_| {
            (
                StatusCode::BAD_GATEWAY,
                "Failed to build Vite proxy response",
            )
                .into_response()
        })
}

fn is_hop_by_hop_header(header_name: &str) -> bool {
    matches!(
        header_name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::settings::OAuthRateLimitSettings;

    fn docs_settings(docs_dir: String) -> ServerSettings {
        ServerSettings {
            host: "127.0.0.1".to_string(),
            port: 3000,
            public_url: "http://localhost:3000".to_string(),
            frontend_dev_proxy_url: None,
            cookie_secure: false,
            cookie_domain: None,
            jwt_signing_secret: "01234567890123456789012345678901".to_string(),
            rs256_private_key_pem: None,
            rs256_public_key_pem: None,
            jwt_claims: vec!["sub".to_string(), "email".to_string(), "name".to_string()],
            jwt_expiry_seconds: 86400,
            static_dir: None,
            docs_dir: Some(docs_dir),
            ssrf: Default::default(),
            oauth_rate_limit: OAuthRateLimitSettings::default(),
        }
    }

    #[tokio::test]
    async fn docs_static_file_serves_root_index() {
        let temp = tempfile::tempdir().unwrap();
        tokio::fs::write(temp.path().join("index.html"), "home")
            .await
            .unwrap();
        let settings = docs_settings(temp.path().to_string_lossy().to_string());

        let (bytes, path) = load_docs_static_file(&settings, "").await.unwrap();

        assert_eq!(bytes, b"home");
        assert_eq!(path, "index.html");
    }

    #[tokio::test]
    async fn docs_static_file_serves_nested_page_index() {
        let temp = tempfile::tempdir().unwrap();
        let page_dir = temp.path().join("user-guide/getting-started");
        tokio::fs::create_dir_all(&page_dir).await.unwrap();
        tokio::fs::write(page_dir.join("index.html"), "page")
            .await
            .unwrap();
        let settings = docs_settings(temp.path().to_string_lossy().to_string());

        let (bytes, path) = load_docs_static_file(&settings, "user-guide/getting-started")
            .await
            .unwrap();

        assert_eq!(bytes, b"page");
        assert_eq!(path, "user-guide/getting-started/index.html");
    }

    #[tokio::test]
    async fn docs_static_file_serves_exact_assets() {
        let temp = tempfile::tempdir().unwrap();
        let asset_dir = temp.path().join("_astro");
        tokio::fs::create_dir_all(&asset_dir).await.unwrap();
        tokio::fs::write(asset_dir.join("app.css"), "asset")
            .await
            .unwrap();
        let settings = docs_settings(temp.path().to_string_lossy().to_string());

        let (bytes, path) = load_docs_static_file(&settings, "_astro/app.css")
            .await
            .unwrap();

        assert_eq!(bytes, b"asset");
        assert_eq!(path, "_astro/app.css");
    }

    #[tokio::test]
    async fn docs_static_file_rejects_traversal() {
        let temp = tempfile::tempdir().unwrap();
        let settings = docs_settings(temp.path().to_string_lossy().to_string());

        let result = load_docs_static_file(&settings, "../secret.txt").await;

        assert!(result.is_none());
    }
}
