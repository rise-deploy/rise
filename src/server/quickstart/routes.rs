use super::handlers;
use crate::server::state::AppState;
use axum::{routing::get, Router};

pub fn routes() -> Router<AppState> {
    Router::new().route(
        "/quickstart-templates",
        get(handlers::list_quickstart_templates),
    )
}
