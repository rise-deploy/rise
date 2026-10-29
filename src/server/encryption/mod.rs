pub mod handlers;
pub mod migration;
pub mod providers;
pub mod routes;

/// Shared encryption contracts for backend-independent credential consumers.
pub use rise_backend_core::{EncryptionContext, EncryptionProvider};
