use super::handlers;
use crate::server::state::AppState;
use axum::{
    routing::{delete, get, post, put},
    Router,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/projects/access-classes",
            get(handlers::list_access_classes),
        )
        .route("/projects", get(handlers::list_projects))
        .route(
            "/teams/{id_or_name}/projects",
            get(handlers::list_team_projects),
        )
        .route("/projects", post(handlers::create_project))
        .route("/projects/{id_or_name}", get(handlers::get_project))
        .route("/projects/{id_or_name}", put(handlers::update_project))
        .route("/projects/{id_or_name}", delete(handlers::delete_project))
        .route(
            "/projects/{id_or_name}/template-image",
            post(handlers::update_project_template_image),
        )
}
