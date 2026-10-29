use super::handlers;
use crate::server::state::AppState;
use axum::{
    routing::{get, post},
    Router,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        // Global route to list all available extension types
        .route("/extensions/types", get(handlers::list_extension_types))
        .route(
            "/projects/{project}/extensions/{extension}/oauth/credential-binding",
            post(super::providers::oauth::handlers::credential_binding),
        )
        // Project-specific extension routes
        .route(
            "/projects/{project}/extensions",
            get(handlers::list_extensions),
        )
        .route(
            "/projects/{project}/extensions/{extension}",
            post(handlers::create_extension)
                .put(handlers::update_extension)
                .patch(handlers::patch_extension)
                .get(handlers::get_extension)
                .delete(handlers::delete_extension),
        )
}
