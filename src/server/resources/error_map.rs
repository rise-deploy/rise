//! Convert `rise_resource_api::StoreError` into the server's `ServerError`.
//!
//! Kept out of the store crate so the store stays free of HTTP-specific
//! dependencies. All HTTP handler code converts via this module.

use rise_resource_api::StoreError;

use crate::server::error::ServerError;

pub fn store_error_to_server_error(err: StoreError) -> ServerError {
    match err {
        StoreError::NotFound => ServerError::not_found("resource not found"),
        StoreError::RevisionConflict { expected, found } => ServerError::conflict(format!(
            "revision conflict: expected {expected}, found {found}"
        )),
        StoreError::NameConflict => {
            ServerError::conflict("a resource with this name already exists in this scope")
        }
        StoreError::Conflict(msg) => ServerError::conflict(msg),
        StoreError::DiscriminatorExhausted => ServerError::service_unavailable(
            "could not generate a unique discriminator; please retry",
        ),
        StoreError::KindMismatch { expected, got } => ServerError::not_found(format!(
            "path segment kind mismatch: expected '{expected}', got '{got}'"
        )),
        StoreError::ParentNotFound => ServerError::not_found("parent path segment not found"),
        // An unserved/undefined apiVersion is not addressable — 404, matching
        // Kubernetes' behaviour for an unserved version.
        StoreError::UnknownVersion(msg) => ServerError::not_found(msg),
        StoreError::ReservedFinalizer(f) => ServerError::bad_request(format!(
            "finalizer '{f}' is in the reserved system.rise.dev/* namespace"
        )),
        StoreError::EmptyPath => {
            ServerError::bad_request("path resolution requires at least one segment")
        }
        StoreError::Validation(msg) => ServerError::bad_request(msg),
        // The write path's retry loop replays this; it only reaches a client if
        // every attempt lost the race, which is a genuine "try again".
        StoreError::Serialization => ServerError::service_unavailable(
            "the write lost a concurrent-update race; please retry",
        )
        .retryable()
        .expected(),
        StoreError::Backend { source } => ServerError::internal_anyhow(
            anyhow::Error::from_boxed(source),
            "resource store backend error",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    #[test]
    fn maps_not_found() {
        let err = store_error_to_server_error(StoreError::NotFound);
        assert_eq!(err.status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn maps_revision_conflict() {
        let err = store_error_to_server_error(StoreError::RevisionConflict {
            expected: 7,
            found: 8,
        });
        assert_eq!(err.status, StatusCode::CONFLICT);
        assert!(err.message.contains("expected 7"));
        assert!(err.message.contains("found 8"));
    }

    #[test]
    fn maps_name_conflict() {
        let err = store_error_to_server_error(StoreError::NameConflict);
        assert_eq!(err.status, StatusCode::CONFLICT);
    }

    #[test]
    fn maps_conflict() {
        let err = store_error_to_server_error(StoreError::Conflict(
            "a live UserIdentity with this issuer and subject already exists".into(),
        ));
        assert_eq!(err.status, StatusCode::CONFLICT);
        assert!(err.message.contains("UserIdentity"));
    }

    #[test]
    fn maps_discriminator_exhausted_to_503() {
        let err = store_error_to_server_error(StoreError::DiscriminatorExhausted);
        assert_eq!(err.status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn maps_validation() {
        let err = store_error_to_server_error(StoreError::Validation("bad spec".into()));
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(err.message, "bad spec");
    }

    #[test]
    fn maps_unknown_version_to_not_found() {
        let err = store_error_to_server_error(StoreError::UnknownVersion(
            "version 'v3' is not served".into(),
        ));
        assert_eq!(err.status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn maps_kind_mismatch_to_not_found() {
        let err = store_error_to_server_error(StoreError::KindMismatch {
            expected: "Organization".into(),
            got: "Widget".into(),
        });
        assert_eq!(err.status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn maps_opaque_backend_errors_to_internal_server_error() {
        let err = store_error_to_server_error(StoreError::backend(std::io::Error::other(
            "database unavailable",
        )));
        assert_eq!(err.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(err.message, "resource store backend error");
    }
}
