//! Resource discriminators.
//!
//! Every resource carries a system-generated 8-character `discriminator`,
//! unique among all resources that share the same parent (its siblings),
//! regardless of kind — it is **not** unique across different parents or
//! globally. Like `name` it identifies a resource within its sibling scope;
//! unlike `name` (user-chosen, and potentially reconstructable from external
//! inputs) the discriminator is random, so it gives controllers a
//! collision-free token when constructing derived identifiers in external
//! systems while reconciling a resource.

use rand::RngExt;

const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";

pub fn generate() -> String {
    let mut rng = rand::rng();
    (0..8)
        .map(|_| CHARSET[rng.random_range(0..CHARSET.len())] as char)
        .collect()
}
