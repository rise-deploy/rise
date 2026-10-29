use crate::db::projects;
use crate::server::auth::context::AuthContext;
use crate::server::encryption::EncryptionContext;
use crate::server::error::{ServerError, ServerErrorExt};
use crate::server::project::handlers::ensure_project_access_or_admin;
use crate::server::state::AppState;
use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};

/// A target may be sealed before its extension exists; the enclosing project must exist.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EncryptRequest {
    pub plaintext: String,
    pub project: String,
    pub extension_type: String,
    pub extension: String,
    pub slot: String,
    pub binding: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct EncryptResponse {
    pub encrypted: String,
}

/// Seal plaintext for an authorized target. Binding semantics belong to consumers.
pub async fn encrypt_handler(
    State(state): State<AppState>,
    auth: AuthContext,
    Json(req): Json<EncryptRequest>,
) -> Result<Json<EncryptResponse>, ServerError> {
    let user = auth.user()?;
    let project = projects::find_by_name(&state.db_pool, &req.project)
        .await
        .internal_err("Failed to look up encryption target")?
        .ok_or_else(|| ServerError::not_found("Project not found"))?;
    ensure_project_access_or_admin(&state, user, &project).await?;
    if state.extension_registry.get(&req.extension_type).is_none() {
        return Err(ServerError::bad_request("Unknown extension type"));
    }
    if req.extension.is_empty()
        || req.extension.len() > 255
        || !req.slot.starts_with('/')
        || req.slot.len() > 1024
        || req.binding.as_ref().is_some_and(|b| b.len() > 8192)
    {
        return Err(ServerError::bad_request(
            "Invalid credential scope, slot, or binding",
        ));
    }
    // The typed extension creation API uses the same parent-project access rule.
    let context =
        EncryptionContext::extension(project.id, &req.extension_type, &req.extension, &req.slot);
    let context = match req.binding {
        Some(binding) => context.with_binding(binding),
        None => context,
    };
    let key = format!("encrypt:{}", user.id);
    let count = state.encrypt_rate_limiter.get(&key).await.unwrap_or(0);
    if count >= 100 {
        return Err(ServerError::new(
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            "Encryption rate limit exceeded",
        ));
    }
    let provider = state
        .encryption_provider
        .as_ref()
        .ok_or_else(|| ServerError::bad_request("Encryption provider not configured"))?;
    let encrypted = provider
        .encrypt(&req.plaintext, &context)
        .await
        .internal_err("Failed to encrypt credential")?;
    state.encrypt_rate_limiter.insert(key, count + 1).await;
    Ok(Json(EncryptResponse { encrypted }))
}
