use crate::server::state::AppState;
use axum::{
    routing::{get, patch, post},
    Router,
};

pub fn deployment_routes() -> Router<AppState> {
    Router::new()
        .route("/deployments", post(super::handlers::create_deployment))
        .route(
            "/deployments/{deployment_id}/status",
            patch(super::handlers::update_deployment_status),
        )
        .route(
            "/projects/{project_name}/deployments",
            get(super::handlers::list_deployments),
        )
        .route(
            "/projects/{project_name}/deployment-groups",
            get(super::handlers::list_deployment_groups),
        )
        .route(
            "/projects/{project_name}/deployments/stop",
            post(super::handlers::stop_deployments_by_group),
        )
        .route(
            "/projects/{project_name}/deployments/{deployment_id}",
            get(super::handlers::get_deployment_by_project),
        )
        .route(
            "/projects/{project_name}/deployments/{deployment_id}/status",
            patch(super::handlers::update_deployment_status_by_project),
        )
        .route(
            "/projects/{project_name}/deployments/{deployment_id}/stop",
            post(super::handlers::stop_deployment),
        )
        .route(
            "/projects/{project_name}/deployments/{deployment_id}/logs",
            get(super::handlers::stream_deployment_logs),
        )
        .route(
            "/projects/{project_name}/deployments/{deployment_id}/logs/counts",
            get(super::handlers::count_deployment_logs),
        )
}

/// Metacontroller webhook routes (token-authenticated on an internal listener).
/// These are called by Metacontroller within the cluster and require a shared-secret
/// token passed as a `?token=` query parameter.
#[cfg(feature = "backend")]
pub fn metacontroller_routes() -> Router<AppState> {
    Router::new()
        .route("/metacontroller/sync", post(super::webhook::handle_sync))
        .route(
            "/metacontroller/finalize",
            post(super::webhook::handle_finalize),
        )
}
