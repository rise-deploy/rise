//! Migration compatibility barriers (ADR-0007).
//!
//! A barrier records that the migrations shipped with `version` break older
//! code: no deployment with a lower `migration_version` may replace one at or
//! above it. The ledger is append-only (a trigger refuses updates).

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

/// One recorded barrier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MigrationBarrier {
    pub id: Uuid,
    pub project_id: Uuid,
    pub version: i64,
    pub reason: String,
    /// Email of whoever recorded it, while that user still exists.
    pub recorded_by_email: Option<String>,
    pub recorded_at: DateTime<Utc>,
}

/// Record a barrier. Fails with a unique violation when the project already
/// has one at `version` (see [`is_duplicate`]).
pub async fn create(
    pool: &PgPool,
    project_id: Uuid,
    version: i64,
    reason: &str,
    user_id: Option<Uuid>,
) -> Result<MigrationBarrier> {
    let barrier = sqlx::query_as!(
        MigrationBarrier,
        r#"
        WITH ins AS (
            INSERT INTO migration_barriers (project_id, version, reason, recorded_by_id)
            VALUES ($1, $2, $3, $4)
            RETURNING id, project_id, version, reason, recorded_by_id, recorded_at
        )
        SELECT
            ins.id, ins.project_id, ins.version, ins.reason,
            u.email AS "recorded_by_email?",
            ins.recorded_at
        FROM ins
        LEFT JOIN users u ON u.id = ins.recorded_by_id
        "#,
        project_id,
        version,
        reason,
        user_id
    )
    .fetch_one(pool)
    .await
    .context("Failed to record migration barrier")?;

    Ok(barrier)
}

/// Whether `err` is the unique violation of a second barrier at the same
/// version in one project.
pub fn is_duplicate(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<sqlx::Error>()
            .and_then(|e| e.as_database_error())
            .and_then(|db| db.constraint())
            == Some("migration_barriers_project_id_version_key")
    })
}

/// Every barrier of a project, by ascending version.
pub async fn list_for_project(pool: &PgPool, project_id: Uuid) -> Result<Vec<MigrationBarrier>> {
    list_crossed(pool, project_id, 0, i64::MAX).await
}

/// The barriers a move from `high_inclusive` down to `low_exclusive` crosses:
/// every version `v` with `low_exclusive < v <= high_inclusive`, ascending.
pub async fn list_crossed(
    pool: &PgPool,
    project_id: Uuid,
    low_exclusive: i64,
    high_inclusive: i64,
) -> Result<Vec<MigrationBarrier>> {
    let barriers = sqlx::query_as!(
        MigrationBarrier,
        r#"
        SELECT
            b.id, b.project_id, b.version, b.reason,
            u.email AS "recorded_by_email?",
            b.recorded_at
        FROM migration_barriers b
        LEFT JOIN users u ON u.id = b.recorded_by_id
        WHERE b.project_id = $1
          AND b.version > $2
          AND b.version <= $3
        ORDER BY b.version ASC
        "#,
        project_id,
        low_exclusive,
        high_inclusive
    )
    .fetch_all(pool)
    .await
    .context("Failed to list migration barriers")?;

    Ok(barriers)
}

/// Whether the project has recorded any barrier.
pub async fn project_has_any(pool: &PgPool, project_id: Uuid) -> Result<bool> {
    let exists = sqlx::query_scalar!(
        r#"
        SELECT EXISTS (SELECT 1 FROM migration_barriers WHERE project_id = $1) AS "exists!"
        "#,
        project_id
    )
    .fetch_one(pool)
    .await
    .context("Failed to check for migration barriers")?;

    Ok(exists)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{models::ProjectStatus, projects, users};

    async fn seed(pool: &PgPool) -> (Uuid, Uuid) {
        let user = users::create(pool, "barriers@example.com").await.unwrap();
        let project = projects::create(
            pool,
            "barriers-project",
            ProjectStatus::Stopped,
            "public".to_string(),
            Some(user.id),
            None,
            None,
        )
        .await
        .unwrap();
        (project.id, user.id)
    }

    #[sqlx::test]
    async fn create_and_list_in_version_order(pool: PgPool) {
        let (project_id, user_id) = seed(&pool).await;
        assert!(!project_has_any(&pool, project_id).await.unwrap());
        assert!(list_for_project(&pool, project_id)
            .await
            .unwrap()
            .is_empty());

        let five = create(&pool, project_id, 5, "drops column a", Some(user_id))
            .await
            .unwrap();
        assert_eq!(
            five.recorded_by_email.as_deref(),
            Some("barriers@example.com")
        );
        create(&pool, project_id, 2, "renames table b", None)
            .await
            .unwrap();

        let all = list_for_project(&pool, project_id).await.unwrap();
        assert_eq!(
            all.iter().map(|b| b.version).collect::<Vec<_>>(),
            vec![2, 5]
        );
        assert_eq!(all[0].recorded_by_email, None);
        assert!(project_has_any(&pool, project_id).await.unwrap());
    }

    /// Duplicates are detected, and the trigger refuses updates.
    #[sqlx::test]
    async fn duplicates_and_updates_are_refused(pool: PgPool) {
        let (project_id, user_id) = seed(&pool).await;
        let barrier = create(&pool, project_id, 3, "first", Some(user_id))
            .await
            .unwrap();
        let err = create(&pool, project_id, 3, "second", Some(user_id))
            .await
            .unwrap_err();
        assert!(is_duplicate(&err), "{err:?}");
        assert!(!is_duplicate(&anyhow::anyhow!("something else")));

        let err = sqlx::query("UPDATE migration_barriers SET reason = 'changed' WHERE id = $1")
            .bind(barrier.id)
            .execute(&pool)
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("append-only"), "{err}");
    }

    #[sqlx::test]
    async fn list_crossed_is_half_open(pool: PgPool) {
        let (project_id, _) = seed(&pool).await;
        for v in [2, 4, 6] {
            create(&pool, project_id, v, "barrier", None).await.unwrap();
        }
        // (low, high]: the lower bound is excluded, the upper included.
        for (low, high, expected) in [
            (2, 6, vec![4, 6]),
            (1, 4, vec![2, 4]),
            (4, 5, vec![]),
            (6, 6, vec![]),
        ] {
            let crossed = list_crossed(&pool, project_id, low, high).await.unwrap();
            assert_eq!(
                crossed.iter().map(|b| b.version).collect::<Vec<_>>(),
                expected,
                "({low}, {high}]"
            );
        }
    }
}
