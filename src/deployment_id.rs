//! Deployment ID generation shared by the CLI (client-controlled push) and
//! the backend (server-allocated path).
//!
//! Format: UUIDv7 — a timestamp-ordered UUID with 74 bits of randomness and
//! 48 bits of Unix-ms timestamp. Renders as the standard hyphenated form
//! (`018f3a4b-c2d8-7abc-9def-0123456789ab`, 36 chars).
//!
//! Properties we rely on:
//!
//! - **Time-ordered**: lexicographic sort matches creation order across the
//!   whole system (DB, registry tag listings, UI).
//! - **Unguessable** (74 random bits): an attacker can't enumerate another
//!   project's deploys by guessing. Primary access control still lives in
//!   `RegistryProvider::validate_image_for_project` (project membership +
//!   image-name match); the entropy here is defense-in-depth.
//! - **Standard format**: consumers that need the timestamp can use a UUIDv7
//!   parser; nothing in our code currently parses it.
//!
//! Eyeball-readable creation dates are surfaced separately (DB `created_at`,
//! `kubectl get … --show-labels`, rise UI), so the standard UUID format
//! costs nothing operationally.

use uuid::Uuid;

pub fn generate_deployment_id() -> String {
    Uuid::now_v7().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deployment_id_has_expected_shape() {
        let id = generate_deployment_id();
        // Standard hyphenated UUID: 8-4-4-4-12 = 32 hex + 4 hyphens = 36 chars.
        assert_eq!(id.len(), 36, "unexpected length for {id}");
        let parsed = Uuid::parse_str(&id).expect("valid UUID");
        assert_eq!(
            parsed.get_version_num(),
            7,
            "expected UUIDv7, got version {} ({id})",
            parsed.get_version_num()
        );
    }

    #[test]
    fn deployment_id_unique_within_a_burst() {
        // 1024 draws from a 74-bit random space: collision probability ~3e-17.
        // A flake here means the RNG isn't actually random.
        let mut ids = std::collections::HashSet::new();
        for _ in 0..1024 {
            assert!(ids.insert(generate_deployment_id()));
        }
    }

    #[test]
    fn deployment_ids_sort_in_creation_order() {
        // UUIDv7's timestamp prefix means lexicographic sort matches creation
        // order — used by the UI and by registry tag listings.
        let mut ids: Vec<String> = (0..16).map(|_| generate_deployment_id()).collect();
        let original = ids.clone();
        ids.sort();
        assert_eq!(ids, original, "UUIDv7 ids should already be in sort order");
    }
}
