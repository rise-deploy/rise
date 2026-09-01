//! Shared test fixtures for the runtime-agnostic reconcile machinery.
//!
//! Public (rather than `#[cfg(test)]`) so every backend crate's tests build the
//! same canonical desired/actual containers instead of drifting copies.

//! Shared `#[cfg(test)]` fixtures for the reconciler's pure-helper unit tests.
//!
//! These builders construct the `DesiredContainer` / `ActualContainer` slots and
//! the protected-id / health maps used across the `diff`, `rolling`, and
//! `pod_status` test modules so each module's tests can `use super::test_helpers`
//! rather than duplicating the fixtures.

use std::collections::{HashMap, HashSet};

use super::desired::{DesiredContainer, DesiredRoute};
use super::diff::ActualContainer;
use super::naming;

pub fn desired(container: &str, image: &str, hash: &str) -> DesiredContainer {
    DesiredContainer {
        project: "myapp".to_string(),
        project_uuid: "22222222-2222-2222-2222-222222222222".to_string(),
        access_class: "public".to_string(),
        deployment_group: "default".to_string(),
        deployment_id: "20260101-120000".to_string(),
        deployment_uuid: "11111111-1111-1111-1111-111111111111".to_string(),
        container: container.to_string(),
        environment: None,
        image: image.to_string(),
        port: Some(8080),
        cpu: "500m".to_string(),
        memory: "256Mi".to_string(),
        env: vec![],
        env_hash: hash.to_string(),
        routes: vec![DesiredRoute {
            hosts: vec!["myapp.rise.dev".to_string()],
            path_prefix: None,
            access: None,
        }],
        routable: true,
        // Fixed sentinel route-hash for diff tests; the reconciler computes
        // the real value via `route_hash_for`. Tests that exercise routing
        // drift override this and the matching actual label.
        route_hash: "rh-active".to_string(),
        // Seed generation (the diff resolves the real value before apply).
        generation: 1,
        replica: 0,
        health_path: Some("/".to_string()),
        health_check_interval_secs: None,
        health_check_timeout_secs: None,
    }
}

/// Stable identity key for the `desired()` helper's slot, used to build the
/// `identity` field of expected Create/Recreate actions.
pub fn identity_of(d: &DesiredContainer) -> String {
    super::diff::identity_key(
        &d.project,
        &d.deployment_group,
        &d.deployment_id,
        &d.container,
        d.replica,
    )
}

/// Empty protected-deployment-ids set for the common case where no
/// deployment failed desired computation.
pub fn no_protected() -> HashSet<String> {
    HashSet::new()
}

/// The resolved generation-ful name for the `desired()` slot at a given
/// generation (the name a Create/Recreate action carries).
pub fn name_of_gen(d: &DesiredContainer, generation: u32) -> String {
    naming::container_name(
        "rise",
        &d.project,
        &d.deployment_group,
        &d.deployment_id,
        &d.container,
        d.replica,
        generation,
    )
}

/// A live container belonging to the `desired()` helper's deployment, at a
/// given generation. Its name carries the `_g{generation}` suffix and it
/// carries the full identity-label set so the diff can match it.
pub fn actual_for_gen(
    d: &DesiredContainer,
    image: &str,
    env_hash: &str,
    generation: u32,
) -> ActualContainer {
    ActualContainer {
        id: "cid".to_string(),
        name: name_of_gen(d, generation),
        project: Some(d.project.clone()),
        deployment_group: Some(d.deployment_group.clone()),
        container: Some(d.container.clone()),
        deployment_id_label: Some(d.deployment_id.clone()),
        deployment_uuid_label: Some(d.deployment_uuid.clone()),
        generation,
        replica: d.replica,
        image_label: Some(image.to_string()),
        env_hash_label: Some(env_hash.to_string()),
        route_hash_label: Some("rh-active".to_string()),
        state: Some("running".to_string()),
    }
}

/// A live container at generation 1 (the common case for most diff tests).
pub fn actual_for(d: &DesiredContainer, image: &str, env_hash: &str) -> ActualContainer {
    actual_for_gen(d, image, env_hash, 1)
}

/// Build the `desired()` slot for a specific replica index.
pub fn desired_replica(replica: u32) -> DesiredContainer {
    let mut d = desired("app", "img:1", "h1");
    d.replica = replica;
    d
}

/// A live (matched) container for the given replica/state/image. Carries the
/// full identity-label set including the replica so the diff matches it.
pub fn actual_replica(replica: u32, state: &str, image: &str) -> ActualContainer {
    let d = desired_replica(replica);
    ActualContainer {
        id: format!("cid-r{replica}"),
        state: Some(state.to_string()),
        ..actual_for(&d, image, "h1")
    }
}

/// Health map marking every given identity healthy.
pub fn all_healthy(actual: &[ActualContainer]) -> HashMap<String, bool> {
    actual
        .iter()
        .filter_map(|a| a.identity().map(|id| (id, true)))
        .collect()
}

/// A reversible stand-in for a real encryption provider.
///
/// Backend tests that exercise secret env-var resolution care that ciphertext
/// round-trips, not which cipher produced it. This keeps those tests free of a
/// crypto dependency and of any one provider's key handling.
pub struct ReversibleEncryptionProvider;

#[async_trait::async_trait]
impl crate::EncryptionProvider for ReversibleEncryptionProvider {
    async fn encrypt(&self, plaintext: &str) -> anyhow::Result<String> {
        Ok(format!("enc:{plaintext}"))
    }

    async fn decrypt(&self, ciphertext: &str) -> anyhow::Result<String> {
        ciphertext
            .strip_prefix("enc:")
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("not produced by this provider: {ciphertext}"))
    }
}
