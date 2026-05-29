use super::handlers;
use crate::server::state::AppState;
use axum::{routing::get, Router};

pub fn routes() -> Router<AppState> {
    Router::new().route(
        "/platform/capabilities",
        get(handlers::platform_capabilities),
    )
}
