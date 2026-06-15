pub mod handlers;
pub mod providers;
pub mod routes;

/// The `EncryptionProvider` contract lives in `rise-backend-core`; re-exported
/// here so existing `crate::server::encryption::EncryptionProvider` references
/// keep working.
pub use rise_backend_core::EncryptionProvider;
