//! Generic resource HTTP API (`/api/v1/resources`).
//!
//! CRUD over the resource store, authorized per resource by the ADR-0001 engine
//! (see `crate::server::authz`), plus controller-authenticated status/finalizer
//! endpoints and a `pending-deletion` diagnostics listing.
//! See `ROADMAP.md` for the roadmap and phase context.

pub mod error_map;
pub mod gc;
pub mod handlers;
pub mod models;
mod organization;
pub mod path;
pub mod routes;
pub mod schemas;
