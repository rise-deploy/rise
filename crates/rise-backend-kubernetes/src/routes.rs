//! The Metacontroller webhook router.
//!
//! Mounted by `rise-deploy` on an internal, IP-validated listener; the crate
//! owns the routes so the handlers' state never has to cross the boundary.

use std::sync::Arc;

use axum::routing::post;
use axum::Router;

use crate::webhook::{self, WebhookContext};

/// Routes Metacontroller calls: sync computes a project's desired children,
/// finalize tears them down.
///
/// Fully stated — the caller nests this under its own prefix and adds its own
/// middleware. Handlers extract `ConnectInfo<SocketAddr>` for source-IP
/// validation, so the server must be started with
/// `into_make_service_with_connect_info::<SocketAddr>()`.
pub fn metacontroller_router(ctx: Arc<WebhookContext>) -> Router<()> {
    Router::new()
        .route("/metacontroller/sync", post(webhook::handle_sync))
        .route("/metacontroller/finalize", post(webhook::handle_finalize))
        .with_state(ctx)
}
