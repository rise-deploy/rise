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

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/resources/{*path}", get(handlers::dispatch_get))
        .route("/resources/{*path}", post(handlers::dispatch_post))
        .route("/resources/{*path}", put(handlers::dispatch_put))
        .route("/resources/{*path}", delete(handlers::dispatch_delete))
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
