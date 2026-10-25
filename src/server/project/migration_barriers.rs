//! Migration compatibility barriers (ADR-0007): the project's append-only
//! ledger of migration versions that older code must not be rolled back past.
//!
//! Members record barriers, not only admins: a barrier only ever restricts
//! which deployments may be created, so recording one grants nothing.

use crate::db::{migration_barriers as db_barriers, projects};
use crate::server::auth::context::AuthContext;
use crate::server::error::{ServerError, ServerErrorExt};
use crate::server::project::handlers::ensure_project_access_or_admin;
use crate::server::state::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

/// Mirrored by the `migration_barriers.reason` CHECK constraint.
const MAX_REASON_CHARS: usize = 1024;

#[derive(Debug, Deserialize)]
pub struct CreateMigrationBarrierRequest {
    pub version: i64,
    pub reason: String,
}

#[derive(Debug, Serialize)]
pub struct MigrationBarrierResponse {
    pub version: i64,
    pub reason: String,
    /// Email of whoever recorded it, while that user still exists.
    pub recorded_by: Option<String>,
    pub recorded_at: String,
}

impl From<db_barriers::MigrationBarrier> for MigrationBarrierResponse {
    fn from(barrier: db_barriers::MigrationBarrier) -> Self {
        Self {
            version: barrier.version,
            reason: barrier.reason,
            recorded_by: barrier.recorded_by_email,
            recorded_at: barrier.recorded_at.to_rfc3339(),
        }
    }
}

impl CreateMigrationBarrierRequest {
    /// Returns the trimmed reason.
    fn validate(&self) -> Result<&str, ServerError> {
        if self.version <= 0 {
            return Err(ServerError::bad_request(format!(
                "Migration barrier version must be greater than 0 (got {})",
                self.version
            )));
        }
        let reason = self.reason.trim();
        if reason.is_empty() {
            return Err(ServerError::bad_request(
                "A migration barrier needs a reason: what breaks if older code runs against the migrated schema",
            ));
        }
        let chars = reason.chars().count();
        if chars > MAX_REASON_CHARS {
            return Err(ServerError::bad_request(format!(
                "Migration barrier reason is too long ({chars} characters, at most {MAX_REASON_CHARS})"
            )));
        }
        Ok(reason)
    }
}

/// Resolve the project and check the caller may access it.
async fn authorized_project(
    state: &AppState,
    auth: &AuthContext,
    id_or_name: &str,
) -> Result<crate::db::models::Project, ServerError> {
    let project = match id_or_name.parse() {
        Ok(uuid) => projects::find_by_id(&state.db_pool, uuid).await,
        Err(_) => projects::find_by_name(&state.db_pool, id_or_name).await,
    }
    .internal_err("Failed to get project")?
    .ok_or_else(|| ServerError::not_found("Project not found"))?;
    ensure_project_access_or_admin(state, auth.user()?, &project).await?;
    Ok(project)
}

/// List the project's migration barriers by ascending version.
pub async fn list_migration_barriers(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id_or_name): Path<String>,
) -> Result<Json<Vec<MigrationBarrierResponse>>, ServerError> {
    let project = authorized_project(&state, &auth, &id_or_name).await?;
    let barriers = db_barriers::list_for_project(&state.db_pool, project.id)
        .await
        .internal_err("Failed to list migration barriers")?;
    Ok(Json(barriers.into_iter().map(Into::into).collect()))
}

/// Record a migration barrier. A version can be recorded once per project.
pub async fn create_migration_barrier(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id_or_name): Path<String>,
    Json(payload): Json<CreateMigrationBarrierRequest>,
) -> Result<(StatusCode, Json<MigrationBarrierResponse>), ServerError> {
    let project = authorized_project(&state, &auth, &id_or_name).await?;
    let reason = payload.validate()?;

    let barrier = db_barriers::create(
        &state.db_pool,
        project.id,
        payload.version,
        reason,
        Some(auth.user()?.id),
    )
    .await
    .map_err(|e| {
        if db_barriers::is_duplicate(&e) {
            ServerError::conflict(format!(
                "Project '{}' already has a migration barrier at version {}",
                project.name, payload.version
            ))
            .with_error_code("migration_barrier_exists")
        } else {
            ServerError::internal_anyhow(e, "Failed to record migration barrier")
        }
    })?;

    tracing::info!(project = %project.name, version = barrier.version, "Recorded migration barrier");
    Ok((StatusCode::CREATED, Json(barrier.into())))
}

#[cfg(test)]
mod tests {
    use super::{CreateMigrationBarrierRequest, MAX_REASON_CHARS};

    fn validate(version: i64, reason: &str) -> Result<String, ()> {
        let req = CreateMigrationBarrierRequest {
            version,
            reason: reason.to_string(),
        };
        req.validate().map(str::to_string).map_err(|_| ())
    }

    #[test]
    fn validation() {
        assert!(validate(0, "drops users.legacy_name").is_err());
        assert!(validate(-3, "drops users.legacy_name").is_err());
        assert!(validate(1, "drops users.legacy_name").is_ok());
        assert!(validate(5, "").is_err());
        assert!(validate(5, "   ").is_err());
        assert_eq!(validate(5, "  renames orders  ").unwrap(), "renames orders");
        // The bound counts characters, not bytes.
        assert!(validate(5, &"ä".repeat(MAX_REASON_CHARS)).is_ok());
        assert!(validate(5, &"a".repeat(MAX_REASON_CHARS + 1)).is_err());
    }
}
