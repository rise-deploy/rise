//! Ingress access-control model shared between `rise-deploy`'s settings and the
//! deployment backends.

use serde::{Deserialize, Serialize};

/// Access requirement level for project ingress.
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
