//! Authorization policy evaluation and, later, store-backed engine orchestration.
//!
//! The [`policy`] module is a hard Tier-0 boundary: it contains pure functions
//! and canonical values only. It must not acquire store, database, HTTP, or
//! product-resource dependencies when the live engine is added beside it.

pub mod policy;
