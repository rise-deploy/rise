//! Authorization policy evaluation and store-backed engine orchestration.
//!
//! The [`policy`] module is a hard Tier-0 boundary: it contains pure functions
//! and canonical values only. It must not acquire store, database, HTTP, or
//! product-resource dependencies.
//!
//! [`engine`] is Tier 1: it runs ADR-0001 §4's algorithm over facts read
//! through the `ResourceStore` contract and one `MembershipResolver` seam. It
//! stays free of HTTP, SQL, and product resource kinds.

pub mod engine;
pub mod policy;
