//! Database helpers for organization linkage on typed tables.
//!
//! These functions exist to keep all SQLX queries inside `crate::db` per the
//! project conventions. They are used by the bootstrap pass (to backfill the
//! default-organization linkage on existing users/teams/projects) and by
//! typed APIs that need to stamp the default organization on newly created
//! rows.

use anyhow::{Context, Result};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

/// Insert a `(user_id, organization_resource_uid)` row when absent. Idempotent.
#[allow(dead_code)]
pub async fn ensure_user_membership(
    conn: &mut PgConnection,
    user_id: Uuid,
    organization_resource_uid: Uuid,
) -> Result<()> {
    sqlx::query!(
        r#"
        INSERT INTO user_organization_memberships (user_id, organization_resource_uid)
        VALUES ($1, $2)
        ON CONFLICT (user_id, organization_resource_uid) DO NOTHING
        "#,
        user_id,
        organization_resource_uid
    )
    .execute(&mut *conn)
    .await
    .context("Failed to ensure user organization membership")?;
    Ok(())
}

/// Check whether the given user is a member of the given organization.
#[allow(dead_code)]
pub async fn is_user_member(
    conn: &mut PgConnection,
    user_id: Uuid,
    organization_resource_uid: Uuid,
) -> Result<bool> {
    let row = sqlx::query!(
        r#"
        SELECT EXISTS(
            SELECT 1 FROM user_organization_memberships
            WHERE user_id = $1 AND organization_resource_uid = $2
        ) as "exists!"
        "#,
        user_id,
        organization_resource_uid
    )
    .fetch_one(&mut *conn)
    .await
    .context("Failed to look up user organization membership")?;
    Ok(row.exists)
}

/// Backfill `organization_resource_uid` on every team that does not have one.
/// Returns the number of rows updated.
pub async fn backfill_teams_organization(
    pool: &PgPool,
    organization_resource_uid: Uuid,
) -> Result<u64> {
    let result = sqlx::query!(
        r#"
        UPDATE teams
        SET organization_resource_uid = $1
        WHERE organization_resource_uid IS NULL
        "#,
        organization_resource_uid
    )
    .execute(pool)
    .await
    .context("Failed to backfill teams.organization_resource_uid")?;
    Ok(result.rows_affected())
}

/// Backfill `organization_resource_uid` on every project that does not have one.
/// Returns the number of rows updated.
pub async fn backfill_projects_organization(
    pool: &PgPool,
    organization_resource_uid: Uuid,
) -> Result<u64> {
    let result = sqlx::query!(
        r#"
        UPDATE projects
        SET organization_resource_uid = $1
        WHERE organization_resource_uid IS NULL
        "#,
        organization_resource_uid
    )
    .execute(pool)
    .await
    .context("Failed to backfill projects.organization_resource_uid")?;
    Ok(result.rows_affected())
}

/// Backfill missing memberships in the default Organization for every user.
/// Returns the number of rows inserted.
pub async fn backfill_user_organization_memberships(
    pool: &PgPool,
    organization_resource_uid: Uuid,
) -> Result<u64> {
    let result = sqlx::query!(
        r#"
        INSERT INTO user_organization_memberships (user_id, organization_resource_uid)
        SELECT u.id, $1
        FROM users u
        ON CONFLICT (user_id, organization_resource_uid) DO NOTHING
        "#,
        organization_resource_uid
    )
    .execute(pool)
    .await
    .context("Failed to backfill user_organization_memberships")?;
    Ok(result.rows_affected())
}

/// Count users that are NOT members of the given organization. Used by the
/// post-backfill validation pass; non-zero means startup fails.
pub async fn count_users_missing_membership(
    pool: &PgPool,
    organization_resource_uid: Uuid,
) -> Result<i64> {
    let row = sqlx::query!(
        r#"
        SELECT COUNT(*) AS "count!"
        FROM users u
        WHERE NOT EXISTS (
            SELECT 1 FROM user_organization_memberships m
            WHERE m.user_id = u.id
              AND m.organization_resource_uid = $1
        )
        "#,
        organization_resource_uid
    )
    .fetch_one(pool)
    .await
    .context("Failed to count users without default-organization membership")?;
    Ok(row.count)
}

/// Count teams missing an organization linkage. Used by post-backfill
/// validation.
pub async fn count_teams_missing_organization(pool: &PgPool) -> Result<i64> {
    let row = sqlx::query!(
        r#"SELECT COUNT(*) AS "count!" FROM teams WHERE organization_resource_uid IS NULL"#
    )
    .fetch_one(pool)
    .await
    .context("Failed to count teams missing organization linkage")?;
    Ok(row.count)
}

/// Count projects missing an organization linkage. Used by post-backfill
/// validation.
pub async fn count_projects_missing_organization(pool: &PgPool) -> Result<i64> {
    let row = sqlx::query!(
        r#"SELECT COUNT(*) AS "count!" FROM projects WHERE organization_resource_uid IS NULL"#
    )
    .fetch_one(pool)
    .await
    .context("Failed to count projects missing organization linkage")?;
    Ok(row.count)
}

/// Count teams and projects linked to the given Organization. Used by the
/// application-level guard that blocks deletion of an Organization with typed
/// children — these rows are not in the `resources` table, so the generic
/// child-detection check doesn't cover them.
#[allow(dead_code)]
pub async fn count_typed_children_for_organization(
    pool: &PgPool,
    organization_resource_uid: Uuid,
) -> Result<i64> {
    let row = sqlx::query!(
        r#"
        SELECT
            (SELECT COUNT(*) FROM teams WHERE organization_resource_uid = $1)
            + (SELECT COUNT(*) FROM projects WHERE organization_resource_uid = $1)
            AS "count!"
        "#,
        organization_resource_uid
    )
    .fetch_one(pool)
    .await
    .context("Failed to count typed children for organization")?;
    Ok(row.count)
}

/// Look up the organization linkage UID for a team by team_id. Returns `None`
/// when the team does not exist or when the column has not yet been
/// backfilled.
#[allow(dead_code)]
pub async fn organization_uid_for_team(
    conn: &mut PgConnection,
    team_id: Uuid,
) -> Result<Option<Uuid>> {
    let row = sqlx::query!(
        r#"SELECT organization_resource_uid FROM teams WHERE id = $1"#,
        team_id
    )
    .fetch_optional(&mut *conn)
    .await
    .context("Failed to look up team organization linkage")?;
    Ok(row.and_then(|r| r.organization_resource_uid))
}

/// Stamp `organization_resource_uid` on a team. Used when creating a new team
/// in the default-org bootstrap window.
#[allow(dead_code)]
pub async fn set_team_organization(
    conn: &mut PgConnection,
    team_id: Uuid,
    organization_resource_uid: Uuid,
) -> Result<()> {
    sqlx::query!(
        r#"
        UPDATE teams
        SET organization_resource_uid = $2,
            updated_at = NOW()
        WHERE id = $1
        "#,
        team_id,
        organization_resource_uid
    )
    .execute(&mut *conn)
    .await
    .context("Failed to set team organization")?;
    Ok(())
}

/// Stamp `organization_resource_uid` on a project. Used when creating a new
/// project in the default-org bootstrap window.
#[allow(dead_code)]
pub async fn set_project_organization(
    conn: &mut PgConnection,
    project_id: Uuid,
    organization_resource_uid: Uuid,
) -> Result<()> {
    sqlx::query!(
        r#"
        UPDATE projects
        SET organization_resource_uid = $2,
            updated_at = NOW()
        WHERE id = $1
        "#,
        project_id,
        organization_resource_uid
    )
    .execute(&mut *conn)
    .await
    .context("Failed to set project organization")?;
    Ok(())
}
