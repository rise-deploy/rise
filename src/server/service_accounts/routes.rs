use axum::{
    routing::{get, post},
    Router,
};

use crate::server::service_accounts::handlers;
use crate::server::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/projects/{project_name}/service-accounts",
            post(handlers::create_service_account).get(handlers::list_service_accounts),
        )
        .route(
            "/projects/{project_name}/service-accounts/{sa_id}",
            get(handlers::get_service_account)
                .put(handlers::update_service_account)
                .delete(handlers::delete_service_account),
        )
}
