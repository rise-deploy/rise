use super::handlers;
use crate::server::state::AppState;
use axum::{
    routing::{get, post, put},
    Router,
};

/// Environment variables API routes
pub fn routes() -> Router<AppState> {
    Router::new()
        // Project environment variables
        .route(
            "/projects/{project_id_or_name}/env/{key}",
            put(handlers::set_project_env_var).delete(handlers::delete_project_env_var),
        )
        .route(
            "/projects/{project_id_or_name}/env/{key}/move",
            post(handlers::move_project_env_var),
        )
        .route(
            "/projects/{project_id_or_name}/env/{key}/value",
            get(handlers::get_project_env_var_value),
        )
        .route(
            "/projects/{project_id_or_name}/env",
            get(handlers::list_project_env_vars),
        )
        // Preview all env vars a deployment would receive
        .route(
            "/projects/{project_id_or_name}/env/preview",
            get(handlers::preview_deployment_env_vars),
        )
        // Deployment environment variables (read-only)
        .route(
            "/projects/{project_id_or_name}/deployments/{deployment_id}/env",
            get(handlers::list_deployment_env_vars),
        )
        .route(
            "/projects/{project_id_or_name}/deployments/{deployment_id}/env/diff",
            get(handlers::diff_deployment_env_vars),
        )
        .route(
            "/projects/{project_id_or_name}/deployments/{deployment_id}/env/{key}/value",
            get(handlers::get_deployment_env_var_value),
        )
}
