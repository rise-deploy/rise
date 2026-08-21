use anyhow::{Context, Result};
use sqlx::PgPool;
use uuid::Uuid;

use crate::db::models::{Team, TeamMember, TeamRole, User};

/// List all teams
pub async fn list(pool: &PgPool) -> Result<Vec<Team>> {
    let teams = sqlx::query_as!(
        Team,
        r#"
        SELECT id, name, idp_managed, created_at, updated_at
        FROM teams
        ORDER BY created_at DESC
        "#
    )
    .fetch_all(pool)
    .await
    .context("Failed to list teams")?;

    Ok(teams)
}

/// List teams for a specific user
pub async fn list_for_user(pool: &PgPool, user_id: Uuid) -> Result<Vec<Team>> {
    let teams = sqlx::query_as!(
        Team,
        r#"
        SELECT t.id, t.name, t.idp_managed, t.created_at, t.updated_at
        FROM teams t
        INNER JOIN team_members tm ON t.id = tm.team_id
        WHERE tm.user_id = $1
        ORDER BY t.created_at DESC
        "#,
        user_id
    )
    .fetch_all(pool)
    .await
    .context("Failed to list teams for user")?;

    Ok(teams)
}

/// Find team by name (case-insensitive due to unique index)
pub async fn find_by_name<'a, E>(executor: E, name: &str) -> Result<Option<Team>>
where
    E: sqlx::Executor<'a, Database = sqlx::Postgres>,
{
    let team = sqlx::query_as!(
        Team,
        r#"
        SELECT id, name, idp_managed, created_at, updated_at
        FROM teams
        WHERE LOWER(name) = LOWER($1)
        "#,
        name
    )
    .fetch_optional(executor)
    .await
    .context("Failed to find team by name")?;

    Ok(team)
}

/// Find a team by name and hold its row until the caller's transaction ends.
///
/// The IdP takeover in `sync_user_groups` / the Entra sync reads a team, purges
/// its memberships, and sets `idp_managed` — three statements. The membership
/// API reads `idp_managed`, decides whether the caller may write, and inserts —
/// also three. At `READ COMMITTED` those interleave: the writer's guard read can
/// see `idp_managed = false` while the takeover is mid-transaction, and its
/// insert lands on a row the takeover has already deleted. Postgres takes no gap
/// lock, so nothing stops it, and the membership survives the commit — which on
/// an IdP-managed team named after `auth.operator_idp_groups` is operator
/// standing.
///
/// Both sides take this lock, so whichever arrives second blocks and then reads
/// the other's committed state.
pub async fn find_by_name_for_update<'a, E>(executor: E, name: &str) -> Result<Option<Team>>
where
    E: sqlx::Executor<'a, Database = sqlx::Postgres>,
{
    let team = sqlx::query_as!(
        Team,
        r#"SELECT id, name, idp_managed, created_at, updated_at FROM teams WHERE LOWER(name) = LOWER($1) FOR UPDATE"#,
        name
    )
    .fetch_optional(executor)
    .await
    .context("Failed to lock team by name")?;

    Ok(team)
}

/// Find a team by id and hold its row until the caller's transaction ends. See
/// [`find_by_name_for_update`].
pub async fn find_by_id_for_update<'a, E>(executor: E, id: Uuid) -> Result<Option<Team>>
where
    E: sqlx::Executor<'a, Database = sqlx::Postgres>,
{
    let team = sqlx::query_as!(
        Team,
        r#"SELECT id, name, idp_managed, created_at, updated_at FROM teams WHERE id = $1 FOR UPDATE"#,
        id
    )
    .fetch_optional(executor)
    .await
    .context("Failed to lock team by id")?;

    Ok(team)
}

/// Find team by ID
pub async fn find_by_id(pool: &PgPool, id: Uuid) -> Result<Option<Team>> {
    let team = sqlx::query_as!(
        Team,
        r#"
        SELECT id, name, idp_managed, created_at, updated_at
        FROM teams
        WHERE id = $1
        "#,
        id
    )
    .fetch_optional(pool)
    .await
    .context("Failed to find team by ID")?;

    Ok(team)
}

/// Create a new team
pub async fn create<'a, E>(executor: E, name: &str) -> Result<Team>
where
    E: sqlx::Executor<'a, Database = sqlx::Postgres>,
{
    let team = sqlx::query_as!(
        Team,
        r#"
        INSERT INTO teams (name)
        VALUES ($1)
        RETURNING id, name, idp_managed, created_at, updated_at
        "#,
        name
    )
    .fetch_one(executor)
    .await
    .context("Failed to create team")?;

    Ok(team)
}

/// Delete team by ID
pub async fn delete<'a, E>(executor: E, id: Uuid) -> Result<()>
where
    E: sqlx::Executor<'a, Database = sqlx::Postgres>,
{
    sqlx::query!("DELETE FROM teams WHERE id = $1", id)
        .execute(executor)
        .await
        .context("Failed to delete team")?;

    Ok(())
}

/// Get team members (users with member role only, not owners)
pub async fn get_members<'a, E>(executor: E, team_id: Uuid) -> Result<Vec<User>>
where
    E: sqlx::Executor<'a, Database = sqlx::Postgres>,
{
    let members = sqlx::query_as!(
        User,
        r#"
        SELECT u.id, u.email, u.created_at, u.updated_at
        FROM users u
        INNER JOIN team_members tm ON u.id = tm.user_id
        WHERE tm.team_id = $1 AND tm.role = 'member'
        ORDER BY u.email
        "#,
        team_id
    )
    .fetch_all(executor)
    .await
    .context("Failed to get team members")?;

    Ok(members)
}

/// Get team owners
pub async fn get_owners<'a, E>(executor: E, team_id: Uuid) -> Result<Vec<User>>
where
    E: sqlx::Executor<'a, Database = sqlx::Postgres>,
{
    let owners = sqlx::query_as!(
        User,
        r#"
        SELECT u.id, u.email, u.created_at, u.updated_at
        FROM users u
        INNER JOIN team_members tm ON u.id = tm.user_id
        WHERE tm.team_id = $1 AND tm.role = 'owner'
        ORDER BY u.email
        "#,
        team_id
    )
    .fetch_all(executor)
    .await
    .context("Failed to get team owners")?;

    Ok(owners)
}

/// Add member to team
pub async fn add_member<'a, E>(
    executor: E,
    team_id: Uuid,
    user_id: Uuid,
    role: TeamRole,
) -> Result<TeamMember>
where
    E: sqlx::Executor<'a, Database = sqlx::Postgres>,
{
    let role_str = role.to_string();

    let member = sqlx::query_as!(
        TeamMember,
        r#"
        INSERT INTO team_members (team_id, user_id, role)
        VALUES ($1, $2, $3)
        RETURNING team_id, user_id, role as "role: TeamRole", created_at
        "#,
        team_id,
        user_id,
        role_str
    )
    .fetch_one(executor)
    .await
    .context("Failed to add team member")?;

    Ok(member)
}

/// Remove member from team (specific role only)
pub async fn remove_member<'a, E>(
    executor: E,
    team_id: Uuid,
    user_id: Uuid,
    role: TeamRole,
) -> Result<()>
where
    E: sqlx::Executor<'a, Database = sqlx::Postgres>,
{
    let role_str = role.to_string();

    sqlx::query!(
        "DELETE FROM team_members WHERE team_id = $1 AND user_id = $2 AND role = $3",
        team_id,
        user_id,
        role_str
    )
    .execute(executor)
    .await
    .context("Failed to remove team member")?;

    Ok(())
}

/// Remove user from team (all roles)
pub async fn remove_all_user_roles<'a, E>(executor: E, team_id: Uuid, user_id: Uuid) -> Result<()>
where
    E: sqlx::Executor<'a, Database = sqlx::Postgres>,
{
    sqlx::query!(
        "DELETE FROM team_members WHERE team_id = $1 AND user_id = $2",
        team_id,
        user_id
    )
    .execute(executor)
    .await
    .context("Failed to remove user from team")?;

    Ok(())
}

/// Update member role
pub async fn update_member_role<'a, E>(
    executor: E,
    team_id: Uuid,
    user_id: Uuid,
    role: TeamRole,
) -> Result<TeamMember>
where
    E: sqlx::Executor<'a, Database = sqlx::Postgres>,
{
    let role_str = role.to_string();

    let member = sqlx::query_as!(
        TeamMember,
        r#"
        UPDATE team_members
        SET role = $3
        WHERE team_id = $1 AND user_id = $2 AND role = 'member'
        RETURNING team_id, user_id, role as "role: TeamRole", created_at
        "#,
        team_id,
        user_id,
        role_str
    )
    .fetch_one(executor)
    .await
    .context("Failed to update member role")?;

    Ok(member)
}

/// Check if user is team owner
pub async fn is_owner(pool: &PgPool, team_id: Uuid, user_id: Uuid) -> Result<bool> {
    let result = sqlx::query!(
        r#"
        SELECT EXISTS(
            SELECT 1 FROM team_members
            WHERE team_id = $1 AND user_id = $2 AND role = 'owner'
        ) as "exists!"
        "#,
        team_id,
        user_id
    )
    .fetch_one(pool)
    .await
    .context("Failed to check team ownership")?;

    Ok(result.exists)
}

/// Check if user is team member (owner or member)
pub async fn is_member<'a, E>(executor: E, team_id: Uuid, user_id: Uuid) -> Result<bool>
where
    E: sqlx::Executor<'a, Database = sqlx::Postgres>,
{
    let result = sqlx::query!(
        r#"
        SELECT EXISTS(
            SELECT 1 FROM team_members
            WHERE team_id = $1 AND user_id = $2
        ) as "exists!"
        "#,
        team_id,
        user_id
    )
    .fetch_one(executor)
    .await
    .context("Failed to check team membership")?;

    Ok(result.exists)
}

/// Batch fetch team names by IDs
pub async fn get_names_batch(
    pool: &PgPool,
    team_ids: &[Uuid],
) -> Result<std::collections::HashMap<Uuid, String>> {
    let records = sqlx::query!(
        r#"
        SELECT id, name
        FROM teams
        WHERE id = ANY($1)
        "#,
        team_ids
    )
    .fetch_all(pool)
    .await
    .context("Failed to batch fetch team names")?;

    Ok(records.into_iter().map(|r| (r.id, r.name)).collect())
}

/// Batch fetch full team details by IDs
pub async fn get_teams_batch(
    pool: &PgPool,
    team_ids: &[Uuid],
) -> Result<std::collections::HashMap<Uuid, Team>> {
    let teams = sqlx::query_as!(
        Team,
        r#"
        SELECT id, name, idp_managed, created_at, updated_at
        FROM teams
        WHERE id = ANY($1)
        "#,
        team_ids
    )
    .fetch_all(pool)
    .await
    .context("Failed to batch fetch teams")?;

    Ok(teams.into_iter().map(|t| (t.id, t)).collect())
}

// ============================================================================
// IdP Group Sync Functions
// ============================================================================

/// Update team name (for case correction from IdP)
pub async fn update_name<'a, E>(executor: E, team_id: Uuid, name: &str) -> Result<Team>
where
    E: sqlx::Executor<'a, Database = sqlx::Postgres>,
{
    let team = sqlx::query_as!(
        Team,
        r#"
        UPDATE teams
        SET name = $2, updated_at = NOW()
        WHERE id = $1
        RETURNING id, name, idp_managed, created_at, updated_at
        "#,
        team_id,
        name
    )
    .fetch_one(executor)
    .await
    .context("Failed to update team name")?;

    Ok(team)
}

/// Mark team as IdP-managed
pub async fn set_idp_managed<'a, E>(executor: E, team_id: Uuid, idp_managed: bool) -> Result<()>
where
    E: sqlx::Executor<'a, Database = sqlx::Postgres>,
{
    sqlx::query!(
        r#"
        UPDATE teams
        SET idp_managed = $2, updated_at = NOW()
        WHERE id = $1
        "#,
        team_id,
        idp_managed
    )
    .execute(executor)
    .await
    .context("Failed to set idp_managed flag")?;

    Ok(())
}

/// Get all IdP-managed teams
pub async fn list_idp_managed<'a, E>(executor: E) -> Result<Vec<Team>>
where
    E: sqlx::Executor<'a, Database = sqlx::Postgres>,
{
    let teams = sqlx::query_as!(
        Team,
        r#"
        SELECT id, name, idp_managed, created_at, updated_at
        FROM teams
        WHERE idp_managed = TRUE
        ORDER BY name
        "#
    )
    .fetch_all(executor)
    .await
    .context("Failed to list IdP-managed teams")?;

    Ok(teams)
}

/// Get team names for all teams a user is a member of (for JWT groups claim)
pub async fn get_team_names_for_user(pool: &PgPool, user_id: Uuid) -> Result<Vec<String>> {
    let records = sqlx::query!(
        r#"
        SELECT t.name
        FROM teams t
        INNER JOIN team_members tm ON t.id = tm.team_id
        WHERE tm.user_id = $1
        ORDER BY t.name
        "#,
        user_id
    )
    .fetch_all(pool)
    .await
    .context("Failed to get team names for user")?;

    Ok(records.into_iter().map(|r| r.name).collect())
}

/// Get the names of the IdP-managed teams a user is a member of.
///
/// These are the user's IdP groups as Rise knows them: `sync_user_groups`
/// (login) and the Entra active sync both mirror the IdP's groups into
/// `idp_managed` teams. Teams users create themselves are excluded, so a
/// self-service team named after a privileged group cannot grant that group's
/// permissions.
pub async fn list_idp_group_names_for_user<'e, E>(executor: E, user_id: Uuid) -> Result<Vec<String>>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let records = sqlx::query!(
        r#"
        SELECT DISTINCT t.name
        FROM teams t
        INNER JOIN team_members tm ON t.id = tm.team_id
        WHERE tm.user_id = $1 AND t.idp_managed = TRUE
        ORDER BY t.name
        "#,
        user_id
    )
    .fetch_all(executor)
    .await
    .context("Failed to get IdP group names for user")?;

    Ok(records.into_iter().map(|r| r.name).collect())
}

/// Remove every membership from a team, whatever the role, and report how many
/// went.
///
/// Also used when the IdP takes over a team that already existed. Removing only
/// the owners is not enough there: an IdP-managed team's membership is *defined*
/// by the IdP claim, and `list_idp_group_names_for_user` reads that membership
/// to grant operator, admin, and platform access by group. A member row that
/// predates the takeover is a group membership nobody in the IdP asserted — and
/// since anyone may create a team under any unused name while
/// `allow_team_creation` is on, a surviving row is a self-asserted claim to
/// whatever that group name confers.
///
/// Genuine members are re-added by the sync on their next login, so the purge is
/// self-healing in the direction that matters: it can cost a real member their
/// group-derived role until they sign in again, and it cannot give anyone one
/// they were not granted.
pub async fn remove_all_team_members<'a, E>(executor: E, team_id: Uuid) -> Result<u64>
where
    E: sqlx::Executor<'a, Database = sqlx::Postgres>,
{
    let result = sqlx::query!("DELETE FROM team_members WHERE team_id = $1", team_id)
        .execute(executor)
        .await
        .context("Failed to remove all team members")?;

    Ok(result.rows_affected())
}

/// Get all member user IDs for a team (any role)
pub async fn get_all_member_user_ids<'a, E>(executor: E, team_id: Uuid) -> Result<Vec<Uuid>>
where
    E: sqlx::Executor<'a, Database = sqlx::Postgres>,
{
    let records = sqlx::query!(
        r#"
        SELECT DISTINCT user_id
        FROM team_members
        WHERE team_id = $1
        "#,
        team_id
    )
    .fetch_all(executor)
    .await
    .context("Failed to get team member user IDs")?;

    Ok(records.into_iter().map(|r| r.user_id).collect())
}
