//! Generic resource HTTP API (`/api/v1/resources`).
//!
//! Operator-only CRUD over the resource store, plus controller-authenticated
//! status/finalizer endpoints and admin-operator break-glass paths (`orphans`
//! and `reparent`). See `MULTI_TENANCY_PLAN.md` § "Generic API".

pub mod error_map;
pub mod handlers;
pub mod models;
pub mod path;
pub mod routes;
