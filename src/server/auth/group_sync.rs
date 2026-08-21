use anyhow::{Context, Result};
use sqlx::PgPool;
use uuid::Uuid;

use crate::db::{models::TeamRole, teams};

/// Synchronize a user's team memberships from the configured IdP group claim.
///
/// This function implements the complete IdP group synchronization algorithm:
/// 1. Creates teams that don't exist in Rise
/// 2. Marks teams as IdP-managed
/// 3. Updates team names to match IdP case (IdP is source of truth)
/// 4. Removes every pre-existing membership when a team becomes IdP-managed
/// 5. Adds user to all groups in the IdP claim
/// 6. Removes user from IdP-managed teams NOT in the claim
///
/// All operations are performed within a database transaction for atomicity.
///
/// # Arguments
/// * `pool` - Database connection pool
/// * `user_id` - UUID of the user logging in
/// * `idp_groups` - List of group names from the configured IdP group claim
///
/// # Errors
/// Returns an error if database operations fail. The transaction will be rolled back.
pub async fn sync_user_groups(pool: &PgPool, user_id: Uuid, idp_groups: &[String]) -> Result<()> {
    // Start transaction for atomicity - all operations succeed or all fail
    let mut tx = pool.begin().await.context("Failed to start transaction")?;

    tracing::debug!(
        "Syncing {} IdP groups for user {}",
        idp_groups.len(),
        user_id
    );

    // Phase 1: Process each group in the IdP claim
    for group_name in idp_groups {
        // Locked for the rest of the transaction: the membership API decides
        // whether a caller may write to a team by reading `idp_managed`, and at
        // `READ COMMITTED` that read can land between this takeover's purge and
        // its commit. See `teams::find_by_name_for_update`.
        let existing_team = teams::find_by_name_for_update(&mut *tx, group_name)
            .await
            .context("Failed to find team by name")?;

        let team_id = if let Some(team) = existing_team {
            // Team exists - update if needed

            // Update name if case differs (IdP is authoritative source for casing)
            if team.name != *group_name {
                tracing::info!(
                    "Updating team name from '{}' to '{}' (IdP case correction)",
                    team.name,
                    group_name
                );
                teams::update_name(&mut *tx, team.id, group_name)
                    .await
                    .context("Failed to update team name")?;
            }

            // If team wasn't IdP-managed before, convert it.
            //
            // Every existing membership goes, not just the owners. An
            // IdP-managed team's membership is what grants operator, admin, and
            // platform access by group (`auth.operator_idp_groups` and friends,
            // via `list_idp_group_names_for_user`) — and any authenticated user
            // may create a team under any unused name while
            // `allow_team_creation` is on. So a member row that survives the
            // takeover is a claim to that group's authority that nobody in the
            // IdP ever asserted: create `rise-operators` before anyone in it
            // signs in, list yourself as a member, and the first genuine login
            // hands you operator standing. Real members are re-added on their
            // own next login.
            if !team.idp_managed {
                let dropped = teams::remove_all_team_members(&mut *tx, team.id)
                    .await
                    .context("Failed to clear pre-existing team members")?;
                if dropped > 0 {
                    tracing::warn!(
                        team = %group_name,
                        dropped_memberships = dropped,
                        "Converting a self-service team to IdP-managed; dropping pre-existing \
                     memberships the IdP never asserted"
                    );
                }
                teams::set_idp_managed(&mut *tx, team.id, true)
                    .await
                    .context("Failed to mark team as IdP-managed")?;
            }

            team.id
        } else {
            // Team doesn't exist - create it as IdP-managed
            tracing::info!("Creating new IdP-managed team '{}'", group_name);
            let new_team = teams::create(&mut *tx, group_name)
                .await
                .context("Failed to create team")?;

            // Mark as IdP-managed immediately
            teams::set_idp_managed(&mut *tx, new_team.id, true)
                .await
                .context("Failed to mark new team as IdP-managed")?;

            new_team.id
        };

        // Add user as member if not already in the team
        let is_member = teams::is_member(&mut *tx, team_id, user_id)
            .await
            .context("Failed to check membership")?;

        if !is_member {
            tracing::debug!("Adding user to IdP-managed team '{}'", group_name);
            teams::add_member(&mut *tx, team_id, user_id, TeamRole::Member)
                .await
                .context("Failed to add user to team")?;
        }
    }

    // Phase 2: Remove user from IdP-managed teams NOT in the claim
    let all_idp_teams = teams::list_idp_managed(&mut *tx)
        .await
        .context("Failed to list IdP-managed teams")?;

    for team in all_idp_teams {
        // Case-insensitive check if team is in the IdP group claim
        let in_claim = idp_groups
            .iter()
            .any(|g| g.eq_ignore_ascii_case(&team.name));

        if !in_claim {
            // User should not be in this IdP-managed team
            let is_member = teams::is_member(&mut *tx, team.id, user_id)
                .await
                .context("Failed to check membership")?;

            if is_member {
                tracing::info!(
                    "Removing user from IdP-managed team '{}' (not in IdP group claim)",
                    team.name
                );
                teams::remove_all_user_roles(&mut *tx, team.id, user_id)
                    .await
                    .context("Failed to remove user from team")?;
            }
        }
    }

    // Commit transaction - all changes are applied atomically
    tx.commit().await.context("Failed to commit transaction")?;

    tracing::debug!("Successfully synced IdP groups for user {}", user_id);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::users;

    /// Taking over a self-service team must not leave its members behind.
    ///
    /// Team names are first-come, first-served while `allow_team_creation` is
    /// on, and an IdP-managed team's membership is what grants operator, admin,
    /// and platform access by group. So a member row that survives the takeover
    /// is a claim to that group's authority asserted by whoever got to the name
    /// first — create the team named after `auth.operator_idp_groups` before
    /// anyone in that group signs in, list yourself in it, and the first genuine
    /// login makes you an operator.
    #[sqlx::test]
    async fn taking_over_a_self_service_team_drops_its_pre_existing_members(pool: PgPool) {
        let squatter = users::create(&pool, "squatter@example.com").await.unwrap();
        let genuine = users::create(&pool, "genuine@example.com").await.unwrap();

        // The squatter gets to the name first, as owner and as member.
        let team = teams::create(&pool, "rise-operators").await.unwrap();
        teams::add_member(&pool, team.id, squatter.id, TeamRole::Owner)
            .await
            .unwrap();
        teams::add_member(&pool, team.id, squatter.id, TeamRole::Member)
            .await
            .unwrap();
        assert_eq!(
            teams::list_idp_group_names_for_user(&pool, squatter.id)
                .await
                .unwrap(),
            Vec::<String>::new(),
            "a self-service team is not an IdP group yet"
        );

        // A real member of the IdP group logs in, and the team is taken over.
        sync_user_groups(&pool, genuine.id, &["rise-operators".to_string()])
            .await
            .unwrap();

        assert_eq!(
            teams::list_idp_group_names_for_user(&pool, genuine.id)
                .await
                .unwrap(),
            vec!["rise-operators".to_string()],
            "the IdP's own member holds the group"
        );
        assert_eq!(
            teams::list_idp_group_names_for_user(&pool, squatter.id)
                .await
                .unwrap(),
            Vec::<String>::new(),
            "the squatter must not inherit the group by having claimed the name"
        );
    }

    /// The purge alone is not enough: the membership API decides whether a
    /// caller may write by reading `idp_managed`, and at `READ COMMITTED` that
    /// read can land between the takeover's purge and its commit. Postgres takes
    /// no gap lock, so an insert would survive a delete it never saw.
    ///
    /// It calls `lock_and_guard_team` — the function the handlers themselves
    /// call — rather than reproducing its steps, so reverting the lock inside
    /// that function fails this test. An earlier version hand-rolled the
    /// sequence and stayed green when the handler changed, which is the whole
    /// failure mode being guarded against here.
    #[sqlx::test]
    async fn a_takeover_blocks_the_membership_api_guard(pool: PgPool) {
        let squatter = users::create(&pool, "squatter@example.com").await.unwrap();
        let team = teams::create(&pool, "rise-operators").await.unwrap();
        teams::add_member(&pool, team.id, squatter.id, TeamRole::Owner)
            .await
            .unwrap();

        // A takeover in flight: purged and flagged, not yet committed.
        let mut takeover = pool.begin().await.unwrap();
        let locked = teams::find_by_name_for_update(&mut *takeover, "rise-operators")
            .await
            .unwrap()
            .expect("the team exists");
        teams::remove_all_team_members(&mut *takeover, locked.id)
            .await
            .unwrap();
        teams::set_idp_managed(&mut *takeover, locked.id, true)
            .await
            .unwrap();

        let team_id = team.id;
        let squatter_id = squatter.id;
        let writer_pool = pool.clone();
        // The membership API's own guard — `lock_and_guard_team`, the function
        // both `update_team` and `delete_team` call — not a copy of it. Revert
        // the lock inside that function and this fails.
        let writer = async move {
            let mut tx = writer_pool.begin().await.unwrap();
            let refused = crate::server::team::handlers::lock_and_guard_team(
                &mut tx, team_id, false, // the squatter is not an admin
                "modify",
            )
            .await
            .is_err();
            if !refused {
                teams::add_member(&mut *tx, team_id, squatter_id, TeamRole::Member)
                    .await
                    .unwrap();
                tx.commit().await.unwrap();
            } else {
                tx.rollback().await.unwrap();
            }
            refused
        };

        let (_, refused) = tokio::join!(
            async {
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                takeover.commit().await.unwrap();
            },
            writer
        );

        assert!(
            refused,
            "the membership API read a pre-takeover snapshot and would have written through it"
        );
        assert_eq!(
            teams::list_idp_group_names_for_user(&pool, squatter.id)
                .await
                .unwrap(),
            Vec::<String>::new(),
            "the squatter must not have slipped a membership through the window"
        );
    }

    /// A service account must not become a team principal by the create door.
    /// `update_team` has always refused one; `create_team` inserted whatever it
    /// was given, reaching the state that rule exists to prevent — team
    /// membership is what the `Member` access class keys on.
    ///
    /// Calls the guard the handlers call, so a handler that stops calling it
    /// cannot leave this green.
    #[sqlx::test]
    async fn a_service_account_is_refused_as_a_team_principal(pool: PgPool) {
        use crate::db::models::ProjectStatus;
        use crate::db::{projects, service_accounts};
        use crate::server::team::handlers::reject_service_account_principals;

        let human = users::create(&pool, "human@example.com").await.unwrap();
        let project = projects::create(
            &pool,
            "sa-guard",
            ProjectStatus::Stopped,
            "public".to_string(),
            Some(human.id),
            None,
            None,
        )
        .await
        .unwrap();
        let sa = service_accounts::create(
            &pool,
            project.id,
            "https://gitlab.example",
            &std::collections::HashMap::new(),
        )
        .await
        .unwrap();

        let mut tx = pool.begin().await.unwrap();
        reject_service_account_principals(&mut tx, [human.id])
            .await
            .expect("an ordinary user is a fine team principal");
        let refused = reject_service_account_principals(&mut tx, [sa.user_id]).await;
        tx.rollback().await.unwrap();

        let err = refused.expect_err("a service account must be refused");
        assert!(
            err.message
                .contains("Service accounts cannot be team members"),
            "{}",
            err.message
        );
    }
}
