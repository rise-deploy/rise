pub mod handlers;
pub mod models;
pub mod providers;
pub mod routes;

/// The `RegistryProvider` contract and `ImageTagType` live in
/// `rise-backend-core`; re-exported here so existing
/// `crate::server::registry::{RegistryProvider, ImageTagType}` references keep
/// working.
pub use rise_backend_core::{ImageTagType, RegistryProvider};
