use axum::{routing::post, Router};

use crate::server::auth::exchange::handlers;
use crate::server::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/auth/token", post(handlers::exchange))
}
