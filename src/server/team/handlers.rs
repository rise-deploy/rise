use super::fuzzy::find_similar_teams;
use super::models::{
    CreateTeamRequest, CreateTeamResponse, GetTeamParams, Team as ApiTeam, UpdateTeamRequest,
    UpdateTeamResponse, UserInfo,
};
use crate::db::models::TeamRole;
use crate::db::{projects as db_projects, service_accounts, teams as db_teams};
use crate::server::auth::context::AuthContext;
use crate::server::error::{ServerError, ServerErrorExt};
use crate::server::state::AppState;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use uuid::Uuid;

pub async fn create_team(
    State(state): State<AppState>,
    auth: AuthContext,
    Json(payload): Json<CreateTeamRequest>,
) -> Result<Json<CreateTeamResponse>, ServerError> {
    let user = auth.user()?;
    // Check if user is allowed to create teams
    let is_admin = state.is_admin(user).await;
    if !state.auth_settings.allow_team_creation && !is_admin {
        tracing::warn!(
            "User {} attempted to create team '{}' but team creation is disabled for non-admins",
            user.email,
            payload.name
        );
        return Err(ServerError::forbidden(
            "Team creation is disabled. Please contact your administrator.",
        ));
    }

    tracing::info!("Creating team '{}' for user {}", payload.name, user.email);

    // Validate that at least one owner is specified
    if payload.owners.is_empty() {
        return Err(ServerError::bad_request(
            "At least one owner must be specified",
        ));
    }

    // Parse owner IDs
    let owner_ids: Vec<Uuid> = payload
        .owners
        .iter()
        .map(|id| Uuid::parse_str(id))
        .collect::<Result<Vec<_>, _>>()
        .server_err(StatusCode::BAD_REQUEST, "Invalid owner ID")?;

    // Verify the authenticated user is in the owners list
    if !owner_ids.contains(&user.id) {
        return Err(ServerError::bad_request(
            "You must be an owner of the team you create",
        ));
    }

    // Parse member IDs
    let member_ids: Vec<Uuid> = payload
        .members
        .iter()
        .map(|id| Uuid::parse_str(id))
        .collect::<Result<Vec<_>, _>>()
        .server_err(StatusCode::BAD_REQUEST, "Invalid member ID")?;

    // Create the team, stamp the default Organization linkage, and add
    // owners/members in a single transaction. The stamp keeps the
    // post-bootstrap invariant ("every team row carries an
    // organization_resource_uid") holding from this PR forward.
    let mut tx = state
        .db_pool
        .begin()
        .await
        .internal_err("Failed to start transaction for team create")?;

    let team = db_teams::create(&mut *tx, &payload.name)
        .await
        .map_err(|e| {
            if e.to_string().contains("duplicate key")
                || e.to_string().contains("unique constraint")
            {
                ServerError::conflict(format!("Team '{}' already exists", payload.name))
            } else {
                ServerError::internal_anyhow(e, "Failed to create team")
            }
        })?;

    crate::db::organization_links::set_team_organization(
        &mut tx,
        team.id,
        state.default_organization_uid,
    )
    .await
    .internal_err("Failed to stamp default Organization on new team")?;

    // Add owners
    for owner_id in owner_ids {
        db_teams::add_member(&mut *tx, team.id, owner_id, TeamRole::Owner)
            .await
            .internal_err("Failed to add owner")?;
    }

    // Add members.
    // A user can intentionally be both owner and member (dual roles are supported by schema).
    for member_id in member_ids {
        db_teams::add_member(&mut *tx, team.id, member_id, TeamRole::Member)
            .await
            .internal_err("Failed to add member")?;
    }

    tx.commit()
        .await
        .internal_err("Failed to commit team create transaction")?;

    // Fetch members/owners with email info for response
    let members = db_teams::get_members(&state.db_pool, team.id)
        .await
        .internal_err("Failed to get team members")?;
    let owners = db_teams::get_owners(&state.db_pool, team.id)
        .await
        .internal_err("Failed to get team owners")?;

    let member_infos = users_to_infos(&members);
    let owner_infos = users_to_infos(&owners);

    Ok(Json(CreateTeamResponse {
        team: convert_team(team, member_infos, owner_infos),
    }))
}

pub async fn get_team(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id_or_name): Path<String>,
    Query(params): Query<GetTeamParams>,
) -> Result<Json<serde_json::Value>, ServerError> {
    let _user = auth.user()?;
    // Resolve team by ID or name
    let team = resolve_team(&state, &id_or_name, params.by_id).await?;

    // Always resolve member/owner emails
    let members = db_teams::get_members(&state.db_pool, team.id)
        .await
        .internal_err("Failed to get team members")?;

    let owners = db_teams::get_owners(&state.db_pool, team.id)
        .await
        .internal_err("Failed to get team owners")?;

    let member_infos = users_to_infos(&members);
    let owner_infos = users_to_infos(&owners);

    Ok(Json(
        serde_json::to_value(convert_team(team, member_infos, owner_infos)).unwrap(),
    ))
}

pub async fn update_team(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id_or_name): Path<String>,
    Query(params): Query<GetTeamParams>,
    Json(payload): Json<UpdateTeamRequest>,
) -> Result<Json<UpdateTeamResponse>, ServerError> {
    let user = auth.user()?;
    // Resolve team by ID or name
    let team = resolve_team(&state, &id_or_name, params.by_id).await?;

    // Check if user is an admin or owner of the team
    let is_admin = state.is_admin(user).await;
    let is_owner = if !is_admin {
        db_teams::is_owner(&state.db_pool, team.id, user.id)
            .await
            .internal_err("Failed to check team ownership")?
    } else {
        true // Admins bypass ownership check
    };

    if !is_owner {
        return Err(ServerError::forbidden(
            "You must be an owner of the team to update it",
        ));
    }

    // From here the team row is locked and every membership read *and* write
    // runs on this one transaction.
    //
    // The reads matter as much as the writes, for a different reason: the pool
    // is capped, so a handler that holds a transaction's connection and then
    // asks the pool for a second one deadlocks against itself once enough
    // requests do it at once — and with an `is_service_account` call per member
    // in the payload, "enough" is a handful of ordinary team edits. Every read
    // below therefore goes through `tx`.
    //
    // The `idp_managed` guard below and the writes it guards used to be separate
    // autocommit statements, which let an IdP takeover interleave: the guard
    // read `false` while `sync_user_groups` was mid-transaction, and the insert
    // landed on a row the takeover had already purged. Postgres takes no gap
    // lock, so the membership survived — and on a team named after
    // `auth.operator_idp_groups`, a membership is operator standing. Both sides
    // now lock the row, so whichever arrives second reads the other's committed
    // state.
    let mut tx = state
        .db_pool
        .begin()
        .await
        .internal_err("Failed to start transaction")?;
    let team = db_teams::find_by_id_for_update(&mut *tx, team.id)
        .await
        .internal_err("Failed to lock team")?
        .ok_or_else(|| ServerError::not_found("Team not found"))?;
    // Check if team is IdP-managed (only admins can modify)
    if team.idp_managed && !is_admin {
        return Err(ServerError::forbidden(
            "This team is managed by your Identity Provider. Only administrators can modify IdP-managed teams.",
        ));
    }

    // Update name if provided
    let updated_team = if let Some(_name) = payload.name {
        // For now, we don't have an update_name function, we'll need to add it
        // or handle it differently. Let's skip name updates for now.
        team.clone()
    } else {
        team.clone()
    };

    // Update members if provided
    if let Some(new_members) = payload.members {
        let member_ids: Vec<Uuid> = new_members
            .iter()
            .map(|id| Uuid::parse_str(id))
            .collect::<Result<Vec<_>, _>>()
            .server_err(StatusCode::BAD_REQUEST, "Invalid member ID")?;

        // Get current members
        let current_members = db_teams::get_members(&mut *tx, team.id)
            .await
            .internal_err("Failed to get current members")?;

        let current_member_ids: Vec<Uuid> = current_members.iter().map(|m| m.id).collect();

        // Remove members that are no longer in the list
        for current_member_id in &current_member_ids {
            if !member_ids.contains(current_member_id) {
                db_teams::remove_member(&mut *tx, team.id, *current_member_id, TeamRole::Member)
                    .await
                    .internal_err("Failed to remove member")?;
            }
        }

        // Validate that none of the new members are service accounts
        for member_id in &member_ids {
            let is_sa = service_accounts::is_service_account(&mut *tx, *member_id)
                .await
                .internal_err("Failed to check service account status")?;

            if is_sa {
                return Err(ServerError::bad_request(
                    "Service accounts cannot be team members",
                ));
            }
        }

        // Add new members
        for member_id in member_ids {
            if !current_member_ids.contains(&member_id) {
                db_teams::add_member(&mut *tx, team.id, member_id, TeamRole::Member)
                    .await
                    .internal_err("Failed to add member")?;
            }
        }
    }

    // Update owners if provided
    if let Some(new_owners) = payload.owners {
        let owner_ids: Vec<Uuid> = new_owners
            .iter()
            .map(|id| Uuid::parse_str(id))
            .collect::<Result<Vec<_>, _>>()
            .server_err(StatusCode::BAD_REQUEST, "Invalid owner ID")?;

        // Get current owners
        let current_owners = db_teams::get_owners(&mut *tx, team.id)
            .await
            .internal_err("Failed to get current owners")?;

        let current_owner_ids: Vec<Uuid> = current_owners.iter().map(|o| o.id).collect();

        // Remove owners that are no longer in the list
        for current_owner_id in &current_owner_ids {
            if !owner_ids.contains(current_owner_id) {
                db_teams::remove_member(&mut *tx, team.id, *current_owner_id, TeamRole::Owner)
                    .await
                    .internal_err("Failed to remove owner")?;
            }
        }

        // Validate that none of the new owners are service accounts
        for owner_id in &owner_ids {
            let is_sa = service_accounts::is_service_account(&mut *tx, *owner_id)
                .await
                .internal_err("Failed to check service account status")?;

            if is_sa {
                return Err(ServerError::bad_request(
                    "Service accounts cannot be team members",
                ));
            }
        }

        // Add new owners
        for owner_id in owner_ids {
            if !current_owner_ids.contains(&owner_id) {
                db_teams::add_member(&mut *tx, team.id, owner_id, TeamRole::Owner)
                    .await
                    .internal_err("Failed to add owner")?;
            } else {
                // Update role if already a member but not an owner
                db_teams::update_member_role(&mut *tx, team.id, owner_id, TeamRole::Owner)
                    .await
                    .internal_err("Failed to update member role")?;
            }
        }
    }

    // Fetch updated members and owners
    tx.commit()
        .await
        .internal_err("Failed to commit team update")?;

    let members = db_teams::get_members(&state.db_pool, updated_team.id)
        .await
        .internal_err("Failed to get team members")?;

    let owners = db_teams::get_owners(&state.db_pool, updated_team.id)
        .await
        .internal_err("Failed to get team owners")?;

    let member_infos = users_to_infos(&members);
    let owner_infos = users_to_infos(&owners);

    Ok(Json(UpdateTeamResponse {
        team: convert_team(updated_team, member_infos, owner_infos),
    }))
}

pub async fn delete_team(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id_or_name): Path<String>,
    Query(params): Query<GetTeamParams>,
) -> Result<StatusCode, ServerError> {
    let user = auth.user()?;
    // Resolve team by ID or name
    let team = resolve_team(&state, &id_or_name, params.by_id).await?;

    // Check if user is an admin or owner of the team
    let is_admin = state.is_admin(user).await;
    let is_owner = if !is_admin {
        db_teams::is_owner(&state.db_pool, team.id, user.id)
            .await
            .internal_err("Failed to check team ownership")?
    } else {
        true // Admins bypass ownership check
    };

    if !is_owner {
        return Err(ServerError::forbidden(
            "You must be an owner of the team to delete it",
        ));
    }

    // Locked and re-read, for the same reason `update_team` is: the guard below
    // reads `idp_managed`, and an IdP takeover is three statements long. On the
    // unlocked read this guard passed against the pre-takeover row while the
    // takeover was mid-transaction, and the DELETE then blocked on the row lock
    // and re-evaluated `id = $1` against the *updated* tuple — deleting the
    // now-IdP-managed team and cascading its members. No standing was gained,
    // but every genuine member of that group lost their group-derived admin or
    // operator role until the next sync. Both handlers behind this guard have
    // to take the lock; fixing only one of them fixed only one of them.
    let mut tx = state
        .db_pool
        .begin()
        .await
        .internal_err("Failed to start transaction")?;
    let team = db_teams::find_by_id_for_update(&mut *tx, team.id)
        .await
        .internal_err("Failed to lock team")?
        .ok_or_else(|| ServerError::not_found("Team not found"))?;

    // Check if team is IdP-managed (only admins can delete)
    if team.idp_managed && !is_admin {
        return Err(ServerError::forbidden(
            "This team is managed by your Identity Provider. Only administrators can delete IdP-managed teams.",
        ));
    }

    // Check if team owns any projects
    let owned_project_count = db_projects::count_owned_by_team(&mut *tx, team.id)
        .await
        .internal_err("Failed to check projects owned by team")?;
    if owned_project_count > 0 {
        return Err(ServerError::conflict(format!(
            "Team '{}' owns {} project(s). Reassign or delete those projects before deleting the team.",
            team.name, owned_project_count
        )));
    }

    db_teams::delete(&mut *tx, team.id)
        .await
        .internal_err("Failed to delete team")?;

    tx.commit()
        .await
        .internal_err("Failed to commit team deletion")?;

    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_teams(
    State(state): State<AppState>,
    auth: AuthContext,
) -> Result<Json<Vec<ApiTeam>>, ServerError> {
    let user = auth.user()?;
    // Admins can see all teams; other users see all teams if allow_list_all_teams is enabled
    let teams = if state.is_admin(user).await || state.auth_settings.allow_list_all_teams {
        db_teams::list(&state.db_pool)
            .await
            .internal_err("Failed to list teams")?
    } else {
        db_teams::list_for_user(&state.db_pool, user.id)
            .await
            .internal_err("Failed to list teams")?
    };

    let mut api_teams = Vec::new();

    for team in teams {
        let members = db_teams::get_members(&state.db_pool, team.id)
            .await
            .internal_err("Failed to get team members")?;

        let owners = db_teams::get_owners(&state.db_pool, team.id)
            .await
            .internal_err("Failed to get team owners")?;

        let member_infos = users_to_infos(&members);
        let owner_infos = users_to_infos(&owners);

        api_teams.push(convert_team(team, member_infos, owner_infos));
    }

    Ok(Json(api_teams))
}

/// Query team by ID
async fn query_team_by_id(
    state: &AppState,
    team_id: &str,
) -> Result<crate::db::models::Team, String> {
    let uuid = Uuid::parse_str(team_id).map_err(|e| format!("Invalid team ID: {}", e))?;

    db_teams::find_by_id(&state.db_pool, uuid)
        .await
        .map_err(|e| format!("Team not found: {}", e))?
        .ok_or_else(|| "Team not found".to_string())
}

/// Query team by name
async fn query_team_by_name(
    state: &AppState,
    team_name: &str,
) -> Result<crate::db::models::Team, String> {
    tracing::info!("Querying team by name: {}", team_name);

    db_teams::find_by_name(&state.db_pool, team_name)
        .await
        .map_err(|e| format!("Failed to query team by name: {}", e))?
        .ok_or_else(|| format!("Team '{}' not found", team_name))
}

/// Resolve team by ID or name with fuzzy matching support
async fn resolve_team(
    state: &AppState,
    id_or_name: &str,
    by_id: bool,
) -> Result<crate::db::models::Team, ServerError> {
    tracing::info!("Resolving team '{}', by_id={}", id_or_name, by_id);

    let team = if by_id {
        // Explicit ID lookup
        tracing::info!("Using explicit ID lookup");
        query_team_by_id(state, id_or_name)
            .await
            .map_err(ServerError::not_found)?
    } else {
        // Try name first, fallback to ID
        tracing::info!("Trying name lookup first, will fallback to ID");
        match query_team_by_name(state, id_or_name).await {
            Ok(team) => team,
            Err(e) => {
                tracing::info!("Name lookup failed: {}, trying ID fallback", e);
                query_team_by_id(state, id_or_name).await.map_err(|_e| {
                    tracing::info!("Both lookups failed, generating fuzzy suggestions");
                    // Both failed - provide fuzzy suggestions
                    let all_teams = tokio::task::block_in_place(|| {
                        tokio::runtime::Handle::current().block_on(db_teams::list(&state.db_pool))
                    });

                    let suggestions = match all_teams {
                        Ok(all_teams) => {
                            let api_teams: Vec<ApiTeam> = all_teams
                                .into_iter()
                                .map(|t| convert_team(t, vec![], vec![]))
                                .collect();
                            let similar = find_similar_teams(id_or_name, &api_teams, 0.85);
                            if similar.is_empty() {
                                None
                            } else {
                                Some(similar)
                            }
                        }
                        Err(_) => None,
                    };

                    ServerError::not_found(format!("Team '{}' not found", id_or_name))
                        .with_suggestions(suggestions)
                })?
            }
        }
    };

    Ok(team)
}

/// Convert a list of db users to UserInfo structs
fn users_to_infos(users: &[crate::db::models::User]) -> Vec<UserInfo> {
    users
        .iter()
        .map(|u| UserInfo {
            id: u.id.to_string(),
            email: u.email.clone(),
        })
        .collect()
}

/// Convert database Team model to API Team model
fn convert_team(
    team: crate::db::models::Team,
    members: Vec<UserInfo>,
    owners: Vec<UserInfo>,
) -> ApiTeam {
    ApiTeam {
        id: team.id.to_string(),
        name: team.name,
        members,
        owners,
        idp_managed: team.idp_managed,
        created: team.created_at.to_rfc3339(),
        updated: team.updated_at.to_rfc3339(),
    }
}
