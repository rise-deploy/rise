use super::handlers;
use crate::server::state::AppState;
use axum::{
    routing::{get, post, put},
    Router,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/projects/{project_id_or_name}/domains",
            post(handlers::add_custom_domain).get(handlers::list_custom_domains),
        )
        .route(
            "/projects/{project_id_or_name}/domains/{domain}",
            get(handlers::get_custom_domain)
                .patch(handlers::update_custom_domain)
                .delete(handlers::delete_custom_domain),
        )
        .route(
            "/projects/{project_id_or_name}/domains/{domain}/primary",
            put(handlers::set_primary_domain).delete(handlers::unset_primary_domain),
        )
}
