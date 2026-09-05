//! Background garbage-collection worker for the generic resource store.
//!
//! `store.delete()` cascades by stamping `deletion_timestamp` on every immediate
//! structural child and owner-reference dependent. The
//! `system.rise.dev/cascade-deletion` finalizer retains the target only while a
//! structural child or an owner reference with `blockOwnerDeletion: true`
//! remains. Non-blocking cross-tree dependents continue draining after their
//! owner disappears. Nothing in the request path drains the resulting graph —
//! that is this worker's job.
//!
//! On each tick the leader fetches a batch of tombstoned rows oldest-first
//! (`list_pending_collection`) and calls `try_collect` on each. The store
//! handles per-row mechanics:
//!   - lifecycle dependents remain → stamps the next layer and keeps the
//!     cascade finalizer only for blocking edges
//!   - all finalizers (controller + cascade) clear → hard-deletes the row
//!
//! Controllers progressively shed their own finalizers from lifecycle leaves;
//! successive sweeps eventually drain the graph.
//!
//! Robustness properties worth knowing:
//!   - **Forward-progress guard**: within a single sweep we track which UIDs
//!     have been attempted and grow the SQL LIMIT so that already-tried rows
//!     no longer hide newer tombstones at the tail of the deterministically
//!     sorted feed. Stuck rows (controllers whose finalizer never clears)
//!     therefore cannot starve newer rows within one tick.
//!   - **Panic supervisor**: `run()` supervises the GC loop, respawning it
//!     after a brief backoff if it panics, so a single bad row cannot silently
//!     stop garbage collection.
//!   - **Per-row leadership check**: leadership is re-verified immediately
//!     before each destructive `try_collect` call via `ensure_leader_for`.
//!     The heartbeat publishes a cached lease horizon after every successful
//!     renew, so in the steady state the per-row check is a local-clock
//!     comparison with zero DB cost. When the horizon is insufficient (or
//!     stale), it falls back to a `verify_leader` DB round-trip — same
//!     DB-blip-tolerant semantics as the sibling controllers (project/ECR/
//!     Snowflake), but ~200 round-trips per sweep cheaper on the hot path.
//!   - **Sweep-error backoff**: consecutive sweep failures (e.g. DB outage)
//!     trigger an exponential backoff capped at 60s, preventing log spam.
//!
//! Not covered here:
//!   - Real Prometheus/OpenTelemetry metrics — not yet present in the codebase.
//!     Structured `tracing` fields stand in until that lands.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rise_resource_api::{DeleteOutcome, ResourceRow, ResourceStore, StoreError};
use sqlx::PgPool;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::server::settings::ResourceGcSettings;
use rise_runtime_sync::{
    with_leader_election, GlobalSchedule, LeaderElection, LeaderStatus, LeaseError, LEASE_DURATION,
};
use tokio_util::sync::CancellationToken;

const ACTOR: &str = "system:resource-gc";
const LEASE_NAME: &str = "rise-resource-gc";
const SCHEDULE_NAME: &str = "rise-resource-gc-sweep";
/// Minimum lease validity required before each destructive `try_collect` call.
/// Comfortably above the p99 cost of one `try_collect` round-trip with headroom;
/// well below the `LEASE_DURATION / 2` ceiling enforced by `ensure_leader_for`,
/// so the local-clock fast path succeeds essentially 100% of the time while
/// heartbeats are healthy (cached horizon oscillates between ~2/3 × LEASE_DURATION
/// and ~LEASE_DURATION).
const PER_ROW_MIN_VALIDITY: Duration = Duration::from_secs(5);

/// Background sweeper that drives `try_collect` over tombstoned resource rows.
pub struct ResourceGcController {
    store: Arc<dyn ResourceStore>,
    election: LeaderElection,
    schedule: GlobalSchedule,
    settings: ResourceGcSettings,
}

impl ResourceGcController {
    /// Run the resource GC under a leader election until `shutdown` is
    /// cancelled, releasing the lease on exit so a peer replica can take over
    /// promptly instead of waiting out the lease TTL (default 60s).
    pub async fn run(
        pool: PgPool,
        store: Arc<dyn ResourceStore>,
        settings: ResourceGcSettings,
        shutdown: CancellationToken,
    ) -> anyhow::Result<()> {
        let schedule_pool = pool.clone();
        with_leader_election(
            pool,
            LEASE_NAME,
            Uuid::new_v4(),
            LEASE_DURATION,
            shutdown.clone(),
            move |election| async move {
                let controller = Arc::new(ResourceGcController {
                    store,
                    election,
                    schedule: GlobalSchedule::new(
                        schedule_pool,
                        SCHEDULE_NAME,
                        Duration::from_secs(settings.interval_secs),
                    ),
                    settings,
                });
                controller.supervise(shutdown).await;
                Ok(())
            },
        )
        .await
    }

    /// Supervise the sweep loop: if `gc_loop` panics the supervisor logs it and
    /// respawns it after a short backoff — the classic Erlang/OTP "let it crash"
    /// pattern, so a single poisoned row cannot silently stop garbage collection
    /// (which would leave tombstoned rows accumulating forever while `/health`
    /// keeps returning OK). The election outlives respawns; only the sweep loop
    /// restarts. Returns once `shutdown` is cancelled so the caller releases the
    /// lease.
    async fn supervise(self: Arc<Self>, shutdown: CancellationToken) {
        loop {
            if shutdown.is_cancelled() {
                break;
            }
            let me = Arc::clone(&self);
            let loop_shutdown = shutdown.clone();
            let handle = tokio::spawn(async move { me.gc_loop(loop_shutdown).await });
            match handle.await {
                Ok(()) => {
                    info!("resource GC loop exited");
                    break;
                }
                Err(e) if e.is_panic() => {
                    error!(error = ?e, "resource GC loop panicked; restarting after 5s");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
                Err(e) => {
                    info!(error = ?e, "resource GC loop cancelled");
                    break;
                }
            }
        }
    }

    async fn gc_loop(&self, shutdown: CancellationToken) {
        info!(
            interval_secs = self.settings.interval_secs,
            batch_size = self.settings.batch_size,
            max_batches_per_tick = self.settings.max_batches_per_tick,
            "Resource GC loop started"
        );
        let mut ticker = interval(Duration::from_secs(self.settings.interval_secs));
        // Default `Burst` would fire queued ticks back-to-back after a long
        // sweep, defeating the cadence guarantee. `Delay` re-anchors each tick
        // off the moment we finish waiting, so cadence is honored even when
        // sweeps occasionally run long.
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let mut consecutive_failures: u32 = 0;
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = ticker.tick() => {}
            }
            if !self.election.is_leader() {
                continue;
            }
            // Global cadence gate: prevents leader-transition bursts where
            // the new leader's first tick fires sooner than the old leader's
            // next-scheduled sweep would have. `_as_leader` re-verifies
            // leadership against the DB (fast-path on the cached lease
            // horizon) before the schedule UPSERT, so a stale-cached
            // non-leader cannot poison `last_run_at`.
            if !self
                .schedule
                .try_claim_or_skip_as_leader("resource GC", &self.election)
                .await
            {
                continue;
            }
            match self.sweep().await {
                Ok(_) => {
                    consecutive_failures = 0;
                }
                Err(e) => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    error!(
                        error = ?e,
                        failures = consecutive_failures,
                        "resource GC sweep failed"
                    );
                    // 2, 4, 8, 16, 32, 60, 60 ... — capped at 60s after the
                    // 6th consecutive failure. Recovery still happens within
                    // ~1 minute of the dependency returning, while the cap keeps
                    // a sustained outage to ~1 log line/minute.
                    let exp = consecutive_failures.min(6);
                    let backoff_secs = std::cmp::min(60u64, 2u64.saturating_pow(exp));
                    tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                }
            }
        }
    }

    /// Run a single sweep across up to `max_batches_per_tick` batches. Each
    /// batch invokes `try_collect` once per row and isolates per-row errors so
    /// a single failure cannot stall the loop.
    ///
    /// Leadership is re-verified against the DB immediately before every
    /// destructive `try_collect` call (see `process_row`). `try_collect` may
    /// hard-delete rows; without a per-row DB-backed check, a stale-leader
    /// replica that lost its lease mid-batch could keep deleting alongside
    /// the new leader (split-brain). The per-row check follows the same
    /// pattern as `project::controller`, `ecr::controller`, and
    /// `snowflake_oauth::controller`.
    ///
    /// Forward-progress guard: the SQL feed is `ORDER BY deletion_timestamp
    /// ASC LIMIT N` with no cursor. If the first N rows are stuck
    /// (controller finalizer never clears) `try_collect` returns
    /// `MarkedForDeletion` without modifying `deletion_timestamp`, so every
    /// batch would otherwise refetch the same rows and never see newer
    /// tombstones at the tail. We track attempted UIDs for the duration of
    /// the sweep and grow the LIMIT to step past them, taking the first
    /// `batch_size` unattempted rows from each response.
    async fn sweep(&self) -> anyhow::Result<SweepStats> {
        let stuck_threshold = Duration::from_secs(self.settings.stuck_threshold_secs);
        let now = Utc::now();
        let mut stats = SweepStats::default();
        let mut attempted: HashSet<Uuid> = HashSet::new();

        'outer: for _ in 0..self.settings.max_batches_per_tick {
            // Cheap fast-fail across batches; `process_row` does the
            // authoritative per-row check. `ensure_leader_for` short-circuits
            // to a local-clock check when the cached lease horizon already
            // covers `PER_ROW_MIN_VALIDITY` — the steady-state path costs
            // zero DB calls. When the horizon is insufficient it falls back
            // to a `verify_leader` round-trip with the same DB-blip-tolerant
            // semantics as before.
            match self.election.ensure_leader_for(PER_ROW_MIN_VALIDITY).await {
                Ok(LeaderStatus::Leader) => {}
                Ok(LeaderStatus::NotLeader) => {
                    warn!("lost leader lease between batches; aborting sweep");
                    break;
                }
                Err(LeaseError::Db(e)) => {
                    if !self.election.is_leader() {
                        warn!(error = ?e, "leader verification failed and cached flag is false; aborting sweep");
                        break;
                    }
                    warn!(error = ?e, "leader verification DB blip between batches; falling back to cached flag and continuing");
                }
                Err(e @ LeaseError::InvalidMinValidity { .. }) => {
                    // Programmer error: PER_ROW_MIN_VALIDITY must fit inside
                    // LEASE_DURATION / 2. Surface loudly so it gets fixed
                    // before deploy — the sweep can't safely proceed.
                    error!(error = ?e, "ensure_leader_for misconfigured; aborting sweep");
                    break;
                }
            }

            let limit = self
                .settings
                .batch_size
                .saturating_add(attempted.len() as i64);
            let batch = self.store.list_pending_collection(limit).await?;
            let fresh: Vec<&ResourceRow> = batch
                .iter()
                .filter(|r| !attempted.contains(&r.uid))
                .take(self.settings.batch_size as usize)
                .collect();

            // Truthful "no work" signal: a raw `batch.len() < batch_size`
            // check would lie once `attempted` starts hiding head rows.
            if fresh.is_empty() {
                break;
            }

            for row in fresh {
                attempted.insert(row.uid);
                match self
                    .process_row(row, now, stuck_threshold, &mut stats)
                    .await
                {
                    ProcessOutcome::Continue => {}
                    ProcessOutcome::LostLeadership => break 'outer,
                }
            }
        }

        if stats.has_activity() {
            debug!(
                collected = stats.collected,
                still_pending = stats.still_pending,
                already_gone = stats.already_gone,
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
    ) -> ProcessOutcome {
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

        // Re-verify leadership immediately before the destructive call.
        // `ensure_leader_for` short-circuits via the cached lease horizon
        // (no DB hit) when the heartbeat has confirmed at least
        // `PER_ROW_MIN_VALIDITY` of remaining validity; otherwise it falls
        // back to a `verify_leader` round-trip with the same DB-blip-tolerant
        // semantics as the sibling controllers (project/ecr/snowflake_oauth).
        match self.election.ensure_leader_for(PER_ROW_MIN_VALIDITY).await {
            Ok(LeaderStatus::Leader) => {}
            Ok(LeaderStatus::NotLeader) => {
                warn!("lost leader lease before try_collect; aborting sweep");
                return ProcessOutcome::LostLeadership;
            }
            Err(LeaseError::Db(e)) => {
                if !self.election.is_leader() {
                    warn!(error = ?e, "leader verification failed and cached flag is false; aborting sweep");
                    return ProcessOutcome::LostLeadership;
                }
                warn!(error = ?e, "leader verification DB blip before try_collect; falling back to cached flag");
            }
            Err(e @ LeaseError::InvalidMinValidity { .. }) => {
                error!(error = ?e, "ensure_leader_for misconfigured; aborting sweep");
                return ProcessOutcome::LostLeadership;
            }
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
            Err(StoreError::NotFound) => {
                // Row was hard-deleted between `list_pending_collection` and
                // `try_collect` — either by a parallel sweep (shouldn't
                // happen under our leader-elected single-writer model, but
                // is harmless) or by a future code path. Not an error.
                stats.already_gone += 1;
                debug!(uid = %row.uid, "row vanished between list and collect");
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

        ProcessOutcome::Continue
    }
}

/// Per-row processing result. `LostLeadership` short-circuits the entire sweep
/// — the outer loop must not continue issuing destructive writes once the
/// lease check fails.
#[derive(Debug, PartialEq, Eq)]
enum ProcessOutcome {
    Continue,
    LostLeadership,
}

#[derive(Default, Debug, PartialEq, Eq)]
struct SweepStats {
    collected: u64,
    still_pending: u64,
    errors: u64,
    stuck: u64,
    /// Rows that vanished between `list_pending_collection` and `try_collect`
    /// (the store returned `NotFound`). Not an error — see `process_row`.
    already_gone: u64,
}

impl SweepStats {
    fn has_activity(&self) -> bool {
        self.collected + self.still_pending + self.errors + self.stuck + self.already_gone > 0
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
    use rise_resource_api::{
        CollectionInfo, CreateResourceParams, PathSegment, ResourceApi, StoreError,
        UpdateResourceParams,
    };
    use rise_resource_api::{API_VERSION_V1ALPHA1, ORGANIZATION_KIND, RESOURCE_DEFINITION_KIND};
    use rise_resource_store_postgres::PgResourceStore;
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
        }
    }

    /// Build a controller with its own election for unit tests that drive
    /// `sweep()` directly. Production goes through [`ResourceGcController::run`],
    /// which owns the election via `with_leader_election`.
    fn gc_controller(
        pool: PgPool,
        store: Arc<dyn ResourceStore>,
        settings: ResourceGcSettings,
    ) -> ResourceGcController {
        let election =
            LeaderElection::spawn(pool.clone(), LEASE_NAME, Uuid::new_v4(), LEASE_DURATION);
        let schedule = GlobalSchedule::new(
            pool,
            SCHEDULE_NAME,
            Duration::from_secs(settings.interval_secs),
        );
        ResourceGcController {
            store,
            election,
            schedule,
            settings,
        }
    }

    /// Wait until the controller's election task has acquired the lease.
    /// `sweep()` asserts leadership at the start of every batch (and before
    /// every destructive write) via `ensure_leader_for`, so tests that drive
    /// it synchronously must give the spawned election loop a chance to
    /// claim the lease — and publish its lease horizon — first.
    async fn wait_for_leader(gc: &ResourceGcController) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            if gc.election.is_leader() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("resource GC election did not acquire leadership within 5s");
    }

    /// Layer the resource-store and runtime-sync schemas on top of the root
    /// migrations that `#[sqlx::test]` already ran, mirroring the pattern in
    /// `src/server/resources/handlers.rs` tests. The runtime-sync schema is
    /// required because the GC controller runs under a `LeaderElection` and
    /// `GlobalSchedule`, which write to `runtime_sync.leader_leases` /
    /// `runtime_sync.leader_schedules`.
    async fn store_for(pool: PgPool) -> Arc<dyn ResourceStore> {
        rise_resource_store_postgres::run_migrations(&pool)
            .await
            .expect("resource store migrations");
        rise_runtime_sync::run_migrations(&pool)
            .await
            .expect("runtime sync migrations");
        Arc::new(PgResourceStore::new(pool))
    }

    async fn create_org(store: &dyn ResourceStore, name: &str) -> ResourceRow {
        store
            .create(CreateResourceParams {
                labels: Default::default(),
                api_version: API_VERSION_V1ALPHA1.to_string(),
                kind: ORGANIZATION_KIND.to_string(),
                name: name.to_string(),
                parent_uid: None,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                owner_references: vec![],
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
                labels: Default::default(),
                api_version: API_VERSION_V1ALPHA1.to_string(),
                kind: ORGANIZATION_KIND.to_string(),
                name: name.to_string(),
                parent_uid: None,
                annotations: BTreeMap::new(),
                finalizers: vec![CONTROLLER_FINALIZER.to_string()],
                owner_references: vec![],
                spec: json!({"displayName": name}),
                validator: None,
            })
            .await
            .unwrap()
    }

    async fn register_widget_definition(store: &dyn ResourceStore) {
        store
            .register_resource_definition(CreateResourceParams {
                labels: Default::default(),
                api_version: API_VERSION_V1ALPHA1.to_string(),
                kind: RESOURCE_DEFINITION_KIND.to_string(),
                name: "widgets.example.dev".to_string(),
                parent_uid: None,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                owner_references: vec![],
                spec: json!({
                    "group": "example.dev",
                    "kind": "Widget",
                    "plural": "widgets",
                    "parent": {"apiVersion": API_VERSION_V1ALPHA1, "kind": ORGANIZATION_KIND},
                    "versions": [{"name": "v1", "served": true, "storage": true}],
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
                labels: Default::default(),
                api_version: "example.dev/v1".to_string(),
                kind: "Widget".to_string(),
                name: name.to_string(),
                parent_uid: Some(parent),
                annotations: BTreeMap::new(),
                finalizers,
                owner_references: vec![],
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

        let gc = gc_controller(pool.clone(), store.clone(), default_settings());
        wait_for_leader(&gc).await;

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
        let gc = gc_controller(pool.clone(), failing, default_settings());
        wait_for_leader(&gc).await;

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
        let gc = gc_controller(pool.clone(), store.clone(), settings);
        wait_for_leader(&gc).await;

        let stats = gc.sweep().await.unwrap();
        assert!(
            stats.stuck >= 2,
            "both tombstoned rows should be flagged stuck, got {stats:?}"
        );
        Ok(())
    }

    /// Regression test for the forward-progress starvation guard (Fix #1).
    ///
    /// Without the `attempted` set + grown LIMIT, `list_pending_collection`
    /// returns the same N stuck rows each batch and never sees newer tombstones
    /// at the tail of the deterministically sorted feed. With the guard, one
    /// sweep walks past the stuck head and collects the lone unstuck row.
    #[sqlx::test]
    async fn sweep_starvation_guard_collects_tail_past_stuck_head(
        pool: PgPool,
    ) -> sqlx::Result<()> {
        let store = store_for(pool.clone()).await;

        // Create `batch_size` rows that will remain stuck (controller
        // finalizer never clears), then one extra row whose finalizer we
        // clear. Insert sequentially so the unstuck row has the latest
        // creation order — and we explicitly stamp its `deletion_timestamp`
        // last below to put it at the tail of the ASC-ordered feed.
        let mut stuck = Vec::new();
        for i in 0..2 {
            let r = create_org_with_finalizer(&*store, &format!("stuck-{i}")).await;
            store.delete(r.uid).await.unwrap();
            stuck.push(r);
        }
        // Small sleep so the tail row's deletion_timestamp is strictly newer
        // than the stuck rows' timestamps. NOW() has microsecond resolution
        // in Postgres, but the sequential `store.delete` calls above are
        // already serialized — a tiny pause makes the ordering robust even
        // on machines where consecutive NOW() calls collapse to the same
        // tick.
        tokio::time::sleep(Duration::from_millis(10)).await;
        let tail = create_org_with_finalizer(&*store, "tail-unstuck").await;
        store.delete(tail.uid).await.unwrap();
        // Clear the tail's controller finalizer so `try_collect` collects it.
        store
            .update_controller_finalizers(
                tail.uid,
                CONTROLLER_ID,
                &[],
                &[CONTROLLER_FINALIZER.to_string()],
            )
            .await
            .unwrap();

        // batch_size=2 means the first batch of `list_pending_collection`
        // would return only the two stuck rows. Without the starvation
        // guard, every subsequent batch in the same sweep would refetch the
        // same two stuck rows and never reach `tail`. max_batches_per_tick=3
        // gives the guard room to step past the head within one sweep.
        let settings = ResourceGcSettings {
            interval_secs: 1,
            batch_size: 2,
            max_batches_per_tick: 3,
            stuck_threshold_secs: 3600,
        };
        let gc = gc_controller(pool.clone(), store.clone(), settings);
        wait_for_leader(&gc).await;

        let stats = gc.sweep().await.unwrap();
        assert_eq!(stats.errors, 0, "stuck rows are not errors, got {stats:?}");
        assert_eq!(
            stats.collected, 1,
            "the lone unstuck tail row must be collected within ONE sweep, got {stats:?}"
        );
        assert!(
            store.get(tail.uid).await.unwrap().is_none(),
            "tail row should have been hard-deleted"
        );
        for r in &stuck {
            assert!(
                store.get(r.uid).await.unwrap().is_some(),
                "stuck rows should still be tombstoned"
            );
        }
        Ok(())
    }

    /// Regression test for Fix #6: `StoreError::NotFound` from `try_collect`
    /// is the normal outcome when a row was hard-deleted between
    /// `list_pending_collection` and `try_collect` (e.g. a parallel sweep,
    /// or a future code path that hard-deletes directly). It must NOT be
    /// counted as an error.
    #[sqlx::test]
    async fn sweep_treats_not_found_as_already_gone(pool: PgPool) -> sqlx::Result<()> {
        let inner = store_for(pool.clone()).await;

        // Tombstone two rows so `list_pending_collection` finds something.
        let a = create_org_with_finalizer(&*inner, "gone-a").await;
        let b = create_org_with_finalizer(&*inner, "gone-b").await;
        for r in [&a, &b] {
            inner.delete(r.uid).await.unwrap();
        }

        // Decorator that hard-fails every `try_collect` with NotFound,
        // simulating "the row vanished between list and collect".
        let racy: Arc<dyn ResourceStore> = Arc::new(NotFoundStore {
            inner: inner.clone(),
        });
        let gc = gc_controller(pool.clone(), racy, default_settings());
        wait_for_leader(&gc).await;

        let stats = gc.sweep().await.unwrap();
        assert_eq!(
            stats.errors, 0,
            "NotFound must not be counted as an error, got {stats:?}"
        );
        assert!(
            stats.already_gone > 0,
            "at least one row should be classified as already_gone, got {stats:?}"
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
            labels: Default::default(),
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
            owner_references: vec![],
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
    impl ResourceApi for FailingStore {
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
    }

    #[async_trait::async_trait]
    impl ResourceStore for FailingStore {
        async fn ancestors(&self, uid: Uuid) -> Result<Vec<ResourceRow>, StoreError> {
            self.inner.ancestors(uid).await
        }

        async fn label_inheriting_descendants(
            &self,
            uid: Uuid,
            label_key: &str,
        ) -> Result<Vec<ResourceRow>, StoreError> {
            self.inner
                .label_inheriting_descendants(uid, label_key)
                .await
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

        async fn list_deletion_blockers(
            &self,
            uid: Uuid,
        ) -> Result<rise_resource_api::DeletionBlockerReport, StoreError> {
            self.inner.list_deletion_blockers(uid).await
        }

        async fn resolve_path(
            &self,
            segments: &[PathSegment],
        ) -> Result<Vec<ResourceRow>, StoreError> {
            self.inner.resolve_path(segments).await
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

    /// `ResourceStore` decorator that always returns `StoreError::NotFound`
    /// from `try_collect`, simulating the race where a row was hard-deleted
    /// between `list_pending_collection` and `try_collect`. Every other call
    /// passes through unchanged so `list_pending_collection` still surfaces
    /// the tombstoned rows the test inserted.
    struct NotFoundStore {
        inner: Arc<dyn ResourceStore>,
    }

    #[async_trait]
    impl ResourceApi for NotFoundStore {
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
    }

    #[async_trait::async_trait]
    impl ResourceStore for NotFoundStore {
        async fn ancestors(&self, uid: Uuid) -> Result<Vec<ResourceRow>, StoreError> {
            self.inner.ancestors(uid).await
        }

        async fn label_inheriting_descendants(
            &self,
            uid: Uuid,
            label_key: &str,
        ) -> Result<Vec<ResourceRow>, StoreError> {
            self.inner
                .label_inheriting_descendants(uid, label_key)
                .await
        }

        async fn try_collect(&self, _uid: Uuid) -> Result<DeleteOutcome, StoreError> {
            Err(StoreError::NotFound)
        }

        async fn list_pending_collection(
            &self,
            limit: i64,
        ) -> Result<Vec<ResourceRow>, StoreError> {
            self.inner.list_pending_collection(limit).await
        }

        async fn list_deletion_blockers(
            &self,
            uid: Uuid,
        ) -> Result<rise_resource_api::DeletionBlockerReport, StoreError> {
            self.inner.list_deletion_blockers(uid).await
        }

        async fn resolve_path(
            &self,
            segments: &[PathSegment],
        ) -> Result<Vec<ResourceRow>, StoreError> {
            self.inner.resolve_path(segments).await
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
