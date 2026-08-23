//! Startup seeding of ADR-0001's shipped baseline policy.
//!
//! Five root resources, in dependency order — the Roles before the bindings that
//! reference them, because a binding's `roleRef` is resolved at write time:
//!
//! | Resource | Mutability |
//! |---|---|
//! | `PlatformRole/system-admin` | seeded, immutable, healed if missing |
//! | `PlatformRoleBinding/system-admin` → `system:operators` | seeded, immutable, healed if missing |
//! | `PlatformRole/org-admin` | shipped default, operator-editable |
//! | `PlatformRole/resource-owner` | shipped default, operator-editable |
//! | `PlatformRoleBinding/resource-owner` → `${ref.subject}` | shipped default, operator-editable |
//!
//! The distinction is the whole point. The first two are what ADR-0001 §5 fixes:
//! they are not the *source* of operator authority — the evaluator hardcodes that
//! so it survives a bad restore — they are the inspectable record of it, which is
//! why the store refuses to edit or delete them. The other three are ordinary
//! operator-owned data with an opinionated default, so seeding only ever fills a
//! gap and never overwrites a deliberate edit.
//!
//! Seeding is idempotent, and safe to run on every startup: a present row of the
//! right identity is left exactly as it is.

use anyhow::{anyhow, bail, Context, Result};
use rise_resource_api::{org_admin_role_spec, StoreError};
use rise_resource_api::{
    resource_owner_binding_spec, resource_owner_role_spec, system_admin_binding_spec,
    system_admin_role_spec, CreateResourceParams, PlatformRoleBindingSpec, ResourceApi,
    ResourceRow, RoleSpec, API_VERSION_V1ALPHA1, ORG_ADMIN_PLATFORM_ROLE,
    PLATFORM_ROLE_BINDING_KIND, PLATFORM_ROLE_KIND, RESOURCE_OWNER_PLATFORM_ROLE,
    SYSTEM_ADMIN_PLATFORM_ROLE,
};
use tracing::info;

/// How a seed behaves when a row of the same name already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reseed {
    /// Fixed platform data. The store rejects edits and deletes, so a divergent
    /// row means something wrote to the database directly — which is exactly the
    /// case the ADR's immutability is there to make visible.
    Immutable,
    /// A shipped default over operator-owned data: fill the gap, never overwrite.
    KeepExisting,
}

struct Seed {
    kind: &'static str,
    name: &'static str,
    reseed: Reseed,
    spec: serde_json::Value,
}

/// Seed the baseline policy. Idempotent and safe on every startup.
///
/// Returns the number of rows created, which is zero on every boot after the
/// first unless an operator has deleted an editable default.
pub async fn run(store: &dyn ResourceApi) -> Result<u64> {
    let mut created = 0;
    for seed in seeds()? {
        if apply(store, &seed).await? {
            created += 1;
            info!(
                kind = seed.kind,
                name = seed.name,
                "Seeded baseline authorization policy"
            );
        }
    }
    Ok(created)
}

/// The shipped seeds, in the order they must be written.
///
/// A binding's `roleRef` is resolved against live rows inside the write
/// transaction, so each Role precedes every binding naming it. The specs come
/// from `rise-resource-api`, which is also what admission compares the immutable
/// pair against — one definition, so the seeder cannot write a row its own store
/// would reject.
fn seeds() -> Result<Vec<Seed>> {
    Ok(vec![
        role_seed(
            SYSTEM_ADMIN_PLATFORM_ROLE,
            Reseed::Immutable,
            system_admin_role_spec(),
        )?,
        role_seed(
            ORG_ADMIN_PLATFORM_ROLE,
            Reseed::KeepExisting,
            org_admin_role_spec(),
        )?,
        role_seed(
            RESOURCE_OWNER_PLATFORM_ROLE,
            Reseed::KeepExisting,
            resource_owner_role_spec(),
        )?,
        binding_seed(
            SYSTEM_ADMIN_PLATFORM_ROLE,
            Reseed::Immutable,
            system_admin_binding_spec(),
        )?,
        binding_seed(
            RESOURCE_OWNER_PLATFORM_ROLE,
            Reseed::KeepExisting,
            resource_owner_binding_spec(),
        )?,
    ])
}

fn role_seed(name: &'static str, reseed: Reseed, spec: RoleSpec) -> Result<Seed> {
    Ok(Seed {
        kind: PLATFORM_ROLE_KIND,
        name,
        reseed,
        spec: serde_json::to_value(spec)
            .with_context(|| format!("Failed to serialize seeded {PLATFORM_ROLE_KIND} '{name}'"))?,
    })
}

fn binding_seed(name: &'static str, reseed: Reseed, spec: PlatformRoleBindingSpec) -> Result<Seed> {
    Ok(Seed {
        kind: PLATFORM_ROLE_BINDING_KIND,
        name,
        reseed,
        spec: serde_json::to_value(spec).with_context(|| {
            format!("Failed to serialize seeded {PLATFORM_ROLE_BINDING_KIND} '{name}'")
        })?,
    })
}

/// Apply one seed. Returns whether a row was created.
async fn apply(store: &dyn ResourceApi, seed: &Seed) -> Result<bool> {
    if let Some(existing) = find(store, seed).await? {
        verify(seed, &existing)?;
        return Ok(false);
    }

    match store
        .create(CreateResourceParams {
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: seed.kind.to_string(),
            name: seed.name.to_string(),
            parent_uid: None,
            spec: seed.spec.clone(),
            ..Default::default()
        })
        .await
    {
        Ok(_) => Ok(true),
        // Replicas boot concurrently and this write takes no install-wide lock,
        // so losing the race is the expected outcome, not an error: the winner
        // wrote the identical row. Re-reading keeps the divergence check honest.
        Err(StoreError::NameConflict) => {
            let existing = find(store, seed).await?.ok_or_else(|| {
                anyhow!(
                    "{} '{}' reported a conflict but is not present",
                    seed.kind,
                    seed.name
                )
            })?;
            verify(seed, &existing)?;
            Ok(false)
        }
        Err(error) => Err(anyhow!(
            "Failed to seed {} '{}': {error}",
            seed.kind,
            seed.name
        )),
    }
}

async fn find(store: &dyn ResourceApi, seed: &Seed) -> Result<Option<ResourceRow>> {
    store
        .get_by_name(API_VERSION_V1ALPHA1, seed.kind, seed.name, None)
        .await
        .map_err(|error| anyhow!("Failed to look up {} '{}': {error}", seed.kind, seed.name))
}

/// Check an already-present seed against what was shipped.
///
/// Only the immutable pair is checked, and startup fails rather than repairing
/// them. Repair would need a privileged write path around the very admission
/// rules that keep these rows fixed, and the only way to reach a divergent state
/// is a direct database write — so an actionable error is more honest than
/// quietly overwriting whatever an operator did out of band. A tombstoned row
/// counts as divergent for the same reason: the store refuses to delete these,
/// so a deletion timestamp cannot have come from the API either.
fn verify(seed: &Seed, existing: &ResourceRow) -> Result<()> {
    if seed.reseed == Reseed::KeepExisting {
        return Ok(());
    }
    let normalized = normalized_seed_spec(seed)?;
    if existing.deletion_timestamp.is_none() && existing.spec == normalized {
        return Ok(());
    }
    bail!(
        "{} '{}' is seeded and immutable but the stored row does not match the shipped \
         definition (stored spec: {}, deletion_timestamp: {:?}). The resource API cannot \
         produce this state, so it was written to the database directly. Remove the row \
         from resource_store.resources and restart to let bootstrap re-seed it.",
        seed.kind,
        seed.name,
        existing.spec,
        existing.deletion_timestamp
    )
}

/// The spec the store persists for a seed.
///
/// A binding's stored spec has its contextual defaults applied, so comparing the
/// authored form against the row would report a false divergence on every boot.
fn normalized_seed_spec(seed: &Seed) -> Result<serde_json::Value> {
    if seed.kind != PLATFORM_ROLE_BINDING_KIND {
        return Ok(seed.spec.clone());
    }
    let authored: PlatformRoleBindingSpec = serde_json::from_value(seed.spec.clone())
        .with_context(|| format!("Seeded {} '{}' does not parse", seed.kind, seed.name))?;
    let normalized = authored.normalize().map_err(|error| {
        anyhow!(
            "Seeded {} '{}' does not normalize: {error}",
            seed.kind,
            seed.name
        )
    })?;
    serde_json::to_value(normalized).with_context(|| {
        format!(
            "Failed to serialize normalized {} '{}'",
            seed.kind, seed.name
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_write_roles_before_the_bindings_that_reference_them() {
        let seeds = seeds().expect("seeds build");
        let first_binding = seeds
            .iter()
            .position(|seed| seed.kind == PLATFORM_ROLE_BINDING_KIND)
            .expect("a binding is seeded");
        assert!(
            seeds[..first_binding]
                .iter()
                .all(|seed| seed.kind == PLATFORM_ROLE_KIND),
            "every Role must precede the first binding, or roleRef resolution fails"
        );
        let names: Vec<&str> = seeds.iter().map(|seed| seed.name).collect();
        assert!(names.contains(&SYSTEM_ADMIN_PLATFORM_ROLE));
        assert!(names.contains(&ORG_ADMIN_PLATFORM_ROLE));
        assert!(names.contains(&RESOURCE_OWNER_PLATFORM_ROLE));
    }

    /// Every binding seed must name a Role this same pass creates. A seed
    /// referencing a Role nothing seeds would fail write-time resolution on a
    /// fresh install.
    #[test]
    fn every_seeded_binding_references_a_seeded_role() {
        let seeds = seeds().expect("seeds build");
        let roles: Vec<&str> = seeds
            .iter()
            .filter(|seed| seed.kind == PLATFORM_ROLE_KIND)
            .map(|seed| seed.name)
            .collect();
        for seed in seeds
            .iter()
            .filter(|seed| seed.kind == PLATFORM_ROLE_BINDING_KIND)
        {
            let spec: PlatformRoleBindingSpec =
                serde_json::from_value(seed.spec.clone()).expect("binding seed parses");
            assert!(
                roles.contains(&spec.role_ref.name.as_str()),
                "{} '{}' references unseeded PlatformRole '{}'",
                seed.kind,
                seed.name,
                spec.role_ref.name
            );
        }
    }

    /// `resource-owner` deliberately excludes `create` and every subresource
    /// (ADR-0001 §6.2): ownership alone never grants child creation or status
    /// writes. A widened default would silently hand both to every owner.
    #[test]
    fn resource_owner_grants_no_create_and_no_subresource() {
        let spec = resource_owner_role_spec();
        assert_eq!(spec.statements.len(), 1);
        let statement = &spec.statements[0];
        assert!(
            statement.subresources.is_none(),
            "ownership must not reach subresources"
        );
        let rise_resource_api::VerbMatcher::Verbs(verbs) = &statement.verbs else {
            panic!("resource-owner must enumerate its verbs rather than use a wildcard");
        };
        assert!(!verbs.contains(&rise_resource_api::Verb::Create));
        assert!(!verbs.contains(&rise_resource_api::Verb::Use));
    }

    /// A present editable default is left untouched, so an operator's edit
    /// survives every restart.
    #[test]
    fn verify_leaves_editable_defaults_alone() {
        let seed = role_seed(
            ORG_ADMIN_PLATFORM_ROLE,
            Reseed::KeepExisting,
            org_admin_role_spec(),
        )
        .expect("seed builds");
        let existing = row(serde_json::json!({ "statements": [] }), None);
        assert!(verify(&seed, &existing).is_ok());
    }

    #[test]
    fn verify_rejects_a_divergent_immutable_seed() {
        let seed = role_seed(
            SYSTEM_ADMIN_PLATFORM_ROLE,
            Reseed::Immutable,
            system_admin_role_spec(),
        )
        .expect("seed builds");
        assert!(verify(&seed, &row(seed.spec.clone(), None)).is_ok());

        let error = verify(&seed, &row(serde_json::json!({ "statements": [] }), None))
            .expect_err("a rewritten body must fail startup");
        let message = format!("{error:#}");
        assert!(
            message.contains("written to the database directly"),
            "{message}"
        );
        assert!(message.contains("restart"), "{message}");
    }

    #[test]
    fn verify_rejects_a_tombstoned_immutable_seed() {
        let seed = role_seed(
            SYSTEM_ADMIN_PLATFORM_ROLE,
            Reseed::Immutable,
            system_admin_role_spec(),
        )
        .expect("seed builds");
        let tombstoned = row(seed.spec.clone(), Some(chrono::Utc::now()));
        assert!(verify(&seed, &tombstoned).is_err());
    }

    /// The immutable binding's stored form is its *normalized* one, so the
    /// divergence check has to normalize before comparing or it would fail on
    /// every boot.
    #[test]
    fn verify_compares_the_binding_seed_in_its_normalized_form() {
        let seed = binding_seed(
            SYSTEM_ADMIN_PLATFORM_ROLE,
            Reseed::Immutable,
            system_admin_binding_spec(),
        )
        .expect("seed builds");
        let stored = normalized_seed_spec(&seed).expect("normalizes");
        assert!(verify(&seed, &row(stored, None)).is_ok());
    }

    fn row(
        spec: serde_json::Value,
        deletion_timestamp: Option<chrono::DateTime<chrono::Utc>>,
    ) -> ResourceRow {
        let now = chrono::Utc::now();
        ResourceRow {
            uid: uuid::Uuid::new_v4(),
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: PLATFORM_ROLE_KIND.to_string(),
            parent_uid: None,
            name: SYSTEM_ADMIN_PLATFORM_ROLE.to_string(),
            discriminator: String::new(),
            labels: Default::default(),
            metadata: serde_json::json!({}),
            spec,
            status: serde_json::json!({}),
            revision: 1,
            finalizers: Vec::new(),
            owner_references: Vec::new(),
            deletion_timestamp,
            created_at: now,
            updated_at: now,
        }
    }
}
