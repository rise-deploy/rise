//! Ingress access-control model shared between `rise-deploy`'s settings, the
//! `.rise.toml` route configuration, and the deployment backends.

use serde::{Deserialize, Serialize};

/// Access requirement level for ingress authentication.
///
/// This is the internal/wire representation: the `access_requirement` of a
/// server-configured access class, the per-route requirement carried on a
/// deployment's route side-data, and the `access` query param the reconciler
/// stamps into the auth-url. Spelled PascalCase to match the access-class
/// settings. The `.rise.toml` surface uses [`RouteAccess`] instead, whose
/// `public` reads more naturally on a route than `None`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "PascalCase")]
pub enum AccessRequirement {
    /// No authentication required - fully public access
    None,
    /// Must be authenticated, but no project membership required
    Authenticated,
    /// Must be authenticated AND have project membership (owner or team member)
    Member,
}

/// Per-route access spelling used in `.rise.toml` `[routes].access`.
///
/// A route's auth requirement reads more naturally as `public` than the
/// access-class `None` ("no access" vs. "no requirement" is ambiguous), so the
/// config surface uses this lowercase enum and maps it to [`AccessRequirement`]
/// when the route is resolved. Only the spelling differs — the semantics are
/// identical.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum RouteAccess {
    /// Open to everyone — no authentication (maps to [`AccessRequirement::None`]).
    Public,
    /// Any authenticated user.
    Authenticated,
    /// Project owner or team member only.
    Member,
}

impl From<RouteAccess> for AccessRequirement {
    fn from(access: RouteAccess) -> Self {
        match access {
            RouteAccess::Public => AccessRequirement::None,
            RouteAccess::Authenticated => AccessRequirement::Authenticated,
            RouteAccess::Member => AccessRequirement::Member,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AccessRequirement, RouteAccess};

    #[test]
    fn access_requirement_uses_pascalcase() {
        assert_eq!(
            serde_json::from_str::<AccessRequirement>("\"Authenticated\"").unwrap(),
            AccessRequirement::Authenticated
        );
        assert_eq!(
            serde_json::to_string(&AccessRequirement::Member).unwrap(),
            "\"Member\""
        );
    }

    #[test]
    fn route_access_parses_lowercase_and_maps_to_requirement() {
        let parse = |s: &str| serde_json::from_str::<RouteAccess>(s).unwrap();
        assert_eq!(parse("\"public\""), RouteAccess::Public);
        assert_eq!(parse("\"authenticated\""), RouteAccess::Authenticated);
        assert_eq!(parse("\"member\""), RouteAccess::Member);

        // `public` reads as "open" and maps to the internal `None` requirement.
        assert_eq!(
            AccessRequirement::from(RouteAccess::Public),
            AccessRequirement::None
        );
        assert_eq!(
            AccessRequirement::from(RouteAccess::Member),
            AccessRequirement::Member
        );
    }
}
