use axum::{routing::post, Router};

use crate::server::state::AppState;
use crate::server::workload_tokens::handlers;

pub fn routes() -> Router<AppState> {
    Router::new().route("/identity/token", post(handlers::exchange_token))
}
