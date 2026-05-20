//! URL path parsing for the generic resource API.
//!
//! This module provides two parsing functions:
//!
//! * [`parse_identifier`] — turns a single `<identifier>` segment into a
//!   `PathSegment` (name or `uid:<uuid>` form) for the resource store.
//! * [`parse_resource_path`] — parses a full resource URL path like
//!   `apis/rise.dev/v1alpha1/organizations/acme/apis/example.dev/v1/widgets/w1/status` into a typed [`ResourcePath`],
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
            api_versions: vec![api_version.to_string()],
            kind: kind.to_string(),
            uid,
        })
    } else {
        Ok(PathSegment::Name {
            api_versions: vec![api_version.to_string()],
            kind: kind.to_string(),
            name: raw.to_string(),
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CollectionRef {
    pub group: String,
    pub version: String,
    pub plural: String,
}

/// A single ancestor segment in a hierarchical resource path.
#[derive(Debug, PartialEq)]
pub struct AncestorRef {
    pub collection: CollectionRef,
    pub identifier: String,
}

/// The well-known sub-resource suffixes that can appear at the end of a path.
#[derive(Debug, PartialEq)]
pub enum Subresource {
    Status,
    Finalizers,
    Reparent,
}

/// A parsed representation of a hierarchical resource URL path.
#[derive(Debug, PartialEq)]
pub enum ResourcePath {
    /// The literal path `orphans`.
    Orphans,
    /// Parentless resources of a specific non-root-scoped collection:
    /// `<collection>/orphans`.
    TypeOrphanList { collection: CollectionRef },
    /// A single parentless resource: `<collection>/orphans/<identifier>`.
    TypeOrphanItem {
        collection: CollectionRef,
        identifier: String,
    },
    /// A sub-resource operation on a parentless item:
    /// `<collection>/orphans/<identifier>/<sub>`.
    TypeOrphanSubresource {
        collection: CollectionRef,
        identifier: String,
        subresource: Subresource,
    },
    /// A collection, optionally nested under ancestors: `<ancestors…>/<collection>`.
    List {
        ancestors: Vec<AncestorRef>,
        collection: CollectionRef,
    },
    /// A single item: `<ancestors…>/<collection>/<identifier>`.
    Item {
        ancestors: Vec<AncestorRef>,
        collection: CollectionRef,
        identifier: String,
    },
    /// A sub-resource operation on an item: `<ancestors…>/<collection>/<identifier>/<sub>`.
    Subresource {
        ancestors: Vec<AncestorRef>,
        collection: CollectionRef,
        identifier: String,
        subresource: Subresource,
    },
}

/// Parse a URL resource path into a [`ResourcePath`].
///
/// The path is a `/`-separated sequence of segments. Empty paths and empty
/// segments (e.g. a trailing `/`) are rejected with a 400 error.
///
/// # Grammar
///
/// ```text
/// path        ::= "orphans"
///               | "orphans" "/" collection
///               | "orphans" "/" collection "/" identifier
///               | "orphans" "/" collection "/" identifier "/" "reparent"
///               | segment+
/// collection  ::= "apis" "/" group "/" version "/" plural
/// segment     ::= collection "/" identifier
/// tail        ::= collection                        -- List
///               | collection "/" identifier          -- Item
///               | collection "/" identifier "/" sub  -- Subresource
/// sub         ::= "status" | "finalizers" | "reparent"
/// ```
///
/// The keywords `orphans`, `status`, `finalizers`, and `reparent` are reserved.
/// `orphans` is valid only as the first path segment — bare for the global orphan
/// list, or as a prefix for the type-scoped orphan paths — so a resource may still
/// be named `orphans`. `status`, `finalizers`, and `reparent` are valid only as
/// the final segment of an item path.
pub fn parse_resource_path(raw: &str) -> Result<ResourcePath, ServerError> {
    let segments: Vec<&str> = raw.split('/').collect();

    if segments.is_empty() || segments.iter().any(|s| s.is_empty()) {
        return Err(ServerError::bad_request("empty resource path segment"));
    }

    // `orphans` is only ever the first segment — bare for the global orphan
    // listing, or a prefix for the type-scoped orphan paths. It is never an
    // identifier, so a resource may legitimately be named `orphans`.
    if segments[0] == "orphans" {
        return parse_orphan_path(&segments);
    }

    let mut ancestors: Vec<AncestorRef> = Vec::new();
    let mut pos = 0;

    loop {
        let collection = parse_collection(&segments, pos)?;
        pos += 4;

        if pos == segments.len() {
            return Ok(ResourcePath::List {
                ancestors,
                collection,
            });
        }

        let identifier = segments[pos].to_string();
        pos += 1;

        if pos == segments.len() {
            return Ok(ResourcePath::Item {
                ancestors,
                collection,
                identifier,
            });
        }

        match segments[pos] {
            kw @ ("status" | "finalizers" | "reparent") => {
                if pos + 1 != segments.len() {
                    return Err(ServerError::bad_request(format!(
                        "unexpected segments after subresource '{kw}': path must end after the keyword"
                    )));
                }
                let subresource = match kw {
                    "status" => Subresource::Status,
                    "finalizers" => Subresource::Finalizers,
                    _ => Subresource::Reparent,
                };
                return Ok(ResourcePath::Subresource {
                    ancestors,
                    collection,
                    identifier,
                    subresource,
                });
            }
            "apis" => {
                ancestors.push(AncestorRef {
                    collection,
                    identifier,
                });
            }
            "orphans" => {
                return Err(ServerError::bad_request(
                    "'orphans' is a reserved keyword and may only appear as the first path segment",
                ));
            }
            other => {
                return Err(ServerError::bad_request(format!(
                    "expected nested collection to start with 'apis', got '{other}'"
                )));
            }
        }
    }
}

/// Parse an `orphans`-prefixed path. `segments[0]` is already known to be `orphans`.
fn parse_orphan_path(segments: &[&str]) -> Result<ResourcePath, ServerError> {
    // Bare `orphans` — the global listing of resources detached by an
    // in-progress teardown.
    if segments.len() == 1 {
        return Ok(ResourcePath::Orphans);
    }

    // `orphans/<collection>[/<identifier>[/reparent]]`
    let collection = parse_collection(segments, 1)?;
    let mut pos = 5; // "orphans" (1) + collection (4)

    if pos == segments.len() {
        return Ok(ResourcePath::TypeOrphanList { collection });
    }

    let identifier = segments[pos].to_string();
    pos += 1;

    if pos == segments.len() {
        return Ok(ResourcePath::TypeOrphanItem {
            collection,
            identifier,
        });
    }

    match segments[pos] {
        "reparent" => {
            if pos + 1 != segments.len() {
                return Err(ServerError::bad_request(
                    "unexpected segments after orphan reparent: path must end after the keyword",
                ));
            }
            Ok(ResourcePath::TypeOrphanSubresource {
                collection,
                identifier,
                subresource: Subresource::Reparent,
            })
        }
        kw @ ("status" | "finalizers") => Err(ServerError::bad_request(format!(
            "orphan subresource '{kw}' is not supported"
        ))),
        other => Err(ServerError::bad_request(format!(
            "expected orphan subresource 'reparent', got '{other}'"
        ))),
    }
}

fn parse_collection(segments: &[&str], pos: usize) -> Result<CollectionRef, ServerError> {
    if segments.get(pos) != Some(&"apis") {
        return Err(ServerError::bad_request(
            "resource collection paths must start with 'apis/{group}/{version}/{plural}'",
        ));
    }
    let Some(group) = segments.get(pos + 1) else {
        return Err(ServerError::bad_request("missing resource API group"));
    };
    let Some(version) = segments.get(pos + 2) else {
        return Err(ServerError::bad_request("missing resource API version"));
    };
    let Some(plural) = segments.get(pos + 3) else {
        return Err(ServerError::bad_request(
            "missing resource collection plural",
        ));
    };
    Ok(CollectionRef {
        group: (*group).to_string(),
        version: (*version).to_string(),
        plural: (*plural).to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_name() {
        let seg = parse_identifier("rise.dev/v1alpha1", "Organization", "acme").unwrap();
        match seg {
            PathSegment::Name {
                api_versions,
                kind,
                name,
            } => {
                assert_eq!(api_versions, vec!["rise.dev/v1alpha1"]);
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
                api_versions,
                kind,
                uid: got,
            } => {
                assert_eq!(api_versions, vec!["rise.dev/v1alpha1"]);
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

    fn collection(group: &str, version: &str, plural: &str) -> CollectionRef {
        CollectionRef {
            group: group.into(),
            version: version.into(),
            plural: plural.into(),
        }
    }

    #[test]
    fn resource_path_orphans() {
        assert_eq!(
            parse_resource_path("orphans").unwrap(),
            ResourcePath::Orphans
        );
    }

    #[test]
    fn resource_path_type_orphan_list() {
        assert_eq!(
            parse_resource_path("orphans/apis/example.dev/v1/widgets").unwrap(),
            ResourcePath::TypeOrphanList {
                collection: collection("example.dev", "v1", "widgets"),
            }
        );
    }

    #[test]
    fn resource_path_type_orphan_item() {
        assert_eq!(
            parse_resource_path("orphans/apis/example.dev/v1/widgets/w1").unwrap(),
            ResourcePath::TypeOrphanItem {
                collection: collection("example.dev", "v1", "widgets"),
                identifier: "w1".into(),
            }
        );
    }

    #[test]
    fn resource_path_type_orphan_reparent() {
        assert_eq!(
            parse_resource_path("orphans/apis/example.dev/v1/widgets/w1/reparent").unwrap(),
            ResourcePath::TypeOrphanSubresource {
                collection: collection("example.dev", "v1", "widgets"),
                identifier: "w1".into(),
                subresource: Subresource::Reparent,
            }
        );
    }

    #[test]
    fn resource_path_item_named_orphans_is_addressable() {
        // `orphans` is only special as the first segment, so a resource may be
        // named `orphans` and is still reachable as a normal item.
        assert_eq!(
            parse_resource_path("apis/example.dev/v1/widgets/orphans").unwrap(),
            ResourcePath::Item {
                ancestors: vec![],
                collection: collection("example.dev", "v1", "widgets"),
                identifier: "orphans".into(),
            }
        );
    }

    #[test]
    fn resource_path_orphans_rejected_after_identifier() {
        let err =
            parse_resource_path("apis/rise.dev/v1alpha1/organizations/acme/orphans").unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
        assert!(err.message.contains("orphans"));
    }

    #[test]
    fn resource_path_list_top_level() {
        assert_eq!(
            parse_resource_path("apis/rise.dev/v1alpha1/organizations").unwrap(),
            ResourcePath::List {
                ancestors: vec![],
                collection: collection("rise.dev", "v1alpha1", "organizations"),
            }
        );
    }

    #[test]
    fn resource_path_item_top_level() {
        assert_eq!(
            parse_resource_path("apis/rise.dev/v1alpha1/organizations/acme").unwrap(),
            ResourcePath::Item {
                ancestors: vec![],
                collection: collection("rise.dev", "v1alpha1", "organizations"),
                identifier: "acme".into(),
            }
        );
    }

    #[test]
    fn resource_path_list_depth_1() {
        assert_eq!(
            parse_resource_path(
                "apis/rise.dev/v1alpha1/organizations/acme/apis/example.dev/v1/widgets"
            )
            .unwrap(),
            ResourcePath::List {
                ancestors: vec![AncestorRef {
                    collection: collection("rise.dev", "v1alpha1", "organizations"),
                    identifier: "acme".into(),
                }],
                collection: collection("example.dev", "v1", "widgets"),
            }
        );
    }

    #[test]
    fn resource_path_subresource_depth_1() {
        assert_eq!(
            parse_resource_path(
                "apis/rise.dev/v1alpha1/organizations/acme/apis/example.dev/v1/widgets/w1/status"
            )
            .unwrap(),
            ResourcePath::Subresource {
                ancestors: vec![AncestorRef {
                    collection: collection("rise.dev", "v1alpha1", "organizations"),
                    identifier: "acme".into(),
                }],
                collection: collection("example.dev", "v1", "widgets"),
                identifier: "w1".into(),
                subresource: Subresource::Status,
            }
        );
    }

    #[test]
    fn resource_path_rejects_unversioned_collection() {
        let err = parse_resource_path("organizations/acme").unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
    }
}
