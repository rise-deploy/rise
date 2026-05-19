//! URL path identifier parsing for the generic resource API.
//!
//! Path segments follow the `<kind>/<identifier>` grammar. The identifier is
//! either a name or a UID-prefixed token (`uid:<uuid>`). This module turns the
//! raw path component into a `PathSegment` the store can resolve.

use rise_resource_store::PathSegment;
use uuid::Uuid;

use crate::server::error::ServerError;

const UID_PREFIX: &str = "uid:";

/// Parse a URL path identifier into a `PathSegment`.
///
/// A bare value is treated as a name; a `uid:<uuid>` prefix selects a UID.
/// Malformed UIDs return 400 — the caller does not need to distinguish.
pub fn parse_identifier(kind: &str, raw: &str) -> Result<PathSegment, ServerError> {
    if let Some(rest) = raw.strip_prefix(UID_PREFIX) {
        let uid: Uuid = rest.parse().map_err(|_| {
            ServerError::bad_request(format!("invalid uid token '{raw}': expected uid:<uuid>"))
        })?;
        Ok(PathSegment::Uid {
            kind: kind.to_string(),
            uid,
        })
    } else {
        Ok(PathSegment::Name {
            kind: kind.to_string(),
            name: raw.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_name() {
        let seg = parse_identifier("Organization", "acme").unwrap();
        match seg {
            PathSegment::Name { kind, name } => {
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
        let seg = parse_identifier("Organization", &raw).unwrap();
        match seg {
            PathSegment::Uid { kind, uid: got } => {
                assert_eq!(kind, "Organization");
                assert_eq!(got, uid);
            }
            other => panic!("expected Uid, got {other:?}"),
        }
    }

    #[test]
    fn rejects_malformed_uid_token() {
        let err = parse_identifier("Organization", "uid:not-a-uuid").unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
        assert!(err.message.contains("invalid uid token"));
    }

    #[test]
    fn does_not_treat_uid_substring_as_uid_token() {
        // Only the literal `uid:` prefix triggers the UID branch; `myuid:x` is a name.
        let seg = parse_identifier("Widget", "myuid:abcd").unwrap();
        assert!(matches!(seg, PathSegment::Name { .. }));
    }
}
