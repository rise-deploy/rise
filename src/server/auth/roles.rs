//! Role matching and IdP group resolution.
//!
//! A role — platform access, admin, or Operator — is granted either by an
//! explicit email allowlist (`auth.admin_users`, `auth.operator_users`,
//! `auth.platform_access.allowed_user_emails`) or by IdP group membership
//! (`auth.admin_idp_groups`, `auth.operator_idp_groups`,
//! `auth.platform_access.allowed_idp_groups`). Both forms match
//! case-insensitively.
//!
//! Rise never sees the configured IdP group claim outside of login: `sync_user_groups`
//! (and the Entra active sync) mirror it into `idp_managed` teams, which is the
//! record consulted here. Teams a user created themselves are not IdP-managed
//! and therefore never grant a group-derived role.

use crate::db::models::User;
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

/// Resolve a role from an email allowlist plus a list of IdP groups.
///
/// A user holds the role if their email is on the allowlist or they belong to
/// one of the groups. The allowlist is checked first, and the groups are only
/// looked up when the role actually configures them, so an install that grants
/// roles by email alone never pays for a query.
pub async fn has_role<'e, E>(
    executor: E,
    allowed_emails: &[String],
    allowed_groups: &[String],
    user: &User,
) -> bool
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    if matches_any_email(allowed_emails, &user.email) {
        return true;
    }
    if allowed_groups.is_empty() {
        return false;
    }
    matches_any_group(allowed_groups, &resolve_idp_groups(executor, user.id).await)
}

/// [`has_role`], but reporting the group lookup's failure instead of failing
/// closed.
///
/// The fail-closed form is right where a transient error should cost a role
/// rather than the request — an ingress subrequest, a typed API check. It is
/// wrong inside a `SERIALIZABLE` transaction: a lost race there is not a
/// failure to answer but an instruction to replay, and swallowing it decides
/// the caller's standing from a transaction that is already doomed. The
/// resource API's membership resolver uses this form so the classification
/// reaches its retry loop.
pub async fn has_role_checked<'e, E>(
    executor: E,
    allowed_emails: &[String],
    allowed_groups: &[String],
    user: &User,
) -> Result<bool, anyhow::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    if matches_any_email(allowed_emails, &user.email) {
        return Ok(true);
    }
    if allowed_groups.is_empty() {
        return Ok(false);
    }
    let groups = crate::db::teams::list_idp_group_names_for_user(executor, user.id).await?;
    Ok(matches_any_group(allowed_groups, &groups))
}

/// Resolve the IdP groups a user belongs to.
///
/// Fails closed: a database error yields no groups (and is logged), so a
/// transient failure can only deny a group-derived role, never grant one.
pub async fn resolve_idp_groups<'e, E>(executor: E, user_id: Uuid) -> Vec<String>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    match crate::db::teams::list_idp_group_names_for_user(executor, user_id).await {
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
    use sqlx::PgPool;

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

    /// `has_role` grants by email allowlist or by IdP group, and only the group
    /// path touches the database.
    #[sqlx::test]
    async fn test_has_role_by_email_or_idp_group(pool: PgPool) {
        let user = users::create(&pool, "user@example.com").await.unwrap();
        let emails = vec!["User@Example.Com".to_string()];
        // Team names are constrained to lowercase, so the case difference that
        // matters in practice is on the configured side.
        let groups = vec!["Platform-Admins".to_string()];

        // Email allowlist alone, no groups configured or joined.
        assert!(has_role(&pool, &emails, &[], &user).await);

        // Neither the allowlist nor any group matches yet.
        assert!(!has_role(&pool, &[], &groups, &user).await);

        // Joining the IdP-managed group grants the role.
        let team = teams::create(&pool, "platform-admins").await.unwrap();
        teams::set_idp_managed(&pool, team.id, true).await.unwrap();
        teams::add_member(&pool, team.id, user.id, TeamRole::Member)
            .await
            .unwrap();
        assert!(has_role(&pool, &[], &groups, &user).await);

        // Nothing configured grants nothing.
        assert!(!has_role(&pool, &[], &[], &user).await);
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
