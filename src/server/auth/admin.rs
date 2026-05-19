/// Check if a user is an admin (case-insensitive email match)
///
/// This function provides a centralized admin check that uses case-insensitive
/// email comparison, consistent with the platform access middleware.
///
/// # Arguments
/// * `admin_users` - List of admin email addresses from configuration
/// * `user_email` - Email address of the user to check
///
/// # Returns
/// `true` if the user email matches any admin email (case-insensitive), `false` otherwise
pub fn is_admin_user(admin_users: &[String], user_email: &str) -> bool {
    admin_users
        .iter()
        .any(|admin| admin.eq_ignore_ascii_case(user_email))
}

/// Check if a user has the Operator role (case-insensitive email match).
///
/// Operators are a separate role from admins: admins do NOT implicitly receive
/// Operator access. List the email in both `auth.admin_users` and
/// `auth.operator_users` if a user needs both.
pub fn is_operator_user(operator_users: &[String], user_email: &str) -> bool {
    operator_users
        .iter()
        .any(|op| op.eq_ignore_ascii_case(user_email))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_admin_user_case_insensitive() {
        let admin_users = vec!["Admin@Example.Com".to_string(), "user@test.org".to_string()];

        // Test exact match
        assert!(is_admin_user(&admin_users, "Admin@Example.Com"));

        // Test case variations
        assert!(is_admin_user(&admin_users, "admin@example.com"));
        assert!(is_admin_user(&admin_users, "ADMIN@EXAMPLE.COM"));
        assert!(is_admin_user(&admin_users, "AdMiN@eXaMpLe.CoM"));

        // Test second admin
        assert!(is_admin_user(&admin_users, "user@test.org"));
        assert!(is_admin_user(&admin_users, "USER@TEST.ORG"));

        // Test non-admin
        assert!(!is_admin_user(&admin_users, "other@example.com"));
        assert!(!is_admin_user(&admin_users, ""));
    }

    #[test]
    fn test_is_admin_user_empty_list() {
        let admin_users: Vec<String> = vec![];
        assert!(!is_admin_user(&admin_users, "admin@example.com"));
    }

    #[test]
    fn test_is_operator_user_case_insensitive() {
        let operator_users = vec!["Ops@Example.Com".to_string(), "second@test.org".to_string()];

        assert!(is_operator_user(&operator_users, "Ops@Example.Com"));
        assert!(is_operator_user(&operator_users, "ops@example.com"));
        assert!(is_operator_user(&operator_users, "OPS@EXAMPLE.COM"));
        assert!(is_operator_user(&operator_users, "second@test.org"));
        assert!(is_operator_user(&operator_users, "SECOND@TEST.ORG"));

        assert!(!is_operator_user(&operator_users, "other@example.com"));
        assert!(!is_operator_user(&operator_users, ""));
    }

    #[test]
    fn test_is_operator_user_empty_list() {
        let operator_users: Vec<String> = vec![];
        assert!(!is_operator_user(&operator_users, "ops@example.com"));
    }
}
