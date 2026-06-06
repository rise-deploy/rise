//! Kubernetes resource quantity parsing and validation utilities.
//!
//! The implementation lives in `rise-backend-core`; this module re-exports it so
//! existing `crate::server::deployment::quantity::*` references keep working.

pub use rise_backend_core::quantity::*;
