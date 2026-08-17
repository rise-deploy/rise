//! URL path parsing for the generic resource API.
//!
//! This module provides the *pure-syntax* half of resource-path handling:
//!
//! * [`parse_uid_token`] — parses a `uid:<uuid>` token into its `Uuid`.
//! * [`parse_resource_path`] — splits a full resource URL path into a
//!   [`RawResourcePath`]: the leaf collection (`{group}/{version}/{plural}`)
//!   plus the trailing identifier segments, verbatim.
//!
//! `parse_resource_path` deliberately does *not* decide whether a path is a
//! list, an item, or a subresource: a flat chain of names is ambiguous without
//! knowing the leaf kind's parent-chain depth, which only the resource store
//! knows. Store-aware classification lives in [`super::handlers`].

use uuid::Uuid;

use crate::server::error::ServerError;

/// Prefix marking an identifier segment as a UID rather than a name.
pub const UID_PREFIX: &str = "uid:";

/// Parse a `uid:<uuid>` token into its `Uuid`.
///
/// Returns a 400 if `raw` is not `uid:`-prefixed or the remainder is not a
/// well-formed UUID — the caller does not need to distinguish.
pub fn parse_uid_token(raw: &str) -> Result<Uuid, ServerError> {
    let rest = raw
        .strip_prefix(UID_PREFIX)
        .ok_or_else(|| ServerError::bad_request(format!("expected a uid: token, got '{raw}'")))?;
    rest.parse().map_err(|_| {
        ServerError::bad_request(format!("invalid uid token '{raw}': expected uid:<uuid>"))
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct CollectionRef {
    pub group: String,
    pub version: String,
    pub plural: String,
}

/// The well-known sub-resource suffixes that can appear at the end of a path.
#[derive(Debug, Clone, PartialEq)]
pub enum Subresource {
    Status,
    Finalizers,
    DeletionBlockers,
}

impl Subresource {
    pub const KEYWORDS: &str = "status, finalizers, deletion-blockers";

    /// Parse a reserved subresource keyword. Returns `None` for any other value.
    pub fn from_keyword(value: &str) -> Option<Self> {
        match value {
            "status" => Some(Self::Status),
            "finalizers" => Some(Self::Finalizers),
            "deletion-blockers" => Some(Self::DeletionBlockers),
            _ => None,
        }
    }
}

/// A resource URL path split into its leaf collection and the trailing
/// identifier segments.
///
/// No list/item/subresource classification is applied — that requires the leaf
/// kind's parent-chain depth (see [`super::handlers`]).
#[derive(Debug, PartialEq)]
pub enum RawResourcePath {
    /// The literal path `pending-deletion`: resources tombstoned and awaiting GC.
    PendingDeletion,
    /// A `{group}/{version}/{plural}` collection followed by zero or more
    /// ancestor-name / leaf-name / subresource segments, taken verbatim.
    Collection {
        collection: CollectionRef,
        segments: Vec<String>,
    },
}

/// Parse a URL resource path into a [`RawResourcePath`].
///
/// The path is a `/`-separated sequence of segments. Empty paths and empty
/// segments (e.g. a trailing `/`) are rejected with a 400 error.
///
/// # Grammar
///
/// ```text
/// path        ::= "pending-deletion"
///               | group "/" version "/" plural ( "/" segment )*
/// ```
///
/// The leading `{group}/{version}/{plural}` names the *leaf* collection. The
/// trailing `segment`s are ancestor names, the leaf identifier, and an optional
/// subresource keyword — but distinguishing them requires the leaf kind's
/// parent-chain depth, so that is left to store-aware classification.
/// `pending-deletion` is valid only as the sole path segment.
pub fn parse_resource_path(raw: &str) -> Result<RawResourcePath, ServerError> {
    let segments: Vec<&str> = raw.split('/').collect();

    if segments.is_empty() || segments.iter().any(|s| s.is_empty()) {
        return Err(ServerError::bad_request("empty resource path segment"));
    }

    // `pending-deletion` is the sole-segment diagnostics path. It is never an
    // identifier, so a resource may legitimately be named `pending-deletion`.
    if segments.len() == 1 && segments[0] == "pending-deletion" {
        return Ok(RawResourcePath::PendingDeletion);
    }

    let [group, version, plural, rest @ ..] = segments.as_slice() else {
        return Err(ServerError::bad_request(
            "resource collection paths must start with '{group}/{version}/{plural}'",
        ));
    };

    Ok(RawResourcePath::Collection {
        collection: CollectionRef {
            group: (*group).to_string(),
            version: (*version).to_string(),
            plural: (*plural).to_string(),
        },
        segments: rest.iter().map(|s| (*s).to_string()).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_uid_token_succeeds_for_valid_uuid() {
        let uid = Uuid::new_v4();
        let raw = format!("uid:{uid}");
        assert_eq!(parse_uid_token(&raw).unwrap(), uid);
    }

    #[test]
    fn parse_uid_token_rejects_malformed_uuid() {
        let err = parse_uid_token("uid:not-a-uuid").unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
        assert!(err.message.contains("invalid uid token"));
    }

    #[test]
    fn parse_uid_token_rejects_non_uid_prefix() {
        let err = parse_uid_token("acme").unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn subresource_from_keyword() {
        assert_eq!(
            Subresource::from_keyword("status"),
            Some(Subresource::Status)
        );
        assert_eq!(
            Subresource::from_keyword("finalizers"),
            Some(Subresource::Finalizers)
        );
        assert_eq!(
            Subresource::from_keyword("deletion-blockers"),
            Some(Subresource::DeletionBlockers)
        );
        assert_eq!(Subresource::from_keyword("reparent"), None);
        assert_eq!(Subresource::from_keyword("widgets"), None);
    }

    fn collection(group: &str, version: &str, plural: &str) -> CollectionRef {
        CollectionRef {
            group: group.into(),
            version: version.into(),
            plural: plural.into(),
        }
    }

    #[test]
    fn resource_path_pending_deletion() {
        assert_eq!(
            parse_resource_path("pending-deletion").unwrap(),
            RawResourcePath::PendingDeletion
        );
    }

    #[test]
    fn resource_path_collection_only() {
        assert_eq!(
            parse_resource_path("rise.dev/v1alpha1/organizations").unwrap(),
            RawResourcePath::Collection {
                collection: collection("rise.dev", "v1alpha1", "organizations"),
                segments: vec![],
            }
        );
    }

    #[test]
    fn resource_path_collection_with_trailing_segments() {
        // The trailing segments are returned verbatim — no name/uid/subresource
        // interpretation happens here.
        assert_eq!(
            parse_resource_path("example.dev/v1/widgets/acme/w1/status").unwrap(),
            RawResourcePath::Collection {
                collection: collection("example.dev", "v1", "widgets"),
                segments: vec!["acme".into(), "w1".into(), "status".into()],
            }
        );
    }

    #[test]
    fn resource_path_rejects_unversioned_collection() {
        // Fewer than three leading segments cannot name a `{group}/{version}/{plural}`.
        let err = parse_resource_path("organizations/acme").unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn resource_path_rejects_empty_segment() {
        let err = parse_resource_path("example.dev/v1/widgets/").unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
    }
}
