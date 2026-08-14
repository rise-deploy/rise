//! Role matching and IdP group resolution.
//!
//! A role — platform access, admin, or Operator — is granted either by an
//! explicit email allowlist (`auth.admin_users`, `auth.operator_users`,
//! `auth.platform_access.allowed_user_emails`) or by IdP group membership
//! (`auth.admin_idp_groups`, `auth.operator_idp_groups`,
//! `auth.platform_access.allowed_idp_groups`). Both forms match
//! case-insensitively.
//!
//! Rise never sees the IdP's `groups` claim outside of login: `sync_user_groups`
//! (and the Entra active sync) mirror it into `idp_managed` teams, which is the
//! record consulted here. Teams a user created themselves are not IdP-managed
//! and therefore never grant a group-derived role.

use sqlx::PgPool;
use uuid::Uuid;

/// Check whether an email appears in a configured allowlist (case-insensitive).
pub fn matches_any_email(allowed_emails: &[String], user_email: &str) -> bool {
    allowed_emails
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(user_email))
}

/// Check whether any of a user's IdP groups appears in a configured group list
/// (case-insensitive).
///
/// Used to grant platform access, the admin role, or the Operator role by IdP
/// group instead of by an explicit per-user allowlist.
pub fn matches_any_group(configured_groups: &[String], user_groups: &[String]) -> bool {
    configured_groups.iter().any(|configured| {
        user_groups
            .iter()
            .any(|user_group| user_group.eq_ignore_ascii_case(configured))
    })
}

/// Resolve the IdP groups a user belongs to.
///
/// Fails closed: a database error yields no groups (and is logged), so a
/// transient failure can only deny a group-derived role, never grant one.
pub async fn resolve_idp_groups(pool: &PgPool, user_id: Uuid) -> Vec<String> {
    match crate::db::teams::list_idp_group_names_for_user(pool, user_id).await {
        Ok(groups) => groups,
        Err(e) => {
            tracing::error!(
                user_id = %user_id,
                "Failed to resolve IdP groups; treating the user as belonging to none: {:?}",
                e
            );
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{models::TeamRole, teams, users};

    #[test]
    fn test_matches_any_group_case_insensitive() {
        let configured = vec!["Platform-Admins".to_string(), "sre".to_string()];

        assert!(matches_any_group(
            &configured,
            &["platform-admins".to_string()]
        ));
        assert!(matches_any_group(&configured, &["SRE".to_string()]));
        assert!(matches_any_group(
            &configured,
            &["engineering".to_string(), "sre".to_string()]
        ));

        assert!(!matches_any_group(
            &configured,
            &["engineering".to_string()]
        ));
        assert!(!matches_any_group(&configured, &[]));
        assert!(!matches_any_group(&[], &["sre".to_string()]));
    }

    #[test]
    fn test_matches_any_email_case_insensitive() {
        let allowed = vec!["Admin@Example.Com".to_string(), "user@test.org".to_string()];

        assert!(matches_any_email(&allowed, "Admin@Example.Com"));
        assert!(matches_any_email(&allowed, "admin@example.com"));
        assert!(matches_any_email(&allowed, "ADMIN@EXAMPLE.COM"));
        assert!(matches_any_email(&allowed, "USER@TEST.ORG"));

        assert!(!matches_any_email(&allowed, "other@example.com"));
        assert!(!matches_any_email(&allowed, ""));
        assert!(!matches_any_email(&[], "admin@example.com"));
    }

    /// Only IdP-managed teams count as IdP groups — a team the user created
    /// themselves must never grant a group-derived role.
    #[sqlx::test]
    async fn test_resolve_idp_groups_excludes_self_created_teams(pool: PgPool) {
        let user = users::create(&pool, "user@example.com").await.unwrap();

        let idp_team = teams::create(&pool, "platform-admins").await.unwrap();
        teams::set_idp_managed(&pool, idp_team.id, true)
            .await
            .unwrap();
        teams::add_member(&pool, idp_team.id, user.id, TeamRole::Member)
            .await
            .unwrap();

        // A team the user made up, named like a privileged group.
        let self_made = teams::create(&pool, "operators").await.unwrap();
        teams::add_member(&pool, self_made.id, user.id, TeamRole::Owner)
            .await
            .unwrap();

        let groups = resolve_idp_groups(&pool, user.id).await;
        assert_eq!(groups, vec!["platform-admins".to_string()]);
    }

    #[sqlx::test]
    async fn test_resolve_idp_groups_excludes_other_users_groups(pool: PgPool) {
        let user = users::create(&pool, "user@example.com").await.unwrap();
        let other = users::create(&pool, "other@example.com").await.unwrap();

        let team = teams::create(&pool, "sre").await.unwrap();
        teams::set_idp_managed(&pool, team.id, true).await.unwrap();
        teams::add_member(&pool, team.id, other.id, TeamRole::Member)
            .await
            .unwrap();

        assert!(resolve_idp_groups(&pool, user.id).await.is_empty());
    }
}
