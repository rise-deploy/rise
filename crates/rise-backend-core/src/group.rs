//! Deployment-group naming helpers shared between `rise-deploy` and the
//! deployment backends.

/// The default deployment group name.
pub const DEFAULT_DEPLOYMENT_GROUP: &str = "default";

/// Normalize a deployment group name for use in URLs and resource names.
///
/// Replaces sequences of characters that are not alphanumeric, `-`, `_`, or `.`
/// with `--` (e.g., `mr/123` → `mr--123`). The result is also trimmed so it
/// starts and ends with an alphanumeric character, satisfying the Kubernetes
/// label value regex: `(([A-Za-z0-9][-A-Za-z0-9_.]*)?[A-Za-z0-9])?`
///
/// **Collision safety**: This function is injective (collision-free) only when
/// input group names do not contain `--`. The deployment group validation in
/// `is_valid_group_name` enforces this constraint.
///
/// This matches the normalization used in the `{deployment_group}` placeholder
/// of `staging_ingress_url_template`.
pub fn normalize_deployment_group(deployment_group: &str) -> String {
    let mut result = String::new();
    let mut last_was_invalid = false;

    for ch in deployment_group.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
            result.push(ch);
            last_was_invalid = false;
        } else if !last_was_invalid {
            result.push_str("--");
            last_was_invalid = true;
        }
    }

    result
        .trim_matches(|c: char| !c.is_ascii_alphanumeric())
        .to_string()
}
