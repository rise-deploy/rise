use axum::{extract::State, http::HeaderMap, Json};
use sha2::{Digest, Sha256};

use crate::db::{
    deployments as db_deployments, environments as db_environments, projects as db_projects,
};
use crate::server::deployment::webhook::should_have_infrastructure;
use crate::server::error::{ServerError, ServerErrorExt};
use crate::server::state::AppState;
use crate::server::workload_tokens::models::{ExchangeTokenRequest, ExchangeTokenResponse};
use crate::server::workload_tokens::workload_subject;

/// Lifetime of exchange-endpoint workload identity tokens.
const WORKLOAD_TOKEN_TTL_SECS: u64 = 900;

/// Exchange a deployment's bootstrap credential for a workload identity token.
///
/// This route is unauthenticated: the bootstrap credential presented in the
/// `Authorization: Bearer` header *is* the authentication. A missing deployment
/// and a deployment without live infrastructure are both reported as an invalid
/// credential, so a caller cannot distinguish the two.
pub async fn exchange_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ExchangeTokenRequest>,
) -> Result<Json<ExchangeTokenResponse>, ServerError> {
    let credential = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .ok_or_else(|| ServerError::unauthorized("Missing bootstrap credential"))?;

    let audience = req.audience.trim();
    if audience.is_empty() {
        return Err(ServerError::bad_request("audience must not be empty"));
    }

    let hash = sha256_hex(credential.as_bytes());
    let deployment = db_deployments::get_by_identity_credential_hash(&state.db_pool, &hash)
        .await
        .internal_err("Failed to look up deployment")?
        .filter(should_have_infrastructure)
        .ok_or_else(|| ServerError::unauthorized("Invalid bootstrap credential"))?;

    let project = db_projects::find_by_id(&state.db_pool, deployment.project_id)
        .await
        .internal_err("Failed to load project")?
        .ok_or_else(|| ServerError::internal("Project not found for deployment"))?;

    let environment = match deployment.environment_id {
        Some(env_id) => db_environments::find_by_id(&state.db_pool, env_id)
            .await
            .internal_err("Failed to load environment")?
            .map(|e| e.name),
        None => None,
    };

    let sub = workload_subject(&project.name, environment.as_deref());

    let token = state
        .jwt_signer
        .sign_workload_jwt(
            &sub,
            &project.name,
            environment.as_deref().unwrap_or("_none"),
            &deployment.deployment_group,
            &deployment.deployment_id,
            audience,
            WORKLOAD_TOKEN_TTL_SECS,
        )
        .map_err(|e| ServerError::internal(format!("Failed to sign workload token: {:?}", e)))?;

    Ok(Json(ExchangeTokenResponse {
        token,
        token_type: "Bearer".to_string(),
        expires_in: WORKLOAD_TOKEN_TTL_SECS,
    }))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
