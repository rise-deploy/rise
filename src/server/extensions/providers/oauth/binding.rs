//! The credential-binding endpoint used to seal an OAuth extension's upstream secret.

use super::endpoints::resolve_oauth_endpoints;
use super::models::OAuthExtensionSpec;
use crate::server::state::AppState;
use axum::{
    extract::{Path, State},
    Json,
};

/// Derive an opaque upstream binding for clients sealing a not-yet-created OAuth extension.
pub async fn credential_binding(
    State(state): State<AppState>,
    auth: crate::server::auth::context::AuthContext,
    Path((project_name, _extension_name)): Path<(String, String)>,
    Json(spec): Json<OAuthExtensionSpec>,
) -> Result<Json<serde_json::Value>, crate::server::error::ServerError> {
    use crate::server::error::{ServerError, ServerErrorExt};
    let user = auth.user()?;
    let project = crate::db::projects::find_by_name(&state.db_pool, &project_name)
        .await
        .internal_err("Failed to find project")?
        .ok_or_else(|| ServerError::not_found("Project not found"))?;
    crate::server::project::handlers::ensure_project_access_or_admin(&state, user, &project)
        .await?;
    let endpoints = resolve_oauth_endpoints(&spec, &state.server_settings.ssrf)
        .await
        .map_err(|_| ServerError::bad_request("Failed to resolve OAuth endpoints"))?;
    crate::server::ssrf::validate_url(&endpoints.token_endpoint, &state.server_settings.ssrf)
        .await
        .map_err(|_| ServerError::bad_request("OAuth token endpoint is not permitted"))?;
    let binding = rise_deployment_spec::credential_binding::oauth_credential_binding(
        &serde_json::to_value(spec).internal_err("Invalid OAuth spec")?,
        &endpoints.token_endpoint,
    );
    Ok(Json(serde_json::json!({"binding": binding})))
}
