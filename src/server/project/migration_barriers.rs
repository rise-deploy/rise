//! Migration compatibility barriers (ADR-0007): the project's append-only
//! ledger of migration versions that older code must not be rolled back past.
//!
//! Project owners and members record barriers, not only admins: a barrier can
//! only restrict which deployments may be created, so recording one grants
//! nothing.

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

/// Longest accepted barrier reason, in characters; mirrored by the
/// `migration_barriers.reason` CHECK constraint.
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
    /// Check the request against the ledger's rules; returns the trimmed reason.
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
                "Migration barrier reason is too long ({} characters, at most {})",
                chars, MAX_REASON_CHARS
            )));
        }
        Ok(reason)
    }
}

async fn resolve_project(
    state: &AppState,
    id_or_name: &str,
) -> Result<crate::db::models::Project, ServerError> {
    if let Ok(uuid) = id_or_name.parse() {
        projects::find_by_id(&state.db_pool, uuid)
            .await
            .internal_err("Failed to get project")?
    } else {
        projects::find_by_name(&state.db_pool, id_or_name)
            .await
            .internal_err("Failed to get project")?
    }
    .ok_or_else(|| ServerError::not_found("Project not found"))
}

/// List the project's migration barriers by ascending version.
pub async fn list_migration_barriers(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id_or_name): Path<String>,
) -> Result<Json<Vec<MigrationBarrierResponse>>, ServerError> {
    let project = resolve_project(&state, &id_or_name).await?;
    let user = auth.user()?;
    ensure_project_access_or_admin(&state, user, &project).await?;

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
    let project = resolve_project(&state, &id_or_name).await?;
    let user = auth.user()?;
    ensure_project_access_or_admin(&state, user, &project).await?;

    let reason = payload.validate()?;

    let barrier = match db_barriers::create(
        &state.db_pool,
        project.id,
        payload.version,
        reason,
        Some(user.id),
    )
    .await
    {
        Ok(barrier) => barrier,
        Err(e) if db_barriers::is_duplicate(&e) => {
            return Err(ServerError::conflict(format!(
                "Project '{}' already has a migration barrier at version {}",
                project.name, payload.version
            ))
            .with_error_code("migration_barrier_exists"));
        }
        Err(e) => {
            return Err(ServerError::internal_anyhow(
                e,
                "Failed to record migration barrier",
            ))
        }
    };

    tracing::info!(
        project = %project.name,
        version = barrier.version,
        "Recorded migration barrier"
    );

    Ok((StatusCode::CREATED, Json(barrier.into())))
}

#[cfg(test)]
mod tests {
    use super::{CreateMigrationBarrierRequest, MAX_REASON_CHARS};

    fn request(version: i64, reason: &str) -> CreateMigrationBarrierRequest {
        CreateMigrationBarrierRequest {
            version,
            reason: reason.to_string(),
        }
    }

    #[test]
    fn version_must_be_positive() {
        assert!(request(0, "drops users.legacy_name").validate().is_err());
        assert!(request(-3, "drops users.legacy_name").validate().is_err());
        assert!(request(1, "drops users.legacy_name").validate().is_ok());
    }

    #[test]
    fn reason_must_be_present_and_bounded() {
        assert!(request(5, "").validate().is_err());
        assert!(request(5, "   ").validate().is_err());
        assert_eq!(
            request(5, "  renames orders  ").validate().unwrap(),
            "renames orders"
        );

        // The bound counts characters, not bytes.
        let at_limit = "ä".repeat(MAX_REASON_CHARS);
        assert!(request(5, &at_limit).validate().is_ok());
        let over = "a".repeat(MAX_REASON_CHARS + 1);
        assert!(request(5, &over).validate().is_err());
    }
}
