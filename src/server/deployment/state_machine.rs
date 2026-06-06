//! Deployment status state-machine helpers.
//!
//! The implementation lives in `rise-backend-core`; this module re-exports it so
//! existing `crate::server::deployment::state_machine::*` references keep working.

pub use rise_backend_core::state_machine::*;
