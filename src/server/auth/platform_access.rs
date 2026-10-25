//! Platform access control
//!
//! Determines whether a user has access to Rise platform features (CLI/API/Dashboard)
//! vs application-only access (ingress auth).
//!
//! Access is granted by an email allowlist or by IdP group membership; the
//! groups are the user's IdP-managed team names, resolved by
//! [`crate::server::auth::roles::resolve_idp_groups`].

use crate::db::models::User;
use crate::server::auth::roles::{matches_any_email, matches_any_group};
use crate::server::settings::{PlatformAccessConfig, PlatformAccessPolicy};

/// Trait for checking platform access permissions
pub trait PlatformAccessChecker {
    /// Check if a user has platform access based on email and IdP groups
    fn has_platform_access(&self, user: &User, idp_groups: &[String]) -> bool;
}

/// Config-based platform access checker
pub struct ConfigBasedAccessChecker<'a> {
    pub config: &'a PlatformAccessConfig,
    pub admin_users: &'a [String],
    pub admin_idp_groups: &'a [String],
    pub operator_users: &'a [String],
    pub operator_idp_groups: &'a [String],
}

impl<'a> PlatformAccessChecker for ConfigBasedAccessChecker<'a> {
    fn has_platform_access(&self, user: &User, idp_groups: &[String]) -> bool {
        // Admin users always have platform access
        if matches_any_email(self.admin_users, &user.email)
            || matches_any_group(self.admin_idp_groups, idp_groups)
        {
            return true;
        }

        // Operators always have platform access — they manage the generic
        // resource API and must be able to reach it regardless of the
        // platform access policy.
        if matches_any_email(self.operator_users, &user.email)
            || matches_any_group(self.operator_idp_groups, idp_groups)
        {
            return true;
        }

        match self.config.policy {
            PlatformAccessPolicy::AllowAll => true,
            PlatformAccessPolicy::Restrictive => {
                matches_any_email(&self.config.allowed_user_emails, &user.email)
                    || matches_any_group(&self.config.allowed_idp_groups, idp_groups)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn make_user(email: &str) -> User {
        User {
            id: Uuid::new_v4(),
            email: email.to_string(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn config(
        policy: PlatformAccessPolicy,
        emails: &[&str],
        groups: &[&str],
    ) -> PlatformAccessConfig {
        PlatformAccessConfig {
            policy,
            allowed_user_emails: emails.iter().map(|s| s.to_string()).collect(),
            allowed_idp_groups: groups.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_allow_all_policy() {
        let config = config(PlatformAccessPolicy::AllowAll, &[], &[]);
        let checker = ConfigBasedAccessChecker {
            config: &config,
            admin_users: &[],
            admin_idp_groups: &[],
            operator_users: &[],
            operator_idp_groups: &[],
        };

        let user = make_user("anyone@example.com");
        assert!(checker.has_platform_access(&user, &[]));
    }

    #[test]
    fn test_admin_always_has_access() {
        let config = config(PlatformAccessPolicy::Restrictive, &[], &[]);
        let admin_users = strings(&["admin@example.com"]);
        let checker = ConfigBasedAccessChecker {
            config: &config,
            admin_users: &admin_users,
            admin_idp_groups: &[],
            operator_users: &[],
            operator_idp_groups: &[],
        };

        let user = make_user("admin@example.com");
        assert!(checker.has_platform_access(&user, &[]));
    }

    #[test]
    fn test_operator_always_has_access() {
        let config = config(PlatformAccessPolicy::Restrictive, &[], &[]);
        let operator_users = strings(&["ops@example.com"]);
        let checker = ConfigBasedAccessChecker {
            config: &config,
            admin_users: &[],
            admin_idp_groups: &[],
            operator_users: &operator_users,
            operator_idp_groups: &[],
        };

        // Operator passes platform access even under Restrictive with no
        // allowlists — they need the generic resource API.
        let user = make_user("ops@example.com");
        assert!(checker.has_platform_access(&user, &[]));

        // Email matching is case-insensitive.
        let user_upper = make_user("OPS@EXAMPLE.COM");
        assert!(checker.has_platform_access(&user_upper, &[]));

        // Non-operator without allowlist match is still denied.
        let other = make_user("someone@example.com");
        assert!(!checker.has_platform_access(&other, &[]));
    }

    /// Admins and Operators granted by IdP group bypass the platform access
    /// policy exactly as their email-listed counterparts do.
    #[test]
    fn test_admin_and_operator_groups_always_have_access() {
        let config = config(PlatformAccessPolicy::Restrictive, &[], &[]);
        let admin_idp_groups = strings(&["platform-admins"]);
        let operator_idp_groups = strings(&["platform-operators"]);
        let checker = ConfigBasedAccessChecker {
            config: &config,
            admin_users: &[],
            admin_idp_groups: &admin_idp_groups,
            operator_users: &[],
            operator_idp_groups: &operator_idp_groups,
        };

        let user = make_user("user@example.com");
        assert!(checker.has_platform_access(&user, &strings(&["Platform-Admins"])));
        assert!(checker.has_platform_access(&user, &strings(&["platform-operators"])));
        assert!(!checker.has_platform_access(&user, &strings(&["engineering"])));
    }

    #[test]
    fn test_restrictive_email_allowlist() {
        let config = config(
            PlatformAccessPolicy::Restrictive,
            &["allowed@example.com"],
            &[],
        );
        let checker = ConfigBasedAccessChecker {
            config: &config,
            admin_users: &[],
            admin_idp_groups: &[],
            operator_users: &[],
            operator_idp_groups: &[],
        };

        let allowed_user = make_user("allowed@example.com");
        assert!(checker.has_platform_access(&allowed_user, &[]));

        let denied_user = make_user("denied@example.com");
        assert!(!checker.has_platform_access(&denied_user, &[]));
    }

    #[test]
    fn test_restrictive_group_allowlist() {
        let config = config(PlatformAccessPolicy::Restrictive, &[], &["engineering"]);
        let checker = ConfigBasedAccessChecker {
            config: &config,
            admin_users: &[],
            admin_idp_groups: &[],
            operator_users: &[],
            operator_idp_groups: &[],
        };

        let user = make_user("user@example.com");

        // User with allowed group
        assert!(checker.has_platform_access(&user, &strings(&["engineering"])));

        // User without allowed group
        assert!(!checker.has_platform_access(&user, &strings(&["marketing"])));

        // User with no groups
        assert!(!checker.has_platform_access(&user, &[]));
    }

    #[test]
    fn test_case_insensitive_matching() {
        let config = config(
            PlatformAccessPolicy::Restrictive,
            &["User@Example.Com"],
            &["Engineering"],
        );
        let admin_users = strings(&["Admin@Example.Com"]);
        let checker = ConfigBasedAccessChecker {
            config: &config,
            admin_users: &admin_users,
            admin_idp_groups: &[],
            operator_users: &[],
            operator_idp_groups: &[],
        };

        // Email case insensitive
        let user1 = make_user("user@example.com");
        assert!(checker.has_platform_access(&user1, &[]));

        // Admin case insensitive
        let user2 = make_user("admin@example.com");
        assert!(checker.has_platform_access(&user2, &[]));

        // Group case insensitive
        let user3 = make_user("other@example.com");
        assert!(checker.has_platform_access(&user3, &strings(&["engineering"])));
    }
}
