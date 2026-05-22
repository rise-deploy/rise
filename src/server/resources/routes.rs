//! Routes for the generic resource API.
//!
//! All routes are mounted under `/api/v1` (the rise API namespace). The routes
//! themselves use `/resources` as the next segment to distinguish the generic
//! resource API from the existing typed APIs.
//!
//! All resource paths are dispatched through four handler functions that parse
//! the wildcard `{*path}` capture and classify it against the resource store.

use axum::{
    routing::{delete, get, post, put},
    Router,
};

use super::handlers;
use crate::server::state::AppState;

/// Wildcard route pattern for the generic resource API. `{*path}` captures the
/// entire `/`-separated tail in one segment, so `parse_resource_path` receives
/// the full resource path rather than just its first component.
const RESOURCE_PATH_ROUTE: &str = "/resources/{*path}";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(RESOURCE_PATH_ROUTE, get(handlers::dispatch_get))
        .route(RESOURCE_PATH_ROUTE, post(handlers::dispatch_post))
        .route(RESOURCE_PATH_ROUTE, put(handlers::dispatch_put))
        .route(RESOURCE_PATH_ROUTE, delete(handlers::dispatch_delete))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        extract::Path,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt; // for `oneshot`

    /// matchit (the router behind axum) panics at registration time when two
    /// routes are ambiguous. Building the full router here catches that early
    /// instead of at server startup. The router has no state at this stage —
    /// `Router::<AppState>` is generic over state until it's mounted.
    #[test]
    fn routes_build_without_conflict() {
        let _router: Router<crate::server::state::AppState> = routes();
    }

    /// HTTP-layer test for the resource-API route table.
    ///
    /// The real `dispatch_*` handlers extract `State<AppState>` and
    /// `AppState`-bound auth, and `AppState` has no test constructor — so they
    /// cannot be driven through a real `Router` here. Their behaviour is
    /// covered by `handlers::dispatch_tests` against `dispatch_*_inner`.
    ///
    /// This test instead pins the *routing contract* those handlers rely on:
    /// stub handlers are mounted on the same `RESOURCE_PATH_ROUTE` pattern and
    /// driven with real requests through an `axum::Router`, verifying that
    ///
    /// * the `{*path}` wildcard captures a full multi-segment path verbatim
    ///   (slashes, dots and `uid:` colons included) — `parse_resource_path`
    ///   depends on receiving the whole tail, so a regression of the pattern
    ///   to a non-wildcard `{path}` fails the multi-segment assertion below;
    /// * each of GET/POST/PUT/DELETE dispatches independently;
    /// * an unsupported method is a 405, and `/resources` with no tail a 404.
    #[tokio::test]
    async fn resource_route_dispatches_methods_and_captures_wildcard() {
        let app: Router = Router::new().route(
            RESOURCE_PATH_ROUTE,
            get(|Path(p): Path<String>| async move { format!("GET {p}") })
                .post(|Path(p): Path<String>| async move { format!("POST {p}") })
                .put(|Path(p): Path<String>| async move { format!("PUT {p}") })
                .delete(|Path(p): Path<String>| async move { format!("DELETE {p}") }),
        );

        // A deeply nested path in the compact grammar — ancestor names, a
        // version, and a `uid:`-style colon — must arrive at the handler whole.
        let nested = "example.dev/v1/widgets/acme/uid:0c0ffee0-dead-beef-cafe-000000000000/status";
        for method in ["GET", "POST", "PUT", "DELETE"] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(format!("/resources/{nested}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{method} should route");
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            assert_eq!(
                String::from_utf8_lossy(&body),
                format!("{method} {nested}"),
                "{method}: the wildcard must capture the full path verbatim"
            );
        }

        // An unsupported method on the route is a 405, not a 404.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/resources/example.dev/v1/widgets")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);

        // `/resources` with no tail is unrouted — the wildcard needs >=1 segment.
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/resources")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
