//! Persistence for the OAuth relay's transient key-value store (`oauth_relay_store`).
//!
//! Functions to be added: `put_if_absent`, `get`, `compare_and_swap`, `delete` (the
//! `oauthrelay_core::TransientStore` operations) and `delete_expired` (batched cleanup).
