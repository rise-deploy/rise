pub mod handlers;
pub mod models;
pub mod providers;
pub mod routes;

/// The `RegistryProvider` contract and `ImageTagType` live in
/// `rise-backend-core`; re-exported here so existing
/// `crate::server::registry::{RegistryProvider, ImageTagType}` references keep
/// working.
pub use rise_backend_core::{ImageTagType, RegistryProvider};

/// A registry provider's refusal to mint credentials for a repository the
/// project may not use. Carried inside the provider's `anyhow::Error` and
/// answered as `409 Conflict` by [`credentials_error`].
#[derive(Debug)]
pub struct RepositoryUnavailable(pub String);

impl std::fmt::Display for RepositoryUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RepositoryUnavailable {}

/// Map a `RegistryProvider::get_credentials` failure onto its response: a
/// [`RepositoryUnavailable`] refusal is the caller's `409`, anything else a
/// `500` described by `message`.
pub fn credentials_error(error: anyhow::Error, message: &str) -> crate::server::error::ServerError {
    match error.downcast_ref::<RepositoryUnavailable>() {
        Some(refusal) => crate::server::error::ServerError::conflict(refusal.to_string()),
        None => crate::server::error::ServerError::internal_anyhow(error, message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    #[test]
    fn an_unavailable_repository_is_a_conflict() {
        let refused = credentials_error(
            anyhow::Error::new(RepositoryUnavailable("held by a deleted project".into())),
            "Failed to get registry credentials",
        );
        assert_eq!(refused.status, StatusCode::CONFLICT);
        assert_eq!(refused.message, "held by a deleted project");

        let failed = credentials_error(
            anyhow::anyhow!("boom"),
            "Failed to get registry credentials",
        );
        assert_eq!(failed.status, StatusCode::INTERNAL_SERVER_ERROR);
    }
}
