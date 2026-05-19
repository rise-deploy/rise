//! Routes for the generic resource API.
//!
//! All routes are mounted under `/api/v1` (the rise API namespace). The routes
//! themselves use `/resources` as the next segment to distinguish the generic
//! resource API from the existing typed APIs.

use axum::{
    routing::{get, post, put},
    Router,
};

use super::handlers;
use crate::server::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        // Break-glass: orphan discovery (must come before /{collection}).
        .route("/resources/orphans", get(handlers::list_orphans))
        // Root-scoped subresource endpoints (3 path segments after /resources/).
        .route(
            "/resources/{collection}/{name}/reparent",
            post(handlers::reparent_root),
        )
        .route(
            "/resources/{collection}/{name}/status",
            put(handlers::update_status_root),
        )
        .route(
            "/resources/{collection}/{name}/finalizers",
            put(handlers::update_finalizers_root),
        )
        // Root-scoped CRUD.
        .route(
            "/resources/{collection}",
            get(handlers::list_root).post(handlers::create_root),
        )
        .route(
            "/resources/{collection}/{name}",
            get(handlers::get_root)
                .put(handlers::update_root)
                .delete(handlers::delete_root),
        )
        // Organization-scoped subresource endpoints.
        .route(
            "/resources/organizations/{org}/{collection}/{name}/reparent",
            post(handlers::reparent_org),
        )
        .route(
            "/resources/organizations/{org}/{collection}/{name}/status",
            put(handlers::update_status_org),
        )
        .route(
            "/resources/organizations/{org}/{collection}/{name}/finalizers",
            put(handlers::update_finalizers_org),
        )
        // Organization-scoped CRUD.
        .route(
            "/resources/organizations/{org}/{collection}",
            get(handlers::list_org).post(handlers::create_org),
        )
        .route(
            "/resources/organizations/{org}/{collection}/{name}",
            get(handlers::get_org)
                .put(handlers::update_org)
                .delete(handlers::delete_org),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// matchit (the router behind axum) panics at registration time when two
    /// routes are ambiguous. Building the full router here catches that early
    /// instead of at server startup. The router has no state at this stage —
    /// `Router::<AppState>` is generic over state until it's mounted.
    #[test]
    fn routes_build_without_conflict() {
        let _router: Router<crate::server::state::AppState> = routes();
    }
}

