//! Generic resource HTTP API (`/api/v1/resources`).
//!
//! Operator-only CRUD over the resource store, plus controller-authenticated
//! status/finalizer endpoints and a `pending-deletion` diagnostics listing.
//! See `ROADMAP.md` for the roadmap and phase context.

pub mod error_map;
pub mod gc;
pub mod handlers;
pub mod models;
mod organization;
pub mod path;
pub mod routes;
pub mod schemas;
