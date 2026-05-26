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

use crate::db::global_lock::GlobalLock;
use crate::db::organization_links;
use crate::server::settings::{
    DefaultOrganizationSettings, DeploymentControllerSettings, Settings,
};

/// Annotation key on the default Organization that the Kubernetes controller
/// reads to determine its per-project namespace prefix.
pub const NAMESPACE_PREFIX_ANNOTATION: &str = "kubernetes.rise.dev/namespace-prefix";

/// Name of the install-wide [`GlobalLock`] that serializes the bootstrap pass
/// across replicas. Scoped string ("subsystem/purpose") to avoid colliding
/// with other `GlobalLock` callers.
const BOOTSTRAP_LOCK_NAME: &str = "bootstrap/default-organization";

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

    // Serialize bootstrap across replicas via a session-scoped advisory lock.
    // Bind the outcome before releasing so a failure in `run_inner` still
    // hits the explicit release path — see `GlobalLock` docs for why Drop
    // alone is not sufficient.
    let lock = GlobalLock::acquire(pool, BOOTSTRAP_LOCK_NAME).await?;
    let outcome = run_inner(pool, store, settings).await;
    if let Err(e) = lock.release().await {
        warn!("Failed to release bootstrap GlobalLock: {:?}", e);
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
fn controller_class_name_for_bootstrap(settings: &Settings) -> Option<&str> {
    match &settings.deployment_controller {
        Some(DeploymentControllerSettings::Kubernetes {
            controller_class_name,
            ..
        }) => Some(controller_class_name.as_str()),
        _ => None,
    }
}

/// Locate or create the configured default Organization, then apply the
/// configured display name, namespace-prefix annotation, and
/// `spec.deploymentControllerClass`. Unrelated annotations on an existing
/// row are preserved.
///
/// Lookup is by configured name first; if that misses, falls back to the
/// single-Org world's "find the only Organization in the store" rule so
/// renaming `default_organization.name` in config re-keys the existing row
/// (preserving its UID and every typed-row linkage) rather than minting a
/// new Organization and orphaning the linkages. Two or more existing
/// Organizations is treated as a configuration error: this PR's bootstrap
/// cannot guess which one is "the default."
async fn upsert_default_organization(
    store: &Arc<dyn ResourceStore>,
    default_org: &DefaultOrganizationSettings,
    controller_class_name: Option<&str>,
) -> Result<ResourceRow> {
    let mut desired_annotations = default_org.annotations.clone();
    // Stamp the namespace-prefix annotation only when explicitly configured.
    // Leaving it unset is the signal to the controller-side fallback
    // (`DefaultOrganizationView::resolved_namespace_prefix`) that it should
    // synthesize `org-{discriminator}-` for this org. When configured,
    // `kubernetes_namespace_prefix` always wins over any collision in the
    // freeform `annotations` map.
    if let Some(prefix) = default_org.kubernetes_namespace_prefix.as_ref() {
        desired_annotations.insert(NAMESPACE_PREFIX_ANNOTATION.to_string(), prefix.clone());
    }

    let spec = build_organization_spec(&default_org.display_name, controller_class_name);
    let spec_value =
        serde_json::to_value(&spec).context("Failed to serialize Organization spec")?;

    let Some(row) = find_default_organization(store, &default_org.name).await? else {
        // Fresh install: no Organizations exist yet.
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
        return Ok(created);
    };

    // If the row was located by the fallback (single-Org world) its name may
    // differ from the configured one. Rename first; UID and discriminator
    // are preserved, so every typed row's `organization_resource_uid`
    // linkage remains valid.
    let row = if row.name != default_org.name {
        info!(
            old_name = %row.name,
            new_name = %default_org.name,
            uid = %row.uid,
            "Renaming existing default Organization to match configured default_organization.name"
        );
        store
            .rename(row.uid, &default_org.name)
            .await
            .map_err(|e| {
                anyhow!(
                    "Failed to rename default Organization to '{}': {e}",
                    default_org.name
                )
            })?
    } else {
        row
    };

    // Preserve unrelated annotations on the existing row; only ensure our
    // managed ones (namespace prefix, plus configured extras) appear with
    // the configured values.
    let existing_annotations = annotations_from_metadata(&row.metadata);
    let merged_annotations = merge_annotations(existing_annotations, &desired_annotations);

    // Skip the write when nothing bootstrap-owned changes — keeps the row's
    // revision stable across no-op restarts. Finalizers are controller-owned
    // and preserved verbatim by the update path below, so their presence
    // must not force a rewrite here.
    if row.spec == spec_value && annotations_from_metadata(&row.metadata) == merged_annotations {
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

/// Locate the row this bootstrap pass should treat as the default
/// Organization. Returns `None` only on a fresh install (no Organizations
/// exist in the store).
///
/// - Fast path: look up by the configured name.
/// - Fallback: list every Organization in the store. If exactly one exists,
///   treat it as the default regardless of name (this is the rename path:
///   `default_organization.name` was changed in config; the caller will
///   rename the row's `name` to match).
/// - If two or more Organizations exist and none match the configured name,
///   refuse to guess and bail. This is unreachable on PR5 installs (only
///   bootstrap mints Organizations and only the configured name) but is the
///   safe behaviour once multi-org becomes reachable.
async fn find_default_organization(
    store: &Arc<dyn ResourceStore>,
    configured_name: &str,
) -> Result<Option<ResourceRow>> {
    if let Some(row) = store
        .get_by_name(
            API_VERSION_V1ALPHA1,
            ORGANIZATION_KIND,
            configured_name,
            None,
        )
        .await
        .map_err(|e| anyhow!("Failed to look up default Organization: {e}"))?
    {
        return Ok(Some(row));
    }

    let mut existing = store
        .list_versions(&[API_VERSION_V1ALPHA1.to_string()], ORGANIZATION_KIND, None)
        .await
        .map_err(|e| anyhow!("Failed to list existing Organizations: {e}"))?;

    match existing.len() {
        0 => Ok(None),
        1 => Ok(Some(existing.remove(0))),
        n => {
            let names: Vec<&str> = existing.iter().map(|r| r.name.as_str()).collect();
            bail!(
                "Found {n} existing Organizations (names: {names:?}) but none match the configured \
                 default_organization.name '{configured_name}'. Refusing to guess which one is the \
                 default. Set default_organization.name to one of the existing Organizations, or \
                 delete the unwanted ones."
            )
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

/// Resolved view of the default Organization for downstream consumers.
///
/// Returned by [`load_default_organization_view`] so the Kubernetes
/// controller can read its namespace prefix and `deploymentControllerClass`
/// without re-running the bootstrap path.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct DefaultOrganizationView {
    pub uid: uuid::Uuid,
    pub discriminator: String,
    pub namespace_prefix: Option<String>,
    pub deployment_controller_class: Option<String>,
}

impl DefaultOrganizationView {
    /// Namespace prefix to apply to projects in this Organization.
    /// Returns the `kubernetes.rise.dev/namespace-prefix` annotation when
    /// present; otherwise synthesizes `org-{discriminator}-` so each Org
    /// gets a unique-by-construction prefix without any operator config.
    ///
    /// Upgrade callers (single-org installs that previously named
    /// namespaces `rise-{project_name}`) must set
    /// `default_organization.kubernetes_namespace_prefix = "rise-"` so
    /// bootstrap stamps the annotation; otherwise the discriminator
    /// fallback will rename the namespaces and orphan the legacy ones.
    #[allow(dead_code)]
    pub fn resolved_namespace_prefix(&self) -> String {
        match self.namespace_prefix.as_deref() {
            Some(p) => p.to_string(),
            None => resolve_namespace_prefix_fallback(&self.discriminator),
        }
    }
}

/// Apply the `org-{discriminator}-` fallback when no namespace-prefix
/// annotation is configured. Extracted so the per-request multi-org path
/// (`webhook::load_org_namespace_prefix`) and the bootstrap-time
/// `DefaultOrganizationView::resolved_namespace_prefix` agree by
/// construction.
pub fn resolve_namespace_prefix_fallback(discriminator: &str) -> String {
    format!("org-{discriminator}-")
}

/// Resolve a project's Kubernetes namespace prefix from its Organization's
/// annotation map and discriminator: the
/// `kubernetes.rise.dev/namespace-prefix` annotation wins, otherwise fall
/// back to `org-{discriminator}-`. The single source of truth shared by
/// bootstrap and the webhook's per-sync cache loader.
pub fn resolve_namespace_prefix(
    annotations: &BTreeMap<String, String>,
    discriminator: &str,
) -> String {
    annotations
        .get(NAMESPACE_PREFIX_ANNOTATION)
        .cloned()
        .unwrap_or_else(|| resolve_namespace_prefix_fallback(discriminator))
}

/// Look up the default Organization row from the store and project the
/// fields the Kubernetes controller needs. Returns `None` only when the
/// resource is absent (controllers should fail startup in that case).
#[allow(dead_code)]
pub async fn load_default_organization_view(
    store: &Arc<dyn ResourceStore>,
    default_org: &DefaultOrganizationSettings,
) -> Result<Option<DefaultOrganizationView>> {
    let row = store
        .get_by_name(
            API_VERSION_V1ALPHA1,
            ORGANIZATION_KIND,
            &default_org.name,
            None,
        )
        .await
        .map_err(|e| anyhow!("Failed to load default Organization: {e}"))?;
    let Some(row) = row else {
        return Ok(None);
    };
    let annotations = annotations_from_metadata(&row.metadata);
    let namespace_prefix = annotations.get(NAMESPACE_PREFIX_ANNOTATION).cloned();
    let spec: OrganizationSpec = serde_json::from_value(row.spec.clone())
        .context("Stored Organization spec is malformed")?;
    Ok(Some(DefaultOrganizationView {
        uid: row.uid,
        discriminator: row.discriminator,
        namespace_prefix,
        deployment_controller_class: spec.deployment_controller_class,
    }))
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
        let spec = build_organization_spec("Default", Some("default"));
        assert_eq!(spec.display_name, "Default");
        assert_eq!(spec.deployment_controller_class.as_deref(), Some("default"));
    }

    #[test]
    fn build_organization_spec_without_controller_class() {
        let spec = build_organization_spec("Default", None);
        assert!(spec.deployment_controller_class.is_none());
    }

    #[test]
    fn resolve_namespace_prefix_uses_annotation_when_present() {
        let mut annotations = BTreeMap::new();
        annotations.insert(NAMESPACE_PREFIX_ANNOTATION.to_string(), "rise-".to_string());
        // Discriminator is ignored when the annotation is set.
        assert_eq!(resolve_namespace_prefix(&annotations, "abcd"), "rise-");
    }

    #[test]
    fn resolve_namespace_prefix_falls_back_to_discriminator() {
        let annotations = BTreeMap::new();
        assert_eq!(resolve_namespace_prefix(&annotations, "abcd"), "org-abcd-");
    }

    /// Construct a minimal `Settings` for tests. Only `default_organization`
    /// matters to `bootstrap::run`; the other required fields exist purely
    /// to satisfy `serde`.
    fn test_settings() -> Settings {
        test_settings_with_org_name("default")
    }

    fn test_settings_with_org_name(name: &str) -> Settings {
        let json = serde_json::json!({
            "server": {
                "host": "0.0.0.0",
                "port": 3000,
                "public_url": "http://test.local",
                "jwt_signing_secret": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            },
            "auth": {
                "issuer": "http://test.local",
                "client_id": "test",
                "client_secret": "test",
            },
            "default_organization": {
                "name": name,
            },
        });
        serde_json::from_value(json).expect("test settings")
    }

    #[sqlx::test]
    async fn bootstrap_is_idempotent(pool: sqlx::PgPool) {
        // Layer the resource-store schema on top of the root migrations that
        // `#[sqlx::test]` already ran.
        rise_resource_store::run_migrations(&pool)
            .await
            .expect("resource store migrations");
        let store: Arc<dyn ResourceStore> =
            Arc::new(rise_resource_store::PgResourceStore::new(pool.clone()));

        let settings = test_settings();

        // 1. First run: creates the default Organization. No typed rows
        //    exist yet, so all backfill counts are zero.
        let first = run(&pool, &store, &settings).await.expect("first run");
        assert_eq!(first.memberships_backfilled, 0);
        assert_eq!(first.teams_backfilled, 0);
        assert_eq!(first.projects_backfilled, 0);

        // 2. Seed pre-PR5-style rows: a user (no membership), a team and a
        //    project (both with NULL organization_resource_uid). The Entra
        //    sync path inserts users via `users::create` so we mirror that.
        let user = crate::db::users::create(&pool, "seed@example.com")
            .await
            .expect("seed user");
        let team = crate::db::teams::create(&pool, "seed-team")
            .await
            .expect("seed team");
        let project = crate::db::projects::create(
            &pool,
            "seed-project",
            crate::db::models::ProjectStatus::Stopped,
            "public".to_string(),
            Some(user.id),
            None,
            None,
        )
        .await
        .expect("seed project");

        // Sanity: the seed rows have no organization linkage yet.
        let team_org_pre: Option<uuid::Uuid> =
            sqlx::query_scalar("SELECT organization_resource_uid FROM teams WHERE id = $1")
                .bind(team.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(team_org_pre.is_none(), "team should start unlinked");
        let project_org_pre: Option<uuid::Uuid> =
            sqlx::query_scalar("SELECT organization_resource_uid FROM projects WHERE id = $1")
                .bind(project.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(project_org_pre.is_none(), "project should start unlinked");

        // 3. Second run: backfill picks up the seed rows. The Organization
        //    UID must match the first run — no new Org is minted.
        let second = run(&pool, &store, &settings).await.expect("second run");
        assert_eq!(
            first.default_organization_uid,
            second.default_organization_uid
        );
        assert_eq!(second.memberships_backfilled, 1);
        assert_eq!(second.teams_backfilled, 1);
        assert_eq!(second.projects_backfilled, 1);

        // Verify the linkage is actually persisted.
        let team_org: Option<uuid::Uuid> =
            sqlx::query_scalar("SELECT organization_resource_uid FROM teams WHERE id = $1")
                .bind(team.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(team_org, Some(second.default_organization_uid));

        // 4. Third run: nothing left to backfill, validation still passes.
        let third = run(&pool, &store, &settings).await.expect("third run");
        assert_eq!(
            third.default_organization_uid,
            second.default_organization_uid
        );
        assert_eq!(third.memberships_backfilled, 0);
        assert_eq!(third.teams_backfilled, 0);
        assert_eq!(third.projects_backfilled, 0);
    }

    /// Renaming `default_organization.name` in config re-keys the existing
    /// row rather than creating a new Organization. UID is preserved so
    /// every typed-row linkage remains valid.
    #[sqlx::test]
    async fn bootstrap_renames_existing_default_organization(pool: sqlx::PgPool) {
        rise_resource_store::run_migrations(&pool)
            .await
            .expect("resource store migrations");
        let store: Arc<dyn ResourceStore> =
            Arc::new(rise_resource_store::PgResourceStore::new(pool.clone()));

        // First boot mints the Org under the configured name.
        let first = run(&pool, &store, &test_settings_with_org_name("default"))
            .await
            .expect("first run");

        // Second boot with a different name must reuse the same row.
        let renamed = run(&pool, &store, &test_settings_with_org_name("acme"))
            .await
            .expect("second run after rename");
        assert_eq!(
            renamed.default_organization_uid, first.default_organization_uid,
            "rename must preserve the Organization UID"
        );

        // The store row's name is now the new one.
        let row = store
            .get(first.default_organization_uid)
            .await
            .expect("get post-rename")
            .expect("row exists");
        assert_eq!(row.name, "acme");

        // Third boot with the new name takes the fast path (no rename).
        let again = run(&pool, &store, &test_settings_with_org_name("acme"))
            .await
            .expect("third run");
        assert_eq!(
            again.default_organization_uid,
            first.default_organization_uid
        );
    }

    /// Two or more existing Organizations, none matching the configured
    /// name, is a configuration error: bootstrap cannot guess which is the
    /// default and refuses to start.
    #[sqlx::test]
    async fn bootstrap_bails_on_multiple_existing_orgs_with_no_name_match(pool: sqlx::PgPool) {
        rise_resource_store::run_migrations(&pool)
            .await
            .expect("resource store migrations");
        let store: Arc<dyn ResourceStore> =
            Arc::new(rise_resource_store::PgResourceStore::new(pool.clone()));

        // Mint the first Org via bootstrap.
        run(&pool, &store, &test_settings_with_org_name("default"))
            .await
            .expect("first run");

        // Mint a second Org directly through the store.
        store
            .create(CreateResourceParams {
                api_version: API_VERSION_V1ALPHA1.to_string(),
                kind: ORGANIZATION_KIND.to_string(),
                name: "other".to_string(),
                parent_uid: None,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                spec: serde_json::json!({"displayName": "Other"}),
                validator: Some(Arc::new(OrganizationValidator)),
            })
            .await
            .expect("create second org");

        // Bootstrap with a configured name matching neither must error.
        let err = run(&pool, &store, &test_settings_with_org_name("renamed"))
            .await
            .expect_err("bootstrap should refuse to guess");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Found 2 existing Organizations"),
            "expected the multi-org bail message, got: {msg}"
        );
    }
}
