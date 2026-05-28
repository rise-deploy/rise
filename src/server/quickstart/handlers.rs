use super::models::ListQuickstartTemplatesResponse;
use super::registry;
use crate::server::auth::context::AuthContext;
use crate::server::error::ServerError;
use axum::Json;

/// List all available quickstart templates.
///
/// Returns the curated catalog of stateless container images users can deploy
/// in one click. Catalog is static (defined in code), so the endpoint takes
/// no parameters and does not depend on the project context.
pub async fn list_quickstart_templates(
    auth: AuthContext,
) -> Result<Json<ListQuickstartTemplatesResponse>, ServerError> {
    let _user = auth.user()?;
    Ok(Json(ListQuickstartTemplatesResponse {
        templates: registry::all(),
    }))
}
