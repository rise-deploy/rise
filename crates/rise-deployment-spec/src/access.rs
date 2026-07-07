//! Ingress access-control model shared between `rise-deploy`'s settings, the
//! `.rise.toml` route configuration, and the deployment backends.

use serde::{Deserialize, Serialize};

/// Access requirement level for ingress authentication.
///
/// Used both as the `access_requirement` of a server-configured access class
/// (project-wide) and as the optional per-route `access` override in
/// `.rise.toml`. The canonical spelling is PascalCase (matching the access-class
/// settings); lowercase spellings are accepted as aliases for convenience in
/// `.rise.toml`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "PascalCase")]
pub enum AccessRequirement {
    /// No authentication required - fully public access
    #[serde(alias = "none")]
    None,
    /// Must be authenticated, but no project membership required
    #[serde(alias = "authenticated")]
    Authenticated,
    /// Must be authenticated AND have project membership (owner or team member)
    #[serde(alias = "member")]
    Member,
}

#[cfg(test)]
mod tests {
    use super::AccessRequirement;

    #[test]
    fn parses_pascalcase_and_lowercase_aliases() {
        // Canonical PascalCase (as in access-class settings).
        assert_eq!(
            serde_json::from_str::<AccessRequirement>("\"Authenticated\"").unwrap(),
            AccessRequirement::Authenticated
        );
        // Lowercase aliases for `.rise.toml` convenience.
        assert_eq!(
            serde_json::from_str::<AccessRequirement>("\"none\"").unwrap(),
            AccessRequirement::None
        );
        assert_eq!(
            serde_json::from_str::<AccessRequirement>("\"member\"").unwrap(),
            AccessRequirement::Member
        );
    }

    #[test]
    fn serializes_pascalcase() {
        assert_eq!(
            serde_json::to_string(&AccessRequirement::Member).unwrap(),
            "\"Member\""
        );
    }
}
