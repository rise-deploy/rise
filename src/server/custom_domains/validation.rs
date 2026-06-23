//! Custom-domain conflict validation.
//!
//! The validator is backend-agnostic and lives in `rise-backend-core` so the
//! `DeploymentUrlBuilder` can share it; re-exported here for the HTTP handlers.

pub use rise_backend_core::validate_custom_domain;
