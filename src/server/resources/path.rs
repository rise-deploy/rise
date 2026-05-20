//! URL path parsing for the generic resource API.
//!
//! This module provides two parsing functions:
//!
//! * [`parse_identifier`] — turns a single `<identifier>` segment into a
//!   `PathSegment` (name or `uid:<uuid>` form) for the resource store.
//! * [`parse_resource_path`] — parses a full resource URL path like
//!   `organizations/acme/widgets/w1/status` into a typed [`ResourcePath`],
//!   expressing the collection hierarchy, the leaf resource, and any
//!   subresource keyword.

use rise_resource_store::PathSegment;
use uuid::Uuid;

use crate::server::error::ServerError;

const UID_PREFIX: &str = "uid:";

/// Parse a URL path identifier into a `PathSegment`.
///
/// A bare value is treated as a name; a `uid:<uuid>` prefix selects a UID.
/// Malformed UIDs return 400 — the caller does not need to distinguish.
pub fn parse_identifier(
    api_version: &str,
    kind: &str,
    raw: &str,
) -> Result<PathSegment, ServerError> {
    if let Some(rest) = raw.strip_prefix(UID_PREFIX) {
        let uid: Uuid = rest.parse().map_err(|_| {
            ServerError::bad_request(format!("invalid uid token '{raw}': expected uid:<uuid>"))
        })?;
        Ok(PathSegment::Uid {
            api_version: api_version.to_string(),
            kind: kind.to_string(),
            uid,
        })
    } else {
        Ok(PathSegment::Name {
            api_version: api_version.to_string(),
            kind: kind.to_string(),
            name: raw.to_string(),
        })
    }
}

/// A single ancestor segment in a hierarchical resource path, e.g. `organizations/acme`.
#[derive(Debug, PartialEq)]
#[allow(dead_code)]
pub struct AncestorRef {
    pub collection: String,
    pub identifier: String,
}

/// The well-known sub-resource suffixes that can appear at the end of a path.
#[derive(Debug, PartialEq)]
#[allow(dead_code)]
pub enum Subresource {
    Status,
    Finalizers,
    Reparent,
}

/// A parsed representation of a hierarchical resource URL path.
#[derive(Debug, PartialEq)]
#[allow(dead_code)]
pub enum ResourcePath {
    /// The literal path `orphans`.
    Orphans,
    /// A collection, optionally nested under ancestors: `<ancestors…>/<collection>`.
    List {
        ancestors: Vec<AncestorRef>,
        collection: String,
    },
    /// A single item: `<ancestors…>/<collection>/<identifier>`.
    Item {
        ancestors: Vec<AncestorRef>,
        collection: String,
        identifier: String,
    },
    /// A sub-resource operation on an item: `<ancestors…>/<collection>/<identifier>/<sub>`.
    Subresource {
        ancestors: Vec<AncestorRef>,
        collection: String,
        identifier: String,
        subresource: Subresource,
    },
}

/// Keywords that are reserved and may not be used as collection names or identifiers
/// (except in their designated positions).
#[allow(dead_code)]
const RESERVED: &[&str] = &["orphans", "status", "finalizers", "reparent"];

/// Parse a URL resource path into a [`ResourcePath`].
///
/// The path is a `/`-separated sequence of segments. Empty paths and empty
/// segments (e.g. a trailing `/`) are rejected with a 400 error.
///
/// # Grammar
///
/// ```text
/// path      ::= "orphans"
///             | segment+
/// segment   ::= collection "/" identifier
/// tail      ::= collection              -- List
///             | collection "/" name     -- Item
///             | collection "/" name "/" subresource
/// subresource ::= "status" | "finalizers" | "reparent"
/// ```
#[allow(dead_code)]
pub fn parse_resource_path(raw: &str) -> Result<ResourcePath, ServerError> {
    let segments: Vec<&str> = raw.split('/').collect();

    if segments.is_empty() || segments.iter().any(|s| s.is_empty()) {
        return Err(ServerError::bad_request("empty resource path segment"));
    }

    // Special-case: bare `orphans`
    if segments.len() == 1 && segments[0] == "orphans" {
        return Ok(ResourcePath::Orphans);
    }

    let mut ancestors: Vec<AncestorRef> = Vec::new();
    let mut remaining = segments.as_slice();

    loop {
        match remaining.len() {
            0 => {
                // Should be unreachable given earlier checks, but be safe.
                return Err(ServerError::bad_request("empty resource path segment"));
            }
            1 => {
                return Ok(ResourcePath::List {
                    ancestors,
                    collection: remaining[0].to_string(),
                });
            }
            2 => {
                return Ok(ResourcePath::Item {
                    ancestors,
                    collection: remaining[0].to_string(),
                    identifier: remaining[1].to_string(),
                });
            }
            _ => {
                // remaining.len() >= 3
                match remaining[2] {
                    kw @ ("status" | "finalizers" | "reparent") => {
                        if remaining.len() > 3 {
                            return Err(ServerError::bad_request(format!(
                                "unexpected segments after subresource '{}': path must end after \
                                 the keyword",
                                kw
                            )));
                        }
                        let subresource = match kw {
                            "status" => Subresource::Status,
                            "finalizers" => Subresource::Finalizers,
                            _ => Subresource::Reparent,
                        };
                        return Ok(ResourcePath::Subresource {
                            ancestors,
                            collection: remaining[0].to_string(),
                            identifier: remaining[1].to_string(),
                            subresource,
                        });
                    }
                    "orphans" => {
                        return Err(ServerError::bad_request(
                            "'orphans' is a reserved keyword and may not appear at a non-first \
                             path position",
                        ));
                    }
                    _ => {
                        // remaining[2] is a plain segment: push the first two as an ancestor
                        // and continue with remaining[2..].
                        ancestors.push(AncestorRef {
                            collection: remaining[0].to_string(),
                            identifier: remaining[1].to_string(),
                        });
                        remaining = &remaining[2..];
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_name() {
        let seg = parse_identifier("rise.dev/v1alpha1", "Organization", "acme").unwrap();
        match seg {
            PathSegment::Name {
                api_version,
                kind,
                name,
            } => {
                assert_eq!(api_version, "rise.dev/v1alpha1");
                assert_eq!(kind, "Organization");
                assert_eq!(name, "acme");
            }
            other => panic!("expected Name, got {other:?}"),
        }
    }

    #[test]
    fn parses_uid_token() {
        let uid = Uuid::new_v4();
        let raw = format!("uid:{uid}");
        let seg = parse_identifier("rise.dev/v1alpha1", "Organization", &raw).unwrap();
        match seg {
            PathSegment::Uid {
                api_version,
                kind,
                uid: got,
            } => {
                assert_eq!(api_version, "rise.dev/v1alpha1");
                assert_eq!(kind, "Organization");
                assert_eq!(got, uid);
            }
            other => panic!("expected Uid, got {other:?}"),
        }
    }

    #[test]
    fn rejects_malformed_uid_token() {
        let err =
            parse_identifier("rise.dev/v1alpha1", "Organization", "uid:not-a-uuid").unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
        assert!(err.message.contains("invalid uid token"));
    }

    #[test]
    fn does_not_treat_uid_substring_as_uid_token() {
        // Only the literal `uid:` prefix triggers the UID branch; `myuid:x` is a name.
        let seg = parse_identifier("example.dev/v1", "Widget", "myuid:abcd").unwrap();
        assert!(matches!(seg, PathSegment::Name { .. }));
    }

    // ── parse_resource_path tests ────────────────────────────────────────────

    #[test]
    fn resource_path_empty_string_is_error() {
        let err = parse_resource_path("").unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn resource_path_trailing_slash_is_error() {
        let err = parse_resource_path("organizations/").unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn resource_path_leading_slash_is_error() {
        let err = parse_resource_path("/organizations").unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn resource_path_orphans() {
        assert_eq!(
            parse_resource_path("orphans").unwrap(),
            ResourcePath::Orphans
        );
    }

    #[test]
    fn resource_path_list_top_level() {
        assert_eq!(
            parse_resource_path("organizations").unwrap(),
            ResourcePath::List {
                ancestors: vec![],
                collection: "organizations".to_string(),
            }
        );
    }

    #[test]
    fn resource_path_item_top_level() {
        assert_eq!(
            parse_resource_path("organizations/acme").unwrap(),
            ResourcePath::Item {
                ancestors: vec![],
                collection: "organizations".to_string(),
                identifier: "acme".to_string(),
            }
        );
    }

    #[test]
    fn resource_path_list_depth_1() {
        assert_eq!(
            parse_resource_path("organizations/acme/widgets").unwrap(),
            ResourcePath::List {
                ancestors: vec![AncestorRef {
                    collection: "organizations".to_string(),
                    identifier: "acme".to_string(),
                }],
                collection: "widgets".to_string(),
            }
        );
    }

    #[test]
    fn resource_path_item_depth_1() {
        assert_eq!(
            parse_resource_path("organizations/acme/widgets/w1").unwrap(),
            ResourcePath::Item {
                ancestors: vec![AncestorRef {
                    collection: "organizations".to_string(),
                    identifier: "acme".to_string(),
                }],
                collection: "widgets".to_string(),
                identifier: "w1".to_string(),
            }
        );
    }

    #[test]
    fn resource_path_list_depth_2() {
        assert_eq!(
            parse_resource_path("organizations/acme/widgets/w1/sub-things").unwrap(),
            ResourcePath::List {
                ancestors: vec![
                    AncestorRef {
                        collection: "organizations".to_string(),
                        identifier: "acme".to_string(),
                    },
                    AncestorRef {
                        collection: "widgets".to_string(),
                        identifier: "w1".to_string(),
                    },
                ],
                collection: "sub-things".to_string(),
            }
        );
    }

    #[test]
    fn resource_path_item_depth_2() {
        assert_eq!(
            parse_resource_path("organizations/acme/widgets/w1/sub-things/s1").unwrap(),
            ResourcePath::Item {
                ancestors: vec![
                    AncestorRef {
                        collection: "organizations".to_string(),
                        identifier: "acme".to_string(),
                    },
                    AncestorRef {
                        collection: "widgets".to_string(),
                        identifier: "w1".to_string(),
                    },
                ],
                collection: "sub-things".to_string(),
                identifier: "s1".to_string(),
            }
        );
    }

    #[test]
    fn resource_path_subresource_status_top_level() {
        assert_eq!(
            parse_resource_path("organizations/acme/status").unwrap(),
            ResourcePath::Subresource {
                ancestors: vec![],
                collection: "organizations".to_string(),
                identifier: "acme".to_string(),
                subresource: Subresource::Status,
            }
        );
    }

    #[test]
    fn resource_path_subresource_finalizers_top_level() {
        assert_eq!(
            parse_resource_path("organizations/acme/finalizers").unwrap(),
            ResourcePath::Subresource {
                ancestors: vec![],
                collection: "organizations".to_string(),
                identifier: "acme".to_string(),
                subresource: Subresource::Finalizers,
            }
        );
    }

    #[test]
    fn resource_path_subresource_reparent_top_level() {
        assert_eq!(
            parse_resource_path("organizations/acme/reparent").unwrap(),
            ResourcePath::Subresource {
                ancestors: vec![],
                collection: "organizations".to_string(),
                identifier: "acme".to_string(),
                subresource: Subresource::Reparent,
            }
        );
    }

    #[test]
    fn resource_path_subresource_status_depth_1() {
        assert_eq!(
            parse_resource_path("organizations/acme/widgets/w1/status").unwrap(),
            ResourcePath::Subresource {
                ancestors: vec![AncestorRef {
                    collection: "organizations".to_string(),
                    identifier: "acme".to_string(),
                }],
                collection: "widgets".to_string(),
                identifier: "w1".to_string(),
                subresource: Subresource::Status,
            }
        );
    }

    #[test]
    fn resource_path_subresource_finalizers_depth_1() {
        assert_eq!(
            parse_resource_path("organizations/acme/widgets/w1/finalizers").unwrap(),
            ResourcePath::Subresource {
                ancestors: vec![AncestorRef {
                    collection: "organizations".to_string(),
                    identifier: "acme".to_string(),
                }],
                collection: "widgets".to_string(),
                identifier: "w1".to_string(),
                subresource: Subresource::Finalizers,
            }
        );
    }

    #[test]
    fn resource_path_subresource_reparent_depth_1() {
        assert_eq!(
            parse_resource_path("organizations/acme/widgets/w1/reparent").unwrap(),
            ResourcePath::Subresource {
                ancestors: vec![AncestorRef {
                    collection: "organizations".to_string(),
                    identifier: "acme".to_string(),
                }],
                collection: "widgets".to_string(),
                identifier: "w1".to_string(),
                subresource: Subresource::Reparent,
            }
        );
    }

    #[test]
    fn resource_path_subresource_status_depth_2() {
        assert_eq!(
            parse_resource_path("organizations/acme/widgets/w1/sub-things/s1/status").unwrap(),
            ResourcePath::Subresource {
                ancestors: vec![
                    AncestorRef {
                        collection: "organizations".to_string(),
                        identifier: "acme".to_string(),
                    },
                    AncestorRef {
                        collection: "widgets".to_string(),
                        identifier: "w1".to_string(),
                    },
                ],
                collection: "sub-things".to_string(),
                identifier: "s1".to_string(),
                subresource: Subresource::Status,
            }
        );
    }

    #[test]
    fn resource_path_subresource_finalizers_depth_2() {
        assert_eq!(
            parse_resource_path("organizations/acme/widgets/w1/sub-things/s1/finalizers").unwrap(),
            ResourcePath::Subresource {
                ancestors: vec![
                    AncestorRef {
                        collection: "organizations".to_string(),
                        identifier: "acme".to_string(),
                    },
                    AncestorRef {
                        collection: "widgets".to_string(),
                        identifier: "w1".to_string(),
                    },
                ],
                collection: "sub-things".to_string(),
                identifier: "s1".to_string(),
                subresource: Subresource::Finalizers,
            }
        );
    }

    #[test]
    fn resource_path_subresource_reparent_depth_2() {
        assert_eq!(
            parse_resource_path("organizations/acme/widgets/w1/sub-things/s1/reparent").unwrap(),
            ResourcePath::Subresource {
                ancestors: vec![
                    AncestorRef {
                        collection: "organizations".to_string(),
                        identifier: "acme".to_string(),
                    },
                    AncestorRef {
                        collection: "widgets".to_string(),
                        identifier: "w1".to_string(),
                    },
                ],
                collection: "sub-things".to_string(),
                identifier: "s1".to_string(),
                subresource: Subresource::Reparent,
            }
        );
    }

    #[test]
    fn resource_path_orphans_at_non_first_position_is_error() {
        let err = parse_resource_path("things/id/orphans").unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
        assert!(err.message.contains("orphans"));
    }

    #[test]
    fn resource_path_trailing_segment_after_subresource_is_error() {
        for path in &[
            "things/id/reparent/extra",
            "things/id/status/extra",
            "things/id/finalizers/extra",
        ] {
            let err = parse_resource_path(path).unwrap_err();
            assert_eq!(
                err.status,
                axum::http::StatusCode::BAD_REQUEST,
                "expected 400 for path '{path}'"
            );
            assert!(
                err.message.contains("unexpected"),
                "expected 'unexpected' in error for path '{path}', got: {}",
                err.message
            );
        }
    }
}
