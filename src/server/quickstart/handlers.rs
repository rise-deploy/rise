use super::models::ListQuickstartTemplatesResponse;
use crate::server::auth::context::AuthContext;
use crate::server::error::ServerError;
use crate::server::state::AppState;
use axum::{extract::State, Json};

/// List the configured quickstart catalog.
///
/// The catalog is resolved from `settings.quickstart` at startup; the
/// `default.yaml` config layer ships the built-in entries and run-mode /
/// `local.yaml` layers can replace them. The handler simply hands back the
/// pre-resolved list — no per-request work.
pub async fn list_quickstart_templates(
    State(state): State<AppState>,
    auth: AuthContext,
) -> Result<Json<ListQuickstartTemplatesResponse>, ServerError> {
    let _user = auth.user()?;
    Ok(Json(ListQuickstartTemplatesResponse {
        templates: (*state.quickstart_templates).clone(),
    }))
}
