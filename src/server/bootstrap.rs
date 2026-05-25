//! Default-Organization bootstrap.
//!
//! Runs after both the root migrations (`./migrations/`) and the
//! `rise-resource-store` migrations have applied. The bootstrap pass:
//!
//! 1. Acquires a single Postgres advisory lock (so concurrent replicas don't
//!    race), the only mutator of the default-Organization linkage state.
//! 2. Upserts the configured default Organization in the generic resource
//!    store, preserving any unrelated annotations and recording the
//!    `kubernetes.rise.dev/namespace-prefix` annotation plus the
//!    `spec.deploymentControllerClass`.
//! 3. Idempotently backfills `organization_resource_uid` on existing teams
//!    and projects, and `user_organization_memberships` for every existing
//!    user.
//! 4. Validates that no typed row is missing an organization linkage and
//!    every user has a membership; fails startup only if the validation
//!    check fails AFTER a complete backfill pass.
//!
//! Bootstrap is the only mechanism that mints the default Organization, so
//! controllers (which require it at startup) can rely on it being present
//! once `run` returns successfully.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use rise_resource_api::{OrganizationSpec, API_VERSION_V1ALPHA1, ORGANIZATION_KIND};
use rise_resource_store::{
    CreateResourceParams, OrganizationValidator, ResourceRow, ResourceStore, UpdateResourceParams,
};
use sqlx::PgPool;
use tracing::{info, warn};

use crate::db::organization_links;
use crate::server::settings::{
    DefaultOrganizationSettings, DeploymentControllerSettings, Settings,
};

/// Annotation key on the default Organization that the Kubernetes controller
/// reads to determine its per-project namespace prefix.
pub const NAMESPACE_PREFIX_ANNOTATION: &str = "kubernetes.rise.dev/namespace-prefix";

/// Stable, install-wide identifier for the advisory lock that gates the
/// bootstrap pass. Chosen arbitrarily; the only requirement is that no other
/// caller uses the same value.
const BOOTSTRAP_ADVISORY_LOCK_KEY: i64 = 0x7269_7365_0001_0001u64 as i64;

/// Result of the bootstrap pass — surfaced for tests and so the rest of the
/// startup path has access to the default Organization's resource UID.
#[derive(Debug, Clone)]
pub struct BootstrapOutcome {
    /// Resource UID of the default Organization (always set on success).
    pub default_organization_uid: uuid::Uuid,
    /// Number of teams updated by the typed-row backfill.
    #[allow(dead_code)]
    pub teams_backfilled: u64,
    /// Number of projects updated by the typed-row backfill.
    #[allow(dead_code)]
    pub projects_backfilled: u64,
    /// Number of new `user_organization_memberships` rows inserted by the
    /// typed-row backfill.
    #[allow(dead_code)]
    pub memberships_backfilled: u64,
}

/// Bootstrap entry point. Idempotent and concurrency-safe; safe to invoke on
/// every backend startup.
///
/// Returns the default Organization's resource UID once bootstrap has both
/// applied the configured Organization state and confirmed (via a validation
/// pass) that every existing typed row is linked.
pub async fn run(
    pool: &PgPool,
    store: &Arc<dyn ResourceStore>,
    settings: &Settings,
) -> Result<BootstrapOutcome> {
    info!("Running default-Organization bootstrap");

    // Acquire the advisory lock for the duration of this connection so
    // concurrent replicas serialize their bootstrap passes. The lock is
    // released automatically when the connection is returned to the pool.
    let mut conn = pool
        .acquire()
        .await
        .context("Failed to acquire DB connection for bootstrap")?;
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(BOOTSTRAP_ADVISORY_LOCK_KEY)
        .execute(&mut *conn)
        .await
        .context("Failed to acquire bootstrap advisory lock")?;

    // The `RAII` style for `pg_advisory_lock` isn't worth a custom guard:
    // the connection is dropped on every code path below (`?` returns,
    // success), and that automatically releases the session-scoped lock.
    let outcome = run_inner(pool, store, settings).await;

    // Best-effort release before returning — failures here are harmless
    // because the lock is also released when the connection drops.
    if let Err(e) = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(BOOTSTRAP_ADVISORY_LOCK_KEY)
        .execute(&mut *conn)
        .await
    {
        warn!("Failed to release bootstrap advisory lock: {:?}", e);
    }

    outcome
}

async fn run_inner(
    pool: &PgPool,
    store: &Arc<dyn ResourceStore>,
    settings: &Settings,
) -> Result<BootstrapOutcome> {
    let default_org = &settings.default_organization;
    let controller_class_name = controller_class_name_for_bootstrap(settings);

    // Step 1: upsert the default Organization FIRST, before any backfill.
    let org_row = upsert_default_organization(store, default_org, controller_class_name).await?;
    info!(
        organization = %org_row.name,
        uid = %org_row.uid,
        "Default Organization is in place"
    );

    // Step 2: backfill typed rows.
    let memberships_backfilled =
        organization_links::backfill_user_organization_memberships(pool, org_row.uid)
            .await
            .context("Backfill of user_organization_memberships failed")?;
    if memberships_backfilled > 0 {
        info!(
            count = memberships_backfilled,
            "Backfilled user_organization_memberships"
        );
    }

    let teams_backfilled = organization_links::backfill_teams_organization(pool, org_row.uid)
        .await
        .context("Backfill of teams.organization_resource_uid failed")?;
    if teams_backfilled > 0 {
        info!(
            count = teams_backfilled,
            "Backfilled team organization linkage"
        );
    }

    let projects_backfilled = organization_links::backfill_projects_organization(pool, org_row.uid)
        .await
        .context("Backfill of projects.organization_resource_uid failed")?;
    if projects_backfilled > 0 {
        info!(
            count = projects_backfilled,
            "Backfilled project organization linkage"
        );
    }

    // Step 3: validate.
    //
    // We only fail startup once a full backfill pass has completed — a
    // process crash mid-backfill leaves some rows unlinked, but the next
    // restart re-runs the (idempotent) backfill above before this validation
    // check fires.
    validate_linkage(pool, org_row.uid).await?;

    Ok(BootstrapOutcome {
        default_organization_uid: org_row.uid,
        teams_backfilled,
        projects_backfilled,
        memberships_backfilled,
    })
}

/// Resolve the `controller_class_name` for the configured deployment
/// controller. The Kubernetes controller carries an explicit field; other
/// backends are treated as having no controller class (the Organization's
/// `spec.deploymentControllerClass` is left unset, which means "no
/// controller manages this org's deployments").
pub fn controller_class_name_for_bootstrap(settings: &Settings) -> Option<&str> {
    match &settings.deployment_controller {
        #[cfg(feature = "backend")]
        Some(DeploymentControllerSettings::Kubernetes {
            controller_class_name,
            ..
        }) => Some(controller_class_name.as_str()),
        #[allow(unreachable_patterns)]
        _ => None,
    }
}

/// Locate or create the configured default Organization, then apply the
/// configured display name, namespace-prefix annotation, and
/// `spec.deploymentControllerClass`. Unrelated annotations on an existing
/// row are preserved.
async fn upsert_default_organization(
    store: &Arc<dyn ResourceStore>,
    default_org: &DefaultOrganizationSettings,
    controller_class_name: Option<&str>,
) -> Result<ResourceRow> {
    let namespace_prefix = default_org.resolved_namespace_prefix();
    let mut desired_annotations = default_org.annotations.clone();
    // Configured `kubernetes_namespace_prefix` always wins over any
    // collision in the `annotations` map.
    desired_annotations.insert(
        NAMESPACE_PREFIX_ANNOTATION.to_string(),
        namespace_prefix.clone(),
    );

    let existing = store
        .get_by_name(
            API_VERSION_V1ALPHA1,
            ORGANIZATION_KIND,
            &default_org.name,
            None,
        )
        .await
        .map_err(|e| anyhow!("Failed to look up default Organization: {e}"))?;

    let spec = build_organization_spec(&default_org.display_name, controller_class_name);

    match existing {
        Some(row) => {
            // Preserve unrelated annotations on the existing row; only ensure
            // our managed ones (namespace prefix, plus configured extras)
            // appear with the configured values.
            let existing_annotations = annotations_from_metadata(&row.metadata);
            let merged_annotations = merge_annotations(existing_annotations, &desired_annotations);

            // Skip the write when nothing changes — keeps the row's revision
            // stable across no-op restarts.
            let spec_value =
                serde_json::to_value(&spec).context("Failed to serialize Organization spec")?;
            if row.spec == spec_value
                && annotations_from_metadata(&row.metadata) == merged_annotations
                && row.finalizers.is_empty()
            {
                return Ok(row);
            }

            let updated = store
                .update(
                    row.uid,
                    UpdateResourceParams {
                        api_version: None,
                        revision: row.revision,
                        annotations: merged_annotations,
                        finalizers: row.finalizers.clone(),
                        spec: spec_value,
                        validator: Some(Arc::new(OrganizationValidator)),
                    },
                )
                .await
                .map_err(|e| {
                    anyhow!(
                        "Failed to update default Organization '{}': {e}",
                        default_org.name
                    )
                })?;
            Ok(updated)
        }
        None => {
            let spec_value =
                serde_json::to_value(&spec).context("Failed to serialize Organization spec")?;
            let created = store
                .create(CreateResourceParams {
                    api_version: API_VERSION_V1ALPHA1.to_string(),
                    kind: ORGANIZATION_KIND.to_string(),
                    name: default_org.name.clone(),
                    parent_uid: None,
                    annotations: desired_annotations,
                    finalizers: vec![],
                    spec: spec_value,
                    validator: Some(Arc::new(OrganizationValidator)),
                })
                .await
                .map_err(|e| {
                    anyhow!(
                        "Failed to create default Organization '{}': {e}",
                        default_org.name
                    )
                })?;
            Ok(created)
        }
    }
}

fn build_organization_spec(
    display_name: &str,
    controller_class_name: Option<&str>,
) -> OrganizationSpec {
    OrganizationSpec {
        display_name: display_name.to_string(),
        deployment_controller_class: controller_class_name.map(|s| s.to_string()),
    }
}

/// Parse the annotation map out of the metadata JSON blob. Treats malformed
/// or non-object metadata as an empty map rather than returning an error;
/// bootstrap is the layer that re-establishes the contract.
fn annotations_from_metadata(metadata: &serde_json::Value) -> BTreeMap<String, String> {
    serde_json::from_value(metadata.clone()).unwrap_or_default()
}

/// Merge two annotation maps, with the second map's entries overriding any
/// collisions in the first.
fn merge_annotations(
    base: BTreeMap<String, String>,
    overrides: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut merged = base;
    for (k, v) in overrides {
        merged.insert(k.clone(), v.clone());
    }
    merged
}

/// Validation pass that fails startup on a partial backfill. Runs after the
/// idempotent backfill has had a chance to repair earlier mid-backfill
/// crashes.
async fn validate_linkage(pool: &PgPool, organization_uid: uuid::Uuid) -> Result<()> {
    let missing_users =
        organization_links::count_users_missing_membership(pool, organization_uid).await?;
    let missing_teams = organization_links::count_teams_missing_organization(pool).await?;
    let missing_projects = organization_links::count_projects_missing_organization(pool).await?;

    if missing_users == 0 && missing_teams == 0 && missing_projects == 0 {
        return Ok(());
    }

    bail!(
        "Default-Organization bootstrap validation failed: \
         {missing_users} user(s), {missing_teams} team(s), {missing_projects} project(s) still missing organization linkage after backfill"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_annotations_overrides_collisions() {
        let mut base = BTreeMap::new();
        base.insert("a".to_string(), "old".to_string());
        base.insert("b".to_string(), "keep".to_string());

        let mut overrides = BTreeMap::new();
        overrides.insert("a".to_string(), "new".to_string());
        overrides.insert("c".to_string(), "added".to_string());

        let merged = merge_annotations(base, &overrides);
        assert_eq!(merged.get("a").map(String::as_str), Some("new"));
        assert_eq!(merged.get("b").map(String::as_str), Some("keep"));
        assert_eq!(merged.get("c").map(String::as_str), Some("added"));
    }

    #[test]
    fn annotations_from_metadata_handles_non_object() {
        let value = serde_json::json!("oops");
        assert!(annotations_from_metadata(&value).is_empty());
    }

    #[test]
    fn annotations_from_metadata_reads_flat_map() {
        let value = serde_json::json!({"k": "v"});
        let parsed = annotations_from_metadata(&value);
        assert_eq!(parsed.get("k").map(String::as_str), Some("v"));
    }

    #[test]
    fn build_organization_spec_includes_controller_class() {
        let spec = build_organization_spec("Default", Some("kubernetes.rise.dev/default"));
        assert_eq!(spec.display_name, "Default");
        assert_eq!(
            spec.deployment_controller_class.as_deref(),
            Some("kubernetes.rise.dev/default")
        );
    }

    #[test]
    fn build_organization_spec_without_controller_class() {
        let spec = build_organization_spec("Default", None);
        assert!(spec.deployment_controller_class.is_none());
    }
}
