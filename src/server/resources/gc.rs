//! Background garbage-collection worker for the generic resource store.
//!
//! `store.delete()` always cascades by stamping `deletion_timestamp` on the
//! target and its *immediate* children, then attaching the
//! `system.rise.dev/cascade-deletion` system finalizer to the target when
//! children exist. Nothing in the request path drains the resulting subtree —
//! that is this worker's job.
//!
//! On each tick the leader fetches a batch of tombstoned rows oldest-first
//! (`list_pending_collection`) and calls `try_collect` on each. The store
//! handles per-row mechanics:
//!   - children remain → stamps the next layer and keeps the cascade finalizer
//!   - all finalizers (controller + cascade) clear → hard-deletes the row
//!
//! Controllers progressively shed their own finalizers from the bottom of the
//! tree; successive sweeps eventually drain each subtree.
//!
//! Out of scope for this PR (per `MULTI_TENANCY_PLAN.md`):
//!   - Application-layer block on deleting an Organization with typed children
//!     (teams/projects via `organization_resource_uid`) — lands with PR 5.
//!   - Real Prometheus/OpenTelemetry metrics — not yet present in the codebase.
//!     Structured `tracing` fields stand in until that lands.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rise_resource_store::{DeleteOutcome, ResourceRow, ResourceStore};
use sqlx::PgPool;
use tokio::time::interval;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::db::leader_leases::LeaderElection;
use crate::server::settings::ResourceGcSettings;

const ACTOR: &str = "system:resource-gc";
const LEASE_NAME: &str = "rise-resource-gc";

/// Background sweeper that drives `try_collect` over tombstoned resource rows.
pub struct ResourceGcController {
    store: Arc<dyn ResourceStore>,
    election: LeaderElection,
    settings: ResourceGcSettings,
}

impl ResourceGcController {
    pub fn new(pool: PgPool, store: Arc<dyn ResourceStore>, settings: ResourceGcSettings) -> Self {
        let election = LeaderElection::spawn(
            pool,
            LEASE_NAME,
            Uuid::new_v4(),
            Duration::from_secs(settings.lease_duration_secs),
        );
        Self {
            store,
            election,
            settings,
        }
    }

    pub fn start(self: Arc<Self>) {
        tokio::spawn(async move {
            self.gc_loop().await;
        });
    }

    async fn gc_loop(&self) {
        info!(
            interval_secs = self.settings.interval_secs,
            batch_size = self.settings.batch_size,
            max_batches_per_tick = self.settings.max_batches_per_tick,
            "Resource GC loop started"
        );
        let mut ticker = interval(Duration::from_secs(self.settings.interval_secs));
        loop {
            ticker.tick().await;
            if !self.election.is_leader() {
                continue;
            }
            if let Err(e) = self.sweep().await {
                error!(error = ?e, "resource GC sweep failed");
            }
        }
    }

    /// Run a single sweep across up to `max_batches_per_tick` batches. Each
    /// batch invokes `try_collect` once per row and isolates per-row errors so
    /// a single failure cannot stall the loop.
    async fn sweep(&self) -> anyhow::Result<SweepStats> {
        let stuck_threshold = Duration::from_secs(self.settings.stuck_threshold_secs);
        let now = Utc::now();
        let mut stats = SweepStats::default();

        for _ in 0..self.settings.max_batches_per_tick {
            let batch = self
                .store
                .list_pending_collection(self.settings.batch_size)
                .await?;
            let batch_len = batch.len();
            if batch_len == 0 {
                break;
            }

            for row in &batch {
                self.process_row(row, now, stuck_threshold, &mut stats)
                    .await;
            }

            // Short batch → no more candidates this tick. Avoids a wasted
            // round-trip when the backlog is drained.
            if (batch_len as i64) < self.settings.batch_size {
                break;
            }
        }

        if stats.has_activity() {
            debug!(
                collected = stats.collected,
                still_pending = stats.still_pending,
                errors = stats.errors,
                "resource GC sweep complete"
            );
        }
        Ok(stats)
    }

    async fn process_row(
        &self,
        row: &ResourceRow,
        now: DateTime<Utc>,
        stuck_threshold: Duration,
        stats: &mut SweepStats,
    ) {
        if is_stuck(row, now, stuck_threshold) {
            warn!(
                uid = %row.uid,
                api_version = %row.api_version,
                kind = %row.kind,
                name = %row.name,
                finalizers = ?row.finalizers,
                stuck_for_secs = stuck_for_secs(row, now),
                "resource stuck pending collection"
            );
            stats.stuck += 1;
        }

        match self.store.try_collect(row.uid).await {
            Ok(DeleteOutcome::Deleted) => {
                stats.collected += 1;
                info!(
                    target: "rise::audit",
                    actor = ACTOR,
                    uid = %row.uid,
                    api_version = %row.api_version,
                    kind = %row.kind,
                    name = %row.name,
                    "resource.deleted"
                );
            }
            Ok(DeleteOutcome::MarkedForDeletion(_)) => {
                stats.still_pending += 1;
            }
            Err(e) => {
                stats.errors += 1;
                error!(
                    uid = %row.uid,
                    api_version = %row.api_version,
                    kind = %row.kind,
                    name = %row.name,
                    error = ?e,
                    "try_collect failed; continuing"
                );
            }
        }
    }
}

#[derive(Default, Debug, PartialEq, Eq)]
struct SweepStats {
    collected: u64,
    still_pending: u64,
    errors: u64,
    stuck: u64,
}

impl SweepStats {
    fn has_activity(&self) -> bool {
        self.collected + self.still_pending + self.errors + self.stuck > 0
    }
}

fn is_stuck(row: &ResourceRow, now: DateTime<Utc>, threshold: Duration) -> bool {
    match row.deletion_timestamp {
        Some(ts) => match (now - ts).to_std() {
            Ok(stuck_for) => stuck_for > threshold,
            // Negative duration: clock skew or freshly stamped — not stuck.
            Err(_) => false,
        },
        None => false,
    }
}

fn stuck_for_secs(row: &ResourceRow, now: DateTime<Utc>) -> u64 {
    row.deletion_timestamp
        .and_then(|ts| (now - ts).to_std().ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use rise_resource_api::{API_VERSION_V1ALPHA1, ORGANIZATION_KIND, RESOURCE_DEFINITION_KIND};
    use rise_resource_store::{
        CollectionInfo, CreateResourceParams, PathSegment, PgResourceStore, StoreError,
        UpdateResourceParams,
    };
    use serde_json::json;
    use std::collections::BTreeMap;

    const CONTROLLER_ID: &str = "controller.example.com";
    const CONTROLLER_FINALIZER: &str = "controller.example.com/cleanup";

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn default_settings() -> ResourceGcSettings {
        ResourceGcSettings {
            interval_secs: 1,
            batch_size: 50,
            max_batches_per_tick: 4,
            stuck_threshold_secs: 3600,
            lease_duration_secs: 60,
        }
    }

    /// Layer the resource-store schema on top of the root migrations that
    /// `#[sqlx::test]` already ran, mirroring the pattern in
    /// `src/server/resources/handlers.rs` tests.
    async fn store_for(pool: PgPool) -> Arc<dyn ResourceStore> {
        rise_resource_store::run_migrations(&pool)
            .await
            .expect("resource store migrations");
        Arc::new(PgResourceStore::new(pool))
    }

    async fn create_org(store: &dyn ResourceStore, name: &str) -> ResourceRow {
        store
            .create(CreateResourceParams {
                api_version: API_VERSION_V1ALPHA1.to_string(),
                kind: ORGANIZATION_KIND.to_string(),
                name: name.to_string(),
                parent_uid: None,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                spec: json!({"displayName": name}),
                validator: None,
            })
            .await
            .unwrap()
    }

    /// Create an Organization carrying a controller-scoped finalizer so that
    /// `delete()` leaves it tombstoned instead of hard-deleting immediately.
    async fn create_org_with_finalizer(store: &dyn ResourceStore, name: &str) -> ResourceRow {
        store
            .create(CreateResourceParams {
                api_version: API_VERSION_V1ALPHA1.to_string(),
                kind: ORGANIZATION_KIND.to_string(),
                name: name.to_string(),
                parent_uid: None,
                annotations: BTreeMap::new(),
                finalizers: vec![CONTROLLER_FINALIZER.to_string()],
                spec: json!({"displayName": name}),
                validator: None,
            })
            .await
            .unwrap()
    }

    async fn register_widget_definition(store: &dyn ResourceStore) {
        store
            .register_resource_definition(CreateResourceParams {
                api_version: API_VERSION_V1ALPHA1.to_string(),
                kind: RESOURCE_DEFINITION_KIND.to_string(),
                name: "widgets.example.dev".to_string(),
                parent_uid: None,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                spec: json!({
                    "group": "example.dev",
                    "kind": "Widget",
                    "plural": "widgets",
                    "parent": {"apiVersion": API_VERSION_V1ALPHA1, "kind": ORGANIZATION_KIND},
                    "versions": [{"name": "v1", "served": true, "storage": true}],
                    "allowedStatusControllerIds": [CONTROLLER_ID]
                }),
                validator: None,
            })
            .await
            .unwrap();
    }

    async fn create_widget(
        store: &dyn ResourceStore,
        parent: Uuid,
        name: &str,
        finalizers: Vec<String>,
    ) -> ResourceRow {
        store
            .create(CreateResourceParams {
                api_version: "example.dev/v1".to_string(),
                kind: "Widget".to_string(),
                name: name.to_string(),
                parent_uid: Some(parent),
                annotations: BTreeMap::new(),
                finalizers,
                spec: json!({}),
                validator: None,
            })
            .await
            .unwrap()
    }

    // -----------------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------------

    #[sqlx::test]
    async fn sweep_drains_two_level_cascade_after_finalizer_cleared(
        pool: PgPool,
    ) -> sqlx::Result<()> {
        let store = store_for(pool.clone()).await;
        register_widget_definition(&*store).await;

        let parent = create_org(&*store, "drain-org").await;
        let child = create_widget(
            &*store,
            parent.uid,
            "w1",
            vec![CONTROLLER_FINALIZER.to_string()],
        )
        .await;

        store.delete(parent.uid).await.unwrap();
        // `delete()` stamps the parent and its immediate children in a single
        // transaction. With one child, both are tombstoned and on the
        // `list_pending_collection` feed immediately.

        let gc = ResourceGcController::new(pool.clone(), store.clone(), default_settings());

        // First sweep: parent has a child → MarkedForDeletion. Child has its
        // controller finalizer → MarkedForDeletion. Nothing collected yet.
        let stats = gc.sweep().await.unwrap();
        assert_eq!(stats.collected, 0);
        assert!(stats.still_pending >= 2);
        assert_eq!(stats.errors, 0);
        assert!(store.get(parent.uid).await.unwrap().is_some());
        assert!(store.get(child.uid).await.unwrap().is_some());

        // Controller clears its finalizer.
        store
            .update_controller_finalizers(
                child.uid,
                CONTROLLER_ID,
                &[],
                &[CONTROLLER_FINALIZER.to_string()],
            )
            .await
            .unwrap();

        // Drain the cascade. The first post-clear sweep collects the child
        // (no finalizers, no children) but cannot yet collect the parent
        // because the in-batch view still sees the child as a child. The
        // next sweep re-lists, sees the parent has no children, clears its
        // cascade finalizer, and hard-deletes it. Allow a few sweeps so the
        // assertion does not depend on `list_pending_collection`'s
        // unspecified order for rows with identical `deletion_timestamp`.
        let mut total_collected = 0u64;
        for _ in 0..8 {
            let s = gc.sweep().await.unwrap();
            total_collected += s.collected;
            assert_eq!(s.errors, 0);
            if store.get(parent.uid).await.unwrap().is_none()
                && store.get(child.uid).await.unwrap().is_none()
            {
                break;
            }
        }
        assert_eq!(total_collected, 2);
        assert!(store.get(parent.uid).await.unwrap().is_none());
        assert!(store.get(child.uid).await.unwrap().is_none());
        Ok(())
    }

    #[sqlx::test]
    async fn sweep_isolates_per_row_errors(pool: PgPool) -> sqlx::Result<()> {
        let inner = store_for(pool.clone()).await;

        // Each row carries a controller-scoped finalizer at creation time so
        // `delete()` tombstones it instead of hard-deleting (a finalizer-free,
        // childless root is collected immediately by `delete` itself, which
        // would leave nothing for the sweep to process).
        let a = create_org_with_finalizer(&*inner, "row-a").await;
        let b = create_org_with_finalizer(&*inner, "row-b").await;
        let c = create_org_with_finalizer(&*inner, "row-c").await;
        for r in [&a, &b, &c] {
            inner.delete(r.uid).await.unwrap();
        }

        // Drop the finalizer on the two rows we expect the sweep to collect.
        // `b` keeps its finalizer, but the failing decorator errors before
        // `try_collect` is reached anyway.
        for r in [&a, &c] {
            inner
                .update_controller_finalizers(
                    r.uid,
                    CONTROLLER_ID,
                    &[],
                    &[CONTROLLER_FINALIZER.to_string()],
                )
                .await
                .unwrap();
        }

        let failing: Arc<dyn ResourceStore> = Arc::new(FailingStore {
            inner: inner.clone(),
            fail_for: b.uid,
        });
        let gc = ResourceGcController::new(pool.clone(), failing, default_settings());

        let stats = gc.sweep().await.unwrap();
        assert_eq!(stats.errors, 1, "exactly one row should fail");
        assert_eq!(stats.collected, 2, "the other two should be collected");

        // Verify against the underlying store: a and c are gone, b survives.
        assert!(inner.get(a.uid).await.unwrap().is_none());
        assert!(inner.get(c.uid).await.unwrap().is_none());
        assert!(inner.get(b.uid).await.unwrap().is_some());
        Ok(())
    }

    #[sqlx::test]
    async fn sweep_marks_rows_stuck_past_threshold(pool: PgPool) -> sqlx::Result<()> {
        let store = store_for(pool.clone()).await;
        register_widget_definition(&*store).await;

        let parent = create_org(&*store, "stuck-org").await;
        let _child = create_widget(
            &*store,
            parent.uid,
            "w1",
            vec![CONTROLLER_FINALIZER.to_string()],
        )
        .await;
        store.delete(parent.uid).await.unwrap();

        // Backdate the deletion_timestamp on every tombstoned row past the
        // threshold so the sweep sees them as stuck.
        sqlx::query(
            "UPDATE resource_store.resources \
             SET deletion_timestamp = NOW() - INTERVAL '2 hours' \
             WHERE deletion_timestamp IS NOT NULL",
        )
        .execute(&pool)
        .await?;

        let mut settings = default_settings();
        settings.stuck_threshold_secs = 60;
        let gc = ResourceGcController::new(pool.clone(), store.clone(), settings);

        let stats = gc.sweep().await.unwrap();
        assert!(
            stats.stuck >= 2,
            "both tombstoned rows should be flagged stuck, got {stats:?}"
        );
        Ok(())
    }

    #[test]
    fn is_stuck_handles_clock_skew_and_unset_timestamp() {
        let now = Utc::now();
        let mut row = sample_row();

        row.deletion_timestamp = None;
        assert!(!is_stuck(&row, now, Duration::from_secs(60)));

        // Future timestamp (clock skew) — not stuck.
        row.deletion_timestamp = Some(now + chrono::Duration::seconds(10));
        assert!(!is_stuck(&row, now, Duration::from_secs(60)));

        // Recent — not stuck.
        row.deletion_timestamp = Some(now - chrono::Duration::seconds(30));
        assert!(!is_stuck(&row, now, Duration::from_secs(60)));

        // Past threshold — stuck.
        row.deletion_timestamp = Some(now - chrono::Duration::seconds(120));
        assert!(is_stuck(&row, now, Duration::from_secs(60)));
    }

    fn sample_row() -> ResourceRow {
        ResourceRow {
            uid: Uuid::new_v4(),
            api_version: API_VERSION_V1ALPHA1.to_string(),
            kind: ORGANIZATION_KIND.to_string(),
            parent_uid: None,
            name: "sample".to_string(),
            discriminator: "abcdefgh".to_string(),
            metadata: json!({}),
            spec: json!({}),
            status: json!({}),
            revision: 1,
            finalizers: vec![],
            deletion_timestamp: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    /// `ResourceStore` decorator that errors from `try_collect` for one UID.
    /// All other calls pass through unchanged.
    struct FailingStore {
        inner: Arc<dyn ResourceStore>,
        fail_for: Uuid,
    }

    #[async_trait]
    impl ResourceStore for FailingStore {
        async fn create(&self, params: CreateResourceParams) -> Result<ResourceRow, StoreError> {
            self.inner.create(params).await
        }

        async fn get(&self, uid: Uuid) -> Result<Option<ResourceRow>, StoreError> {
            self.inner.get(uid).await
        }

        async fn get_by_name(
            &self,
            api_version: &str,
            kind: &str,
            name: &str,
            parent_uid: Option<Uuid>,
        ) -> Result<Option<ResourceRow>, StoreError> {
            self.inner
                .get_by_name(api_version, kind, name, parent_uid)
                .await
        }

        async fn list(
            &self,
            api_version: &str,
            kind: &str,
            parent_uid: Option<Uuid>,
        ) -> Result<Vec<ResourceRow>, StoreError> {
            self.inner.list(api_version, kind, parent_uid).await
        }

        async fn list_versions(
            &self,
            api_versions: &[String],
            kind: &str,
            parent_uid: Option<Uuid>,
        ) -> Result<Vec<ResourceRow>, StoreError> {
            self.inner
                .list_versions(api_versions, kind, parent_uid)
                .await
        }

        async fn update(
            &self,
            uid: Uuid,
            params: UpdateResourceParams,
        ) -> Result<ResourceRow, StoreError> {
            self.inner.update(uid, params).await
        }

        async fn delete(&self, uid: Uuid) -> Result<DeleteOutcome, StoreError> {
            self.inner.delete(uid).await
        }

        async fn try_collect(&self, uid: Uuid) -> Result<DeleteOutcome, StoreError> {
            if uid == self.fail_for {
                return Err(StoreError::Validation("injected".to_string()));
            }
            self.inner.try_collect(uid).await
        }

        async fn list_pending_collection(
            &self,
            limit: i64,
        ) -> Result<Vec<ResourceRow>, StoreError> {
            self.inner.list_pending_collection(limit).await
        }

        async fn resolve_path(
            &self,
            segments: &[PathSegment],
        ) -> Result<Vec<ResourceRow>, StoreError> {
            self.inner.resolve_path(segments).await
        }

        async fn update_controller_status(
            &self,
            uid: Uuid,
            controller_id: &str,
            status_value: serde_json::Value,
        ) -> Result<ResourceRow, StoreError> {
            self.inner
                .update_controller_status(uid, controller_id, status_value)
                .await
        }

        async fn update_controller_finalizers(
            &self,
            uid: Uuid,
            controller_id: &str,
            add: &[String],
            remove: &[String],
        ) -> Result<ResourceRow, StoreError> {
            self.inner
                .update_controller_finalizers(uid, controller_id, add, remove)
                .await
        }

        async fn operator_update_status(
            &self,
            uid: Uuid,
            operator: &str,
            status_value: serde_json::Value,
        ) -> Result<ResourceRow, StoreError> {
            self.inner
                .operator_update_status(uid, operator, status_value)
                .await
        }

        async fn operator_update_finalizers(
            &self,
            uid: Uuid,
            operator: &str,
            add: &[String],
            remove: &[String],
        ) -> Result<ResourceRow, StoreError> {
            self.inner
                .operator_update_finalizers(uid, operator, add, remove)
                .await
        }

        async fn resolve_collection(
            &self,
            collection: &str,
        ) -> Result<Option<CollectionInfo>, StoreError> {
            self.inner.resolve_collection(collection).await
        }

        async fn resolve_collection_version(
            &self,
            group: &str,
            version: &str,
            collection: &str,
        ) -> Result<Option<CollectionInfo>, StoreError> {
            self.inner
                .resolve_collection_version(group, version, collection)
                .await
        }

        async fn resolve_collection_by_kind(
            &self,
            group: &str,
            kind: &str,
        ) -> Result<Option<CollectionInfo>, StoreError> {
            self.inner.resolve_collection_by_kind(group, kind).await
        }

        async fn register_resource_definition(
            &self,
            params: CreateResourceParams,
        ) -> Result<ResourceRow, StoreError> {
            self.inner.register_resource_definition(params).await
        }

        async fn update_resource_definition(
            &self,
            uid: Uuid,
            params: UpdateResourceParams,
        ) -> Result<ResourceRow, StoreError> {
            self.inner.update_resource_definition(uid, params).await
        }
    }
}
