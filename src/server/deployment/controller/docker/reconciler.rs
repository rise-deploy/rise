//! In-process reconcile loop for the Docker backend.
//!
//! Docker has no Metacontroller, so this loop replicates the webhook's
//! responsibilities against the Docker daemon: status-machine transitions,
//! desired-vs-actual container diffing, GC, HTTP health probing, supersession,
//! and `controller_metadata` snapshots.
//!
//! The diff itself ([`diff_desired_vs_actual`]) is a pure function so it can be
//! unit-tested without a daemon.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use bollard::Docker;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use rise_backend_auth::{RiseTokenSigner, WorkloadSubjectInfo};
use rise_runtime_sync::{
    with_leader_election, LeaderElection, LeaderStatus, LeaseError, LEASE_DURATION,
};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use super::container_builder::{self, BuilderConfig, DesiredContainer, DesiredRoute};
use super::diff::{
    diff_desired_vs_actual, identity_key, ActualContainer, InspectedContainer, ReconcileAction,
};
use super::env::{hash_env, merge_container_env, pin_system_env};
use super::health::{effective_health_path, probe_error_detail};
use super::labels::{self, SUFFIX_ENV_HASH, SUFFIX_IMAGE, SUFFIX_MANAGED_BY};
use super::pod_status::build_controller_metadata;
use super::rolling::{
    filter_rolling_actions, replica_ready, routable_for, service_names_for_spec, ReadyVerdict,
};
use crate::db::models::{Deployment, DeploymentStatus, Project, TerminationReason};
use crate::server::deployment::models::rise_system_env_vars;
use crate::server::deployment::resource_builder::ResourceBuilder;
use crate::server::deployment::state_machine;
use crate::server::deployment::webhook::{
    resolve_deployment_env_vars, resolve_runtime_containers, should_have_infrastructure,
    DEPLOYING_TIMEOUT_MINUTES, PRE_PUSHED_TIMEOUT_MINUTES,
};
use crate::server::encryption::EncryptionProvider;
use crate::server::registry::RegistryProvider;
use crate::server::workload_tokens::{sha256_hex, workload_subject, NO_ENVIRONMENT};

use crate::db::custom_domains as db_custom_domains;
use crate::db::deployments as db_deployments;
use crate::db::env_vars as db_env_vars;
use crate::db::environments as db_environments;
use crate::db::projects as db_projects;

/// Controller HARD cap on the number of replicas the Docker backend will run for
/// a single container spec. A single-host daemon can run many containers behind
/// one Traefik LB service, but we cap it to avoid a runaway spec exhausting the
/// host; requests above this are clamped (with a warning).
///
/// This is distinct from the user-facing, configurable
/// `deployment_constraints.max_replicas` (the operator-set per-deployment limit
/// enforced at request time): `MAX_REPLICAS` is the controller's unconditional
/// backstop and is always `>=` any sane configured limit.
const MAX_REPLICAS: u32 = 50;

/// Minimum lease validity required before a project's destructive Docker
/// operations (container create/remove/recreate). Re-verified per project via
/// `election.ensure_leader_for` so a partitioned former leader can't race a new
/// leader against the shared Docker daemon for up to the lease TTL. Mirrors the
/// resource-GC worker's `PER_ROW_MIN_VALIDITY`: comfortably above one project's
/// apply cost, well below the `LEASE_DURATION / 2` ceiling, so the cached-horizon
/// fast path (zero DB cost) succeeds essentially always while heartbeats are
/// healthy.
const PER_PROJECT_MIN_VALIDITY: std::time::Duration = std::time::Duration::from_secs(5);

/// Owned controller configuration the reconciler carries.
#[derive(Clone)]
pub struct ReconcilerConfig {
    pub controller_class: String,
    pub label_namespace: String,
    pub container_prefix: String,
    pub traefik_network: String,
    pub traefik_entrypoint: String,
    pub traefik_certresolver: Option<String>,
    pub reconcile_interval_secs: u64,
    pub health_path: String,
    pub public_url: String,
    /// Internal URL Traefik uses to reach the Rise backend for the forwardAuth
    /// subrequest (e.g. `http://rise:3000`). Empty disables forwardAuth.
    pub auth_backend_url: String,
    /// Access-class name → access requirement, derived from the configured
    /// `access_classes`. The container builder reads this to decide whether to
    /// stamp Traefik forwardAuth middleware labels for a project's access class.
    pub access_classes: HashMap<String, crate::server::settings::AccessRequirement>,
    /// **LOCAL-DEV ONLY.** Hostname(s) to alias to `app_backend_ip` via
    /// `extra_hosts` on each managed app container, so apps can reach the public
    /// issuer host (e.g. `rise.localhost`) at the Rise backend. Empty in
    /// production (public DNS + Traefik handle it). See the `Docker` settings
    /// variant's `app_backend_host_aliases`.
    pub app_backend_host_aliases: Vec<String>,
    /// The Rise backend's resolved IP on `traefik_network`, captured once at
    /// startup. `None` (or empty aliases) disables `extra_hosts` injection.
    /// Staleness caveat: if the backend container restarts and gets a new IP,
    /// existing app containers keep the old alias entry until recreated — fine
    /// for local dev (see `container_builder::build_container`).
    pub app_backend_ip: Option<String>,
    /// **Dev-only.** Publish each routable app container's HTTP port to a random
    /// `127.0.0.1` host port so a host-run backend (Docker Desktop, where
    /// container bridge IPs aren't routable) can health-probe the app directly.
    /// When set, the reconciler probes the published loopback port instead of
    /// the container's network IP. Off in production. See the `Docker` settings
    /// variant's `publish_app_ports`.
    pub publish_app_ports: bool,
    /// Traffic-cutover strategy for the group. Forwarded into [`BuilderConfig`]
    /// so the container builder gates the Traefik health-check labels on
    /// `HealthRolling`, AND consumed by the reconciler: in `HealthRolling` mode
    /// the Deploying→Healthy readiness signal is whether each container's server
    /// is actually in Traefik's rotation (per `serverStatus`), so the old active
    /// deployment is retired only once the new servers are confirmed UP. See the
    /// `Docker` settings variant's `cutover_strategy`.
    pub cutover_strategy: crate::server::settings::CutoverStrategy,
    /// Base URL of Traefik's API (e.g. `http://rise-traefik:8080`), optionally
    /// with embedded basic-auth userinfo. In `HealthRolling` mode the reconciler
    /// reads the top-level `serverStatus` map from Traefik (falling back to a
    /// `loadBalancer.serverStatus` nesting for version tolerance) to learn
    /// whether a container's server is actually in Traefik's rotation (UP) before
    /// retiring the prior active deployment. `None` (unset) → fall back to Rise's
    /// own in-process health probe as the in-rotation proxy. See the `Docker`
    /// settings variant's `traefik_api_url`.
    pub traefik_api_url: Option<String>,
}

/// Background reconciler. Holds everything needed to converge Docker state with
/// the DB on each tick.
pub struct DockerReconciler {
    docker: Docker,
    db_pool: PgPool,
    resource_builder: Arc<ResourceBuilder>,
    registry_provider: Arc<dyn RegistryProvider>,
    encryption_provider: Option<Arc<dyn EncryptionProvider>>,
    /// Resource store used to read each project's owning Organization so the
    /// reconciler only ever touches projects whose `deploymentControllerClass`
    /// matches `config.controller_class` (mirrors the K8s webhook).
    resource_store: Arc<dyn rise_resource_store::ResourceStore>,
    /// HTTP client reused across health probes (built once, not per-probe).
    http_client: reqwest::Client,
    /// Traefik API client for the `HealthRolling` in-rotation signal. `None`
    /// when `config.traefik_api_url` is unset (or unparseable) → the reconciler
    /// falls back to Rise's own probe as the in-rotation proxy.
    traefik_api: Option<super::traefik_api::TraefikApiClient>,
    /// Signs the per-audience workload JWTs delivered as `[identity]` token files
    /// (and the credential-backed token-exchange endpoint uses the same signer).
    jwt_signer: Arc<RiseTokenSigner>,
    /// TTL for auto-minted `[identity]` workload tokens. Tokens are refreshed
    /// in-place once more than half this TTL has elapsed (parity with the K8s
    /// webhook's re-mint-at-50% behavior).
    identity_token_ttl_seconds: u64,
    /// Per-deployment timestamp of the last in-place token refresh, keyed by the
    /// globally-unique deployment UUID (`deployment.id.to_string()`, NOT the
    /// second-granular `deployment_id`, which can collide). Ephemeral (lost on
    /// restart, which only triggers one extra refresh) and only ever touched on
    /// the single leader's sequential tick.
    identity_refresh: Mutex<HashMap<String, DateTime<Utc>>>,
    config: ReconcilerConfig,
}

/// Workload-identity material prepared once per deployment per tick and delivered
/// into each of the deployment's containers right after create (before start).
#[derive(Clone)]
struct PreparedIdentity {
    /// The deployment's stable bootstrap credential (recovered from an existing
    /// container, or freshly generated for a brand-new deployment).
    credential: String,
    /// Freshly minted per-audience workload tokens (filename → JWT). Empty when
    /// the deployment declares no `[identity].audiences`.
    tokens: BTreeMap<String, String>,
    /// Whether `credential` was recovered (read back) from an existing container
    /// rather than freshly generated. The hash of a read-back credential is
    /// already authoritative (some container is running it), so it is persisted
    /// up front; a freshly-generated credential's hash is persisted only AFTER
    /// it has actually been delivered to a started container, so a failed create
    /// can never leave a persisted hash for a credential no container holds
    /// (mirroring the K8s webhook's observed-only persistence).
    read_back: bool,
}

impl DockerReconciler {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        docker: Docker,
        db_pool: PgPool,
        resource_builder: Arc<ResourceBuilder>,
        registry_provider: Arc<dyn RegistryProvider>,
        encryption_provider: Option<Arc<dyn EncryptionProvider>>,
        resource_store: Arc<dyn rise_resource_store::ResourceStore>,
        jwt_signer: Arc<RiseTokenSigner>,
        identity_token_ttl_seconds: u64,
        config: ReconcilerConfig,
    ) -> Self {
        // Built once and reused; on the (unlikely) builder failure fall back to
        // a default client so the reconciler still functions.
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap_or_else(|e| {
                warn!(
                    "Failed to build health probe client, using default: {:?}",
                    e
                );
                reqwest::Client::new()
            });
        // Built once from the configured Traefik API URL (if any). `None` → the
        // reconciler falls back to Rise's own probe for the in-rotation signal.
        let traefik_api = config
            .traefik_api_url
            .as_deref()
            .and_then(super::traefik_api::TraefikApiClient::new);
        Self {
            docker,
            db_pool,
            resource_builder,
            registry_provider,
            encryption_provider,
            resource_store,
            http_client,
            traefik_api,
            jwt_signer,
            identity_token_ttl_seconds,
            identity_refresh: Mutex::new(HashMap::new()),
            config,
        }
    }

    /// Lease name for the Docker reconciler's leader election. Each tick runs
    /// destructive Docker operations (container create/remove, route changes),
    /// so — like every other in-process destructive loop (resource GC, project,
    /// ECR) — only the elected leader may act. In the single-instance standalone
    /// default this replica is always the leader, so behavior is unchanged.
    const LEASE_NAME: &'static str = "docker-deployment-reconciler";

    /// Run the reconcile loop under a leader election until `shutdown` is
    /// cancelled, releasing the lease on exit so a peer replica can take over
    /// promptly instead of waiting out the lease TTL.
    ///
    /// The per-tick destructive work ([`tick`], which drives `reconcile_project`
    /// / `apply_actions`) is gated on leadership: a non-leader replica skips the
    /// tick entirely (logged at debug) and never touches the Docker daemon, and
    /// leadership is RE-VERIFIED against the DB before each project's destructive
    /// actions (see `reconcile_project`) so a partitioned former leader can't
    /// race the new leader on the shared daemon for up to the lease TTL.
    ///
    /// Unlike the resource-GC worker, this loop is NOT panic-supervised: there is
    /// no inner respawn-after-backoff. It relies on `tick()` being panic-free —
    /// per-tick and per-project errors are returned as `Result` / isolated and
    /// logged, never `panic!`/`unwrap` on fallible work — so the loop body cannot
    /// unwind. (We intentionally cite GC only for the leader-election pattern, not
    /// for panic supervision, to avoid overstating the parity.)
    pub async fn run(self, shutdown: CancellationToken) -> Result<()> {
        let pool = self.db_pool.clone();
        with_leader_election(
            pool,
            Self::LEASE_NAME,
            Uuid::new_v4(),
            LEASE_DURATION,
            shutdown.clone(),
            move |election| async move {
                self.reconcile_loop(election, shutdown).await;
                Ok(())
            },
        )
        .await
    }

    /// Drive the periodic reconcile loop, gating the destructive tick behind
    /// `election.is_leader()`. Returns once `shutdown` is cancelled so the
    /// caller (`with_leader_election`) releases the lease.
    ///
    /// Single-instance behavior is unchanged in steady state (the lone replica is
    /// always leader). The one observable difference after a restart: the loop
    /// sleeps `reconcile_interval_secs` before the first tick (the standard
    /// cadence), so the first reconcile is deferred by up to one interval rather
    /// than running immediately on startup.
    async fn reconcile_loop(&self, election: LeaderElection, shutdown: CancellationToken) {
        let interval = std::time::Duration::from_secs(self.config.reconcile_interval_secs.max(1));
        info!(
            interval_secs = interval.as_secs(),
            controller_class = %self.config.controller_class,
            "Docker reconciler started"
        );
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tokio::time::sleep(interval) => {}
            }
            // Only the leader runs the destructive reconcile. A non-leader
            // replica skips the tick so it never races the leader on the shared
            // Docker daemon. In the single-instance default this is always true.
            // (The cheap cached `is_leader()` is a fast pre-filter; leadership is
            // re-verified against the DB per project before any destructive op.)
            if !election.is_leader() {
                debug!("Not the Docker reconciler leader; skipping tick");
                continue;
            }
            if let Err(e) = self.tick(&election).await {
                error!("Docker reconcile tick failed: {:?}", e);
            }
        }
    }

    /// Spawn [`run`](Self::run) on the current Tokio runtime, returning the
    /// task handle so the caller can await graceful lease release on shutdown.
    pub fn spawn(self, shutdown: CancellationToken) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            if let Err(e) = self.run(shutdown).await {
                error!("Docker reconciler exited with error: {:?}", e);
            }
        })
    }

    /// Run a single reconcile pass over the projects this controller owns.
    /// Errors are isolated per project so one bad project can't stall the rest.
    ///
    /// Projects whose owning Organization's `deploymentControllerClass` does
    /// not match `config.controller_class` belong to a different controller and
    /// are skipped entirely — neither reconciled nor GC'd. This mirrors the K8s
    /// webhook's `enforce_controller_class`.
    async fn tick(&self, election: &LeaderElection) -> Result<()> {
        let projects = db_projects::list(&self.db_pool, None).await?;
        // Memoize org_uid → controller-class for the duration of this tick so N
        // projects sharing one Organization don't each trigger a store read.
        let mut org_class_cache: HashMap<uuid::Uuid, Option<String>> = HashMap::new();
        for project in projects {
            match self.owns_project(&project, &mut org_class_cache).await {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    warn!(
                        project = %project.name,
                        "Failed to resolve controller class, skipping project: {:?}", e
                    );
                    continue;
                }
            }
            // Re-verify leadership against the DB immediately before this
            // project's destructive reconcile. A partitioned former leader can
            // otherwise keep mutating the shared Docker daemon (create/remove)
            // alongside the new leader for up to the lease TTL. `ensure_leader_for`
            // short-circuits to a local-clock check when the cached lease horizon
            // already covers `PER_PROJECT_MIN_VALIDITY` (zero DB cost in steady
            // state); otherwise it falls back to a `verify_leader` DB round-trip.
            // A DB-verification FAILURE fails safe (skips the tick) — see
            // `confirm_leadership` — because the shared daemon stays reachable
            // during a DB partition. On confirmed loss (or a DB error) we ABORT
            // the remainder of the tick rather than skipping just this project —
            // the lease is global, so once it's gone no further project may act.
            if !self.confirm_leadership(election).await {
                return Ok(());
            }
            if let Err(e) = self.reconcile_project(&project, election).await {
                error!(project = %project.name, "Failed to reconcile project: {:?}", e);
            }
        }
        // Drop identity-refresh bookkeeping for deployments that have aged out, so
        // the in-memory map stays bounded over the controller's lifetime.
        self.prune_identity_refresh();
        Ok(())
    }

    /// Re-verify leadership before a project's destructive work, mirroring the
    /// resource-GC worker's per-row `ensure_leader_for` check. Returns `true` to
    /// proceed, `false` to SKIP the destructive work (abort the rest of the tick).
    ///
    /// Unlike the sibling controllers, a DB-verification failure here FAILS SAFE
    /// by returning `false` (skip the tick) rather than falling back to the cached
    /// `is_leader()` flag. The Docker daemon is reachable during a DB-only
    /// partition, so a partitioned former leader trusting its stale cached flag
    /// would mutate the SAME daemon a DB-reachable peer is legitimately driving —
    /// a split-brain. (The GC sibling's destructive op is itself a Postgres write,
    /// unreachable during the partition, so its cached fallback is harmless; that
    /// safety does not transfer to a reachable daemon.) Daemon ops are reversible
    /// and retried next tick, so skipping on a DB blip is the correct, safe choice.
    async fn confirm_leadership(&self, election: &LeaderElection) -> bool {
        match election.ensure_leader_for(PER_PROJECT_MIN_VALIDITY).await {
            Ok(LeaderStatus::Leader) => true,
            Ok(LeaderStatus::NotLeader) => {
                warn!("Lost Docker reconciler leader lease mid-tick; aborting remaining projects");
                false
            }
            Err(LeaseError::Db(e)) => {
                // FAIL SAFE: do NOT trust the cached `is_leader()` flag. A
                // partitioned former leader keeps it set during a DB-only
                // partition while a DB-reachable peer takes the lease; trusting
                // it would race both against the shared, still-reachable daemon.
                // Skip the tick — daemon ops are reversible and retried next tick.
                warn!(
                    error = ?e,
                    "Leader verification DB error; failing safe and skipping the tick \
                     (the shared Docker daemon stays reachable during a DB partition, so \
                     trusting a stale cached leader flag could split-brain the daemon)"
                );
                false
            }
            Err(e @ LeaseError::InvalidMinValidity { .. }) => {
                // Programmer error: PER_PROJECT_MIN_VALIDITY must fit inside
                // LEASE_DURATION / 2. Surface loudly and abort the tick — we can't
                // safely confirm leadership.
                error!(error = ?e, "ensure_leader_for misconfigured; aborting tick");
                false
            }
        }
    }

    /// Whether this controller owns `project`, i.e. the project's Organization's
    /// `deploymentControllerClass` matches `config.controller_class`. Reuses the
    /// same DB linkage helper and resource store the K8s webhook reads.
    async fn owns_project(
        &self,
        project: &Project,
        org_class_cache: &mut HashMap<uuid::Uuid, Option<String>>,
    ) -> Result<bool> {
        let org_uid =
            crate::db::organization_links::organization_uid_for_project(&self.db_pool, project.id)
                .await?;
        let Some(org_uid) = org_uid else {
            // No Organization linkage — bootstrap should have backfilled this.
            // Treat as not-owned so we never GC containers for an unlinked
            // project we can't attribute to this controller.
            warn!(
                project = %project.name,
                "Project has no organization linkage; skipping in Docker reconciler"
            );
            return Ok(false);
        };
        let org_class = self
            .resolve_org_controller_class(org_uid, org_class_cache)
            .await?;
        Ok(controller_class_matches(
            &self.config.controller_class,
            org_class.as_deref(),
        ))
    }

    /// Read the Organization's `spec.deploymentControllerClass` from the
    /// resource store, memoized per tick via `org_class_cache` so multiple
    /// projects sharing one Organization only hit the store once.
    async fn resolve_org_controller_class(
        &self,
        org_uid: uuid::Uuid,
        org_class_cache: &mut HashMap<uuid::Uuid, Option<String>>,
    ) -> Result<Option<String>> {
        if let Some(cached) = org_class_cache.get(&org_uid) {
            return Ok(cached.clone());
        }
        let row = self
            .resource_store
            .get(org_uid)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to load Organization {org_uid}: {e}"))?
            .ok_or_else(|| anyhow::anyhow!("Organization {org_uid} is missing"))?;
        let spec: rise_resource_api::OrganizationSpec = serde_json::from_value(row.spec.clone())
            .map_err(|e| anyhow::anyhow!("Organization {org_uid} has malformed spec: {e}"))?;
        let class = spec.deployment_controller_class;
        org_class_cache.insert(org_uid, class.clone());
        Ok(class)
    }

    async fn reconcile_project(&self, project: &Project, election: &LeaderElection) -> Result<()> {
        let non_terminal =
            db_deployments::list_non_terminal_for_project(&self.db_pool, project.id).await?;

        // 1. Status transitions (port of webhook::perform_status_transitions).
        // Isolate per-deployment so one transient DB-write error can't abort the
        // whole project tick — matching the desired-compute and health loops.
        for deployment in &non_terminal {
            if let Err(e) = self.perform_status_transition(project, deployment).await {
                warn!(
                    deployment_id = %deployment.deployment_id,
                    "Status transition failed: {:?}", e
                );
            }
        }
        // Re-read after transitions so the desired set reflects status changes
        // (e.g. Pushed → Deploying).
        let non_terminal =
            db_deployments::list_non_terminal_for_project(&self.db_pool, project.id).await?;

        // 2. Compute desired containers across all infra-bearing deployments.
        // Track deployments whose desired computation FAILED this tick: their
        // existing containers must be protected from GC so a transient error
        // (e.g. a brief KMS/encryption outage, a failed DB read, or a bad
        // runtime-container deserialization) doesn't tear down a healthy,
        // actively-serving container. This mirrors the K8s/Metacontroller
        // model, which leaves existing children untouched on a failed sync.
        let mut desired: Vec<DesiredContainer> = Vec::new();
        let mut protected_deployment_ids: HashSet<String> = HashSet::new();
        for deployment in &non_terminal {
            if !should_have_infrastructure(deployment) {
                continue;
            }
            match self
                .compute_desired_for_deployment(project, deployment)
                .await
            {
                Ok(mut containers) => desired.append(&mut containers),
                Err(e) => {
                    warn!(
                        deployment_id = %deployment.deployment_id,
                        "Failed to compute desired containers; protecting its existing \
                         containers from GC this tick: {:?}",
                        e
                    );
                    protected_deployment_ids.insert(deployment.deployment_id.clone());
                }
            }
        }

        // 3. Enumerate actual Rise containers for this project, diff, throttle,
        // apply. The throttle enforces ROLLING recreate for replicas>1: a running
        // drifted replica is recreated one-at-a-time, only while its siblings are
        // healthy, so a rollout never drops more than one replica of capacity.
        let actual = self.list_actual_containers(project).await?;
        let actions = diff_desired_vs_actual(
            &desired,
            &actual,
            &self.config.container_prefix,
            &protected_deployment_ids,
        );
        // Probe each running replica's HTTP health once for the rolling gate.
        // Skipped entirely when there are no rollout (running+drifted) recreates,
        // so single-replica deployments and steady state pay no probe cost.
        let actions = if actions
            .iter()
            .any(|a| matches!(a, ReconcileAction::Recreate { .. }))
        {
            let healthy_by_identity = self.probe_health_by_identity(&desired, &actual).await;
            filter_rolling_actions(actions, &actual, &healthy_by_identity)
        } else {
            actions
        };
        // Prepare workload-identity material for every deployment that has a
        // Create/Recreate action this tick: resolve its stable bootstrap
        // credential (recovered from an existing container or freshly generated),
        // persist the credential hash so the token-exchange endpoint can
        // authenticate the app, and mint its per-audience tokens. The result is
        // delivered into each new container right after create (before start).
        // Computed BEFORE apply so the credential can be read back from the
        // soon-to-be-replaced container while it still exists. Keyed by the
        // globally-unique deployment UUID (NOT the second-granular `deployment_id`,
        // which can collide across same-second deployments).
        let identity_uuids = deployment_uuids_needing_identity(&actions, &desired);
        let prepared_identity = self
            .prepare_identities(project, &non_terminal, &actual, &identity_uuids)
            .await;

        // Re-verify leadership immediately before the Docker-daemon-destructive
        // apply: the desired computation + health probing above can take a while,
        // so the lease may have lapsed since the per-project check in `tick`. If
        // it can't be confirmed, skip the apply for this project (the health pass
        // below is read-only).
        if !self.confirm_leadership(election).await {
            return Ok(());
        }
        self.apply_actions(project, &desired, &actions, &prepared_identity, election)
            .await?;

        // Re-establish identity for any running deployment whose credential never
        // got persisted (a prior tick's create failed mid-way), then refresh
        // auto-minted `[identity]` token files in place for long-running
        // containers whose tokens are past half their TTL — parity with the K8s
        // webhook, which re-establishes/re-mints on every sync. Both are
        // daemon-destructive (they re-upload files), so they are leader-gated like
        // apply.
        if self.confirm_leadership(election).await {
            self.remediate_missing_identity(project, &non_terminal, &actual)
                .await;
            self.refresh_identity_tokens(project, &non_terminal, &actual)
                .await;
        }

        // 4. Health → status (probe routable containers, transition).
        let non_terminal =
            db_deployments::list_non_terminal_for_project(&self.db_pool, project.id).await?;
        // Memoize Traefik `serverStatus` per service for THIS reconcile pass:
        // during a rollout a group has 2+ non-terminal deployments sharing one
        // group-scoped service, so without this each `reconcile_health` would
        // re-fetch the SAME serverStatus (up to 3s each). Populated once per
        // service and reused across the group's deployments this tick.
        let mut server_status_cache: HashMap<String, Option<HashMap<String, bool>>> =
            HashMap::new();
        for deployment in &non_terminal {
            if !should_have_infrastructure(deployment) {
                continue;
            }
            if let Err(e) = self
                .reconcile_health(project, deployment, &mut server_status_cache)
                .await
            {
                warn!(
                    deployment_id = %deployment.deployment_id,
                    "Health reconcile failed: {:?}", e
                );
            }
        }

        Ok(())
    }

    // ── Status transitions ─────────────────────────────────────────────

    async fn perform_status_transition(
        &self,
        project: &Project,
        deployment: &Deployment,
    ) -> Result<()> {
        match deployment.status {
            DeploymentStatus::Pending | DeploymentStatus::Building | DeploymentStatus::Pushing => {
                // The CLI drives these; only time them out.
                let elapsed = Utc::now().signed_duration_since(deployment.created_at);
                if elapsed > chrono::Duration::minutes(PRE_PUSHED_TIMEOUT_MINUTES) {
                    let msg = format!(
                        "Deployment timed out after {} minutes in {} state. \
                         This usually indicates the CLI was interrupted during build/push.",
                        PRE_PUSHED_TIMEOUT_MINUTES, deployment.status
                    );
                    warn!(deployment_id = %deployment.deployment_id, "{}", msg);
                    db_deployments::mark_failed(&self.db_pool, deployment.id, &msg).await?;
                    db_projects::update_calculated_status(&self.db_pool, project.id).await?;
                }
            }
            DeploymentStatus::Cancelling => {
                info!(
                    deployment_id = %deployment.deployment_id,
                    "Cancelling deployment — marking as Cancelled"
                );
                db_deployments::mark_cancelled(&self.db_pool, deployment.id).await?;
                db_projects::update_calculated_status(&self.db_pool, project.id).await?;
            }
            DeploymentStatus::Terminating => {
                self.complete_termination(project, deployment).await?;
            }
            DeploymentStatus::Pushed => {
                info!(
                    deployment_id = %deployment.deployment_id,
                    "Deployment image pushed, transitioning to Deploying"
                );
                db_deployments::update_status(
                    &self.db_pool,
                    deployment.id,
                    DeploymentStatus::Deploying,
                )
                .await?;
                db_projects::update_calculated_status(&self.db_pool, project.id).await?;
            }
            DeploymentStatus::Deploying => {
                if let Some(started) = deployment.deploying_started_at {
                    let elapsed = Utc::now().signed_duration_since(started);
                    if elapsed > chrono::Duration::minutes(DEPLOYING_TIMEOUT_MINUTES) {
                        let msg = format!(
                            "Deployment timed out after {} seconds in Deploying state",
                            elapsed.num_seconds()
                        );
                        warn!(deployment_id = %deployment.deployment_id, "{}", msg);
                        db_deployments::mark_failed(&self.db_pool, deployment.id, &msg).await?;
                        db_projects::update_calculated_status(&self.db_pool, project.id).await?;
                    }
                }
            }
            _ => {}
        }

        // Expiration applies to every non-terminal deployment.
        if let Some(expires_at) = deployment.expires_at {
            if Utc::now() > expires_at
                && !matches!(
                    deployment.status,
                    DeploymentStatus::Terminating | DeploymentStatus::Cancelling
                )
            {
                info!(
                    deployment_id = %deployment.deployment_id,
                    "Deployment has expired, marking as Terminating"
                );
                db_deployments::mark_terminating(
                    &self.db_pool,
                    deployment.id,
                    TerminationReason::Expired,
                )
                .await?;
                db_projects::update_calculated_status(&self.db_pool, project.id).await?;
            }
        }

        Ok(())
    }

    /// Port of `webhook::complete_termination`.
    async fn complete_termination(&self, project: &Project, deployment: &Deployment) -> Result<()> {
        match deployment.termination_reason {
            Some(TerminationReason::Superseded) => {
                db_deployments::mark_superseded(&self.db_pool, deployment.id).await?;
            }
            Some(TerminationReason::UserStopped) => {
                db_deployments::mark_stopped(&self.db_pool, deployment.id).await?;
            }
            Some(TerminationReason::Expired) => {
                db_deployments::mark_expired(&self.db_pool, deployment.id).await?;
            }
            Some(TerminationReason::Failed) => {
                db_deployments::mark_failed(
                    &self.db_pool,
                    deployment.id,
                    deployment
                        .error_message
                        .as_deref()
                        .unwrap_or("Deployment failed"),
                )
                .await?;
            }
            Some(TerminationReason::Cancelled) => {
                db_deployments::mark_cancelled(&self.db_pool, deployment.id).await?;
            }
            None => {
                db_deployments::mark_stopped(&self.db_pool, deployment.id).await?;
            }
        }
        db_projects::update_calculated_status(&self.db_pool, project.id).await?;
        // The container itself is GC'd on the next diff pass (no longer in the
        // desired set once the deployment is terminal).
        Ok(())
    }

    // ── Desired computation ────────────────────────────────────────────

    /// Build the static [`BuilderConfig`] borrowed from this reconciler's
    /// config. Used both to render containers and to compute the desired
    /// `route-hash` for the diff, so the two always agree.
    fn builder_cfg(&self) -> BuilderConfig<'_> {
        BuilderConfig {
            label_namespace: &self.config.label_namespace,
            controller_class: &self.config.controller_class,
            container_prefix: &self.config.container_prefix,
            traefik_network: &self.config.traefik_network,
            traefik_entrypoint: &self.config.traefik_entrypoint,
            traefik_certresolver: self.config.traefik_certresolver.as_deref(),
            auth_backend_url: &self.config.auth_backend_url,
            access_classes: &self.config.access_classes,
            app_backend_host_aliases: &self.config.app_backend_host_aliases,
            app_backend_ip: self.config.app_backend_ip.as_deref(),
            publish_app_ports: self.config.publish_app_ports,
            cutover_strategy: self.config.cutover_strategy,
        }
    }

    async fn compute_desired_for_deployment(
        &self,
        project: &Project,
        deployment: &Deployment,
    ) -> Result<Vec<DesiredContainer>> {
        // Container + route side-data.
        let (container_specs, route_specs) = resolve_runtime_containers(deployment)?;

        // Environment context.
        let environment = if let Some(env_id) = deployment.environment_id {
            db_environments::find_by_id(&self.db_pool, env_id).await?
        } else {
            None
        };
        let env_name = environment.as_ref().map(|e| e.name.clone());
        let all_environments = db_environments::list_for_project(&self.db_pool, project.id).await?;
        let custom_domains =
            db_custom_domains::list_project_custom_domains(&self.db_pool, project.id).await?;

        // URLs (drives Traefik hosts + RISE_* system env vars).
        let urls = self.resource_builder.compute_deployment_urls(
            project,
            deployment,
            environment.as_ref(),
            &all_environments,
            &custom_domains,
        );
        let primary_hosts: Vec<String> = self
            .resource_builder
            .primary_ingress_hosts(
                project,
                &deployment.deployment_group,
                environment.as_ref().filter(|env| {
                    env.primary_deployment_group.as_deref() == Some(&deployment.deployment_group)
                }),
                &custom_domains,
                true,
                &all_environments,
            )
            .into_iter()
            .map(|h| h.host)
            .collect();

        // Env vars: plain + decrypted secret (merged into plain — Docker has no
        // secret concept) + system env vars.
        let raw_env_vars =
            db_env_vars::list_deployment_env_vars(&self.db_pool, deployment.id).await?;
        let resolved =
            resolve_deployment_env_vars(raw_env_vars, self.encryption_provider.as_deref()).await?;

        let mut base_env: Vec<(String, String)> = Vec::new();
        for ev in &resolved.plain_env_vars {
            base_env.push((ev.name.clone(), ev.value.clone().unwrap_or_default()));
        }
        // Secret env vars become plain KEY=VALUE entries (documented caveat:
        // visible in `docker inspect`). They're included in the per-container
        // env hash below alongside plain + system vars so drift in any kind of
        // variable forces recreation.
        for (key, value) in &resolved.secret_env_vars {
            let plain = String::from_utf8_lossy(&value.0).to_string();
            base_env.push((key.clone(), plain));
        }

        let system_env = rise_system_env_vars(
            &self.config.public_url,
            &deployment.deployment_group,
            &urls,
            env_name.as_deref(),
        );

        // Resolve the deployment's base image (used for the synthesised `app`).
        let source_deployment_id =
            if let Some(source_id) = deployment.rolled_back_from_deployment_id {
                db_deployments::find_by_id(&self.db_pool, source_id)
                    .await?
                    .map(|d| d.deployment_id)
            } else {
                None
            };
        let base_image = self.resource_builder.resolve_image(
            project,
            deployment,
            source_deployment_id.as_deref(),
        );

        // Routability depends on the cutover strategy:
        //
        // - `Recreate`: a container exists (so it can be health-probed) for any
        //   infra-bearing deployment, but it is only *routable* when it is the
        //   active deployment for its group — exactly mirroring the K8s path,
        //   which builds the Ingress solely from `is_active` deployments
        //   (`active_by_group` in `webhook.rs`). `is_active` is flipped on by
        //   `mark_as_active`, which runs only after a deployment becomes Healthy.
        //   Without this gate a still-Deploying/Pushed deployment would advertise
        //   the same `Host(...)` rule as the live active one and Traefik would
        //   split production traffic onto the not-yet-healthy container.
        //
        // - `HealthRolling`: always routable. Both the old active and the new
        //   Deploying deployment join the ONE group-scoped Traefik service and
        //   carry its health-check labels immediately; Traefik's per-server health
        //   check drains the old servers as the new ones come UP, so there is no
        //   cutover gap. Making the new deployment routable from the start (rather
        //   than active-gating) also avoids the g1→g2 route-hash churn a recreate
        //   would otherwise cause when `is_active` later flips.
        // A container whose Traefik router would be WITHHELD (auth required but
        // no `auth_backend_url` to wire forwardAuth — a misconfiguration that
        // fails closed in the builder) must also be treated as NOT routable here,
        // so the cutover/readiness path doesn't mark a never-routed container as
        // in-rotation (ready-when-running) and silently report the deploy Healthy.
        // Keeping routability consistent with router emission makes such a deploy
        // surface as not-Healthy instead. Same predicate the builder uses.
        let routable = routable_for(self.config.cutover_strategy, deployment.is_active)
            && !container_builder::router_withheld(
                &project.access_class,
                &self.config.access_classes,
                &self.config.auth_backend_url,
            );

        // Cross-container service discovery: expose each routable sibling's
        // address as `RISE_CONTAINER_HOST__<NAME>=<host>:<port>`, mirroring the
        // K8s convention (`ResourceBuilder::auto_container_host_env_vars`) so a
        // multi-container app's code is portable across backends. The host is the
        // sibling's GROUP-scoped, deployment-id-FREE `group_app_name` (the alias
        // shared across ALL deployments and replicas of the group) — NOT its live
        // `--name` (which carries the deployment id plus a per-recreate `_g{n}`
        // suffix). Every container of that (project, group, container) attaches
        // this same name as its Docker NETWORK ALIAS (see `build_container`), so
        // Docker's embedded DNS resolves it on the shared user-defined network and
        // ROUND-ROBINS across whatever containers currently carry it — the address
        // keeps pointing at the sibling across recreates AND across deployments of
        // the group. Using the deployment-id-free name also keeps the env hash
        // from drifting per recreate/deployment, which would otherwise trigger an
        // infinite recreate loop. Only meaningful with ≥2 containers, so a
        // single-container app doesn't get a pointless self-entry. Each container
        // also sees its own entry (harmless), matching K8s; order follows
        // `container_specs` for deterministic env hashes.
        let injected_hosts: Vec<(String, String)> = if container_specs.len() >= 2 {
            container_specs
                .iter()
                .filter_map(|spec| {
                    let port = spec.port?;
                    let key = format!(
                        "RISE_CONTAINER_HOST__{}",
                        spec.name.to_uppercase().replace('-', "_")
                    );
                    let host = container_builder::group_app_name(
                        &self.config.container_prefix,
                        &project.name,
                        &deployment.deployment_group,
                        &spec.name,
                    );
                    Some((key, format!("{host}:{port}")))
                })
                .collect()
        } else {
            Vec::new()
        };

        // Build N desired containers per spec — one per replica. All replicas of
        // a spec are IDENTICAL except their `replica` index (and thus the live
        // `--name` + the `replica` bookkeeping label): same image, env, env_hash,
        // routes, route_hash, routable, generation seed. They share ONE Traefik
        // router+service (router naming excludes the replica → Traefik
        // load-balances) and ONE replica-free network alias (Docker DNS
        // round-robins). Recreates are rolled one replica at a time by the
        // rolling throttle (`filter_rolling_actions`) before apply.
        let mut out = Vec::with_capacity(container_specs.len());
        for spec in &container_specs {
            // Resolve the replica count, clamped to [1, MAX_REPLICAS]. A
            // single-host Docker daemon can run N containers behind one Traefik
            // LB service, so replicas>1 ARE supported here (unlike before).
            let requested = spec.replicas.unwrap_or(1);
            if requested > MAX_REPLICAS {
                warn!(
                    deployment_id = %deployment.deployment_id,
                    container = %spec.name,
                    requested,
                    max = MAX_REPLICAS,
                    "Requested replicas exceeds MAX_REPLICAS; clamping"
                );
            }
            let replica_count = clamp_replicas(spec.replicas);

            let mut env = merge_container_env(
                &base_env,
                &system_env,
                &injected_hosts,
                spec,
                env_name.as_deref(),
            );
            // Pin the controller's mandatory env: this container's declared
            // PORT, and RISE_CONTAINER (the container's own name) — both
            // overwrite any user value and fold into `env_hash`. RISE_CONTAINER
            // mirrors the Kubernetes builder (`resource_builder::build_container`).
            pin_system_env(&mut env, &spec.name, spec.port);
            // Hash the *entire* final merged env (plain + system + secret), over
            // a deterministically-sorted copy, so drift in any variable forces
            // recreation. PORT is already pinned above so it participates too.
            let env_hash = hash_env(&env);

            // Routes for this container: the route specs targeting it × the
            // project's primary hosts. A single-container app has a `/` route.
            let container_routes: Vec<DesiredRoute> = route_specs
                .iter()
                .filter(|r| r.container == spec.name)
                .map(|r| DesiredRoute {
                    hosts: primary_hosts.clone(),
                    path_prefix: Some(r.path.clone()),
                })
                .collect();

            // Base container (replica 0). All replicas are clones of this with
            // only their `replica` index differing — same image/env/routes, so
            // they share one Traefik service and one DNS alias, and the route-hash
            // (which never depends on the replica) is computed once below.
            let mut base = DesiredContainer {
                project: project.name.clone(),
                access_class: project.access_class.clone(),
                deployment_group: deployment.deployment_group.clone(),
                deployment_id: deployment.deployment_id.clone(),
                deployment_uuid: deployment.id.to_string(),
                container: spec.name.clone(),
                environment: env_name.clone(),
                image: spec.image.clone().unwrap_or_else(|| base_image.clone()),
                port: spec.port,
                cpu: spec.cpu.clone().unwrap_or_else(|| deployment.cpu.clone()),
                memory: spec
                    .memory
                    .clone()
                    .unwrap_or_else(|| deployment.memory.clone()),
                env,
                env_hash,
                routes: container_routes,
                routable,
                // Filled in below once the container is fully described, using
                // the same render path `build_container` stamps on the live
                // container, so the diff's comparison is exact.
                route_hash: String::new(),
                // Seed generation. `compute_desired_for_deployment` can't know
                // the real value (it depends on the live container's current
                // generation), so the diff resolves it before apply: a brand-new
                // slot stays `1`, a recreate bumps to the live `g{n}` + 1.
                generation: 1,
                replica: 0,
                // Effective health path: a port-less worker has no HTTP probe
                // (None → gated on run-state); an HTTP container honors its
                // `health_check` spec (disabled → None). Used by the rolling
                // throttle's health gate.
                health_path: spec
                    .port
                    .and_then(|_| effective_health_path(spec, &self.config.health_path)),
                // Traefik load-balancer health-check timing, carried from the
                // `health_check` spec. Only consumed by the builder when
                // `cutover_strategy == HealthRolling` AND a health path exists.
                health_check_interval_secs: spec
                    .health_check
                    .as_ref()
                    .and_then(|hc| hc.period_seconds),
                health_check_timeout_secs: spec
                    .health_check
                    .as_ref()
                    .and_then(|hc| hc.timeout_seconds),
            };
            // Precompute the route-hash from the same render the builder uses so
            // the diff can detect routing transitions (active↔inactive) that
            // Docker can't apply to a running container's labels in place. The
            // replica index never feeds the route-hash, so one value serves all
            // replicas.
            base.route_hash = container_builder::route_hash_for(&base, &self.builder_cfg());

            // Emit one identical DesiredContainer per replica index 0..N.
            for replica in 0..replica_count {
                let mut desired = base.clone();
                desired.replica = replica;
                out.push(desired);
            }
        }

        Ok(out)
    }

    // ── Actual containers + diff application ────────────────────────────

    async fn list_actual_containers(&self, project: &Project) -> Result<Vec<ActualContainer>> {
        use bollard::container::ListContainersOptions;
        let ns = &self.config.label_namespace;
        let mut filters: HashMap<String, Vec<String>> = HashMap::new();
        // Scope the listing to *this* controller's containers: managed-by=rise,
        // the configured controller-class, and this project. Without the
        // controller-class filter the GC pass could remove containers another
        // Rise controller owns on the same host.
        //
        // NOTE: containers carry the controller-class they were created under as
        // a label. Renaming an Organization's `deploymentControllerClass` (or
        // this controller's configured class) leaves previously-created
        // containers under the old class invisible to this filter — they are
        // neither reconciled nor GC'd and must be cleaned up manually.
        filters.insert(
            "label".to_string(),
            vec![
                format!("{}={}", labels::ns_key(ns, SUFFIX_MANAGED_BY), "rise"),
                format!(
                    "{}={}",
                    labels::ns_key(ns, labels::SUFFIX_CONTROLLER_CLASS),
                    self.config.controller_class
                ),
                format!(
                    "{}={}",
                    labels::ns_key(ns, labels::SUFFIX_PROJECT),
                    project.name
                ),
            ],
        );
        let summaries = self
            .docker
            .list_containers(Some(ListContainersOptions {
                all: true,
                filters,
                ..Default::default()
            }))
            .await?;

        Ok(summaries
            .into_iter()
            .filter_map(|s| {
                let labels = s.labels.unwrap_or_default();
                let name = s
                    .names
                    .and_then(|n| n.first().cloned())
                    .map(|n| n.trim_start_matches('/').to_string())?;
                Some(ActualContainer {
                    id: s.id.unwrap_or_default(),
                    name,
                    project: labels
                        .get(&labels::ns_key(ns, labels::SUFFIX_PROJECT))
                        .cloned(),
                    deployment_group: labels
                        .get(&labels::ns_key(ns, labels::SUFFIX_DEPLOYMENT_GROUP))
                        .cloned(),
                    container: labels
                        .get(&labels::ns_key(ns, labels::SUFFIX_CONTAINER))
                        .cloned(),
                    deployment_id_label: labels
                        .get(&labels::ns_key(ns, labels::SUFFIX_DEPLOYMENT_ID))
                        .cloned(),
                    deployment_uuid_label: labels
                        .get(&labels::ns_key(ns, labels::SUFFIX_DEPLOYMENT_UUID))
                        .cloned(),
                    // Parse the generation label; default 0 (legacy/missing) so
                    // the first recreate of such a container yields generation 1.
                    generation: labels
                        .get(&labels::ns_key(ns, labels::SUFFIX_GENERATION))
                        .and_then(|g| g.parse::<u32>().ok())
                        .unwrap_or(0),
                    // Parse the replica label; default 0 (legacy/missing) so a
                    // pre-replica container maps onto replica 0 of its spec.
                    replica: labels
                        .get(&labels::ns_key(ns, labels::SUFFIX_REPLICA))
                        .and_then(|r| r.parse::<u32>().ok())
                        .unwrap_or(0),
                    image_label: labels.get(&labels::ns_key(ns, SUFFIX_IMAGE)).cloned(),
                    env_hash_label: labels.get(&labels::ns_key(ns, SUFFIX_ENV_HASH)).cloned(),
                    route_hash_label: labels
                        .get(&labels::ns_key(ns, labels::SUFFIX_ROUTE_HASH))
                        .cloned(),
                    // Daemon-reported lifecycle state ("running", "exited",
                    // "created", "dead", …). Used by the diff to restart
                    // containers that exist but aren't running.
                    state: s.state,
                })
            })
            .collect())
    }

    async fn apply_actions(
        &self,
        project: &Project,
        desired: &[DesiredContainer],
        actions: &[ReconcileAction],
        prepared_identity: &HashMap<String, PreparedIdentity>,
        election: &LeaderElection,
    ) -> Result<()> {
        let builder_cfg = self.builder_cfg();
        // Index desired containers by their stable IDENTITY tuple so an action
        // (which carries the identity, not the generation-ful name) can find the
        // desired container regardless of the `_g{n}` name suffix.
        let by_identity: HashMap<String, &DesiredContainer> = desired
            .iter()
            .map(|d| {
                let key = identity_key(
                    &d.project,
                    &d.deployment_group,
                    &d.deployment_id,
                    &d.container,
                    d.replica,
                );
                (key, d)
            })
            .collect();

        for action in actions {
            // Re-verify leadership before EACH destructive action. A single
            // Create/Recreate can cold-`pull_image` for tens of seconds —
            // exceeding `PER_PROJECT_MIN_VALIDITY` — so the lease confirmed before
            // the loop may have lapsed by the time a later action runs. This fails
            // safe (skips on a DB error, per #1) and is zero-cost on the fast path
            // (cached horizon). On loss we STOP applying further actions for this
            // project — the lease is global, so no further daemon op may run.
            if !self.confirm_leadership(election).await {
                return Ok(());
            }
            match action {
                ReconcileAction::Create {
                    identity,
                    name,
                    generation,
                } => {
                    if let Some(d) = by_identity.get(identity.as_str()) {
                        // Stamp the resolved generation on a clone so the created
                        // container's name + `generation` label match the action.
                        let mut d = (*d).clone();
                        d.generation = *generation;
                        let identity = prepared_identity.get(&d.deployment_uuid);
                        // Plain create (no pre-pull) pulls the image itself.
                        if let Err(e) = self
                            .create_container(&d, &builder_cfg, false, identity)
                            .await
                        {
                            error!(
                                project = %project.name,
                                container = %name,
                                "Failed to create container: {:?}", e
                            );
                        }
                    }
                }
                ReconcileAction::Recreate {
                    identity,
                    name,
                    existing_id,
                    generation,
                } => {
                    // The replacement gets a fresh `_g{n}` name (generation + 1),
                    // distinct from the old container's, so in principle a
                    // create-then-swap is now possible. We still pre-pull the
                    // image (the slowest, most failure-prone step) *before*
                    // removing the running container — a pull failure then leaves
                    // the old container in place rather than tearing it down
                    // first.
                    //
                    // TODO(create-then-swap): now that the generation makes the
                    // replacement's name distinct, create the new container,
                    // confirm it started, then remove the old one — eliminating
                    // the outage window entirely.
                    if let Some(d) = by_identity.get(identity.as_str()) {
                        // Stamp the resolved (bumped) generation on a clone so the
                        // replacement's name + `generation` label match the action.
                        let mut d = (*d).clone();
                        d.generation = *generation;
                        if let Err(e) = self.pull_image(&d.image).await {
                            error!(
                                project = %project.name,
                                container = %name,
                                "Failed to pull image for recreate; leaving existing container in place: {:?}", e
                            );
                            continue;
                        }
                        // If the old container can't be removed, do NOT create the
                        // replacement: two same-identity containers would share the
                        // network alias + Traefik router and both serve (incl. the
                        // stale image). Skip the create — symmetric with the
                        // pull-failure guard above — and retry next tick.
                        if self.remove_container(existing_id).await.is_err() {
                            error!(
                                project = %project.name,
                                container = %name,
                                "Failed to remove existing container for recreate; \
                                 skipping create to avoid two same-identity containers"
                            );
                            continue;
                        }
                        // The image was just pulled above, BEFORE the destructive
                        // remove, so skip the pull inside `create_container` — a
                        // transient pull failure must not strand us with the old
                        // container already gone. The image is pulled exactly once.
                        let identity = prepared_identity.get(&d.deployment_uuid);
                        if let Err(e) = self
                            .create_container(&d, &builder_cfg, true, identity)
                            .await
                        {
                            error!(
                                project = %project.name,
                                container = %name,
                                "Failed to recreate container: {:?}", e
                            );
                        }
                    } else {
                        // No desired match (shouldn't happen for Recreate) — GC.
                        // Best-effort: failure is already logged in remove_container.
                        let _ = self.remove_container(existing_id).await;
                    }
                }
                ReconcileAction::Remove { id, name } => {
                    debug!(project = %project.name, container = %name, "GC removing container");
                    // Best-effort GC: failure is logged in remove_container and
                    // retried next tick (the container stays in the desired-absent
                    // set), so we don't propagate the error.
                    let _ = self.remove_container(id).await;
                }
            }
        }
        Ok(())
    }

    /// Create and start one container. When `already_pulled` is `true` the image
    /// pull is skipped — the caller (the Recreate path) pulled it BEFORE the
    /// destructive remove so a transient pull failure can't strand the slot with
    /// the old container already gone; the image is pulled exactly once. The
    /// plain Create path passes `false` so it still pulls here.
    async fn create_container(
        &self,
        desired: &DesiredContainer,
        builder_cfg: &BuilderConfig<'_>,
        already_pulled: bool,
        identity: Option<&PreparedIdentity>,
    ) -> Result<()> {
        use bollard::container::{CreateContainerOptions, StartContainerOptions};

        let built = container_builder::build_container(desired, builder_cfg);

        // Pull the image with registry auth (anonymous → None), unless the caller
        // already pulled it before a destructive remove.
        if !already_pulled {
            self.pull_image(&desired.image).await?;
        }

        self.docker
            .create_container(
                Some(CreateContainerOptions {
                    name: built.name.clone(),
                    platform: None,
                }),
                built.config,
            )
            .await?;

        // Deliver the workload-identity files (bootstrap credential + per-audience
        // tokens) into the created-but-not-yet-started container, so they are
        // present at `/var/run/secrets/rise/identity/` when the app's process
        // first runs. On failure, remove the half-provisioned container and bail
        // so the slot is recreated next tick rather than running without identity.
        if let Some(identity) = identity {
            if let Err(e) = self.deliver_identity(&built.name, identity).await {
                error!(
                    container = %built.name,
                    "Failed to deliver workload-identity files; removing container to retry: {:?}",
                    e
                );
                let _ = self.remove_container(&built.name).await;
                return Err(e);
            }
        }

        self.docker
            .start_container(&built.name, None::<StartContainerOptions<String>>)
            .await?;

        if let Some(identity) = identity {
            // Persist a FRESHLY-GENERATED credential's hash only now that it has
            // actually been delivered to a started container, so a failed create
            // never leaves a persisted hash for a credential no container holds
            // (a read-back credential's hash was already persisted in
            // `prepare_identity`). Keyed by the globally-unique deployment UUID.
            if !identity.read_back {
                if let Ok(uuid) = Uuid::parse_str(&desired.deployment_uuid) {
                    let hash = sha256_hex(identity.credential.as_bytes());
                    if let Err(e) =
                        db_deployments::set_identity_credential_hash(&self.db_pool, uuid, &hash)
                            .await
                    {
                        warn!(
                            container = %built.name,
                            "Failed to persist identity credential hash after delivery: {:?}", e
                        );
                    }
                }
            }
            // Record the delivery time so the periodic refresh does not
            // immediately re-mint freshly delivered tokens. Only meaningful when
            // the deployment actually has token files to refresh.
            if !identity.tokens.is_empty() {
                self.identity_refresh
                    .lock()
                    .expect("identity_refresh mutex poisoned")
                    .insert(desired.deployment_uuid.clone(), Utc::now());
            }
        }
        info!(container = %built.name, image = %desired.image, "Created and started container");
        Ok(())
    }

    /// Upload a deployment's bootstrap credential + per-audience token files into
    /// a container via the Docker archive API.
    async fn deliver_identity(&self, container: &str, identity: &PreparedIdentity) -> Result<()> {
        let tar =
            super::identity::build_identity_tar(Some(&identity.credential), &identity.tokens)?;
        super::identity::upload_identity(&self.docker, container, tar).await
    }

    // ── Workload identity ───────────────────────────────────────────────

    /// Prepare the workload-identity material for each deployment (keyed by its
    /// globally-unique UUID) that has a Create/Recreate action this tick. A
    /// preparation failure is isolated and logged: the container is still created
    /// (it just won't carry identity files until a later (re)create), so one
    /// transient error can't stall a rollout.
    async fn prepare_identities(
        &self,
        project: &Project,
        deployments: &[Deployment],
        actual: &[ActualContainer],
        deploy_uuids: &HashSet<String>,
    ) -> HashMap<String, PreparedIdentity> {
        let mut out = HashMap::new();
        if deploy_uuids.is_empty() {
            return out;
        }
        for deployment in deployments {
            let uuid = deployment.id.to_string();
            if !deploy_uuids.contains(&uuid) || !should_have_infrastructure(deployment) {
                continue;
            }
            match self.prepare_identity(project, deployment, actual).await {
                Ok(prepared) => {
                    out.insert(uuid, prepared);
                }
                Err(e) => warn!(
                    deployment_id = %deployment.deployment_id,
                    "Failed to prepare workload identity; container(s) will be created \
                     without identity files this tick: {:?}",
                    e
                ),
            }
        }
        out
    }

    /// Resolve a deployment's stable bootstrap credential, persist its hash (only
    /// for a read-back credential — see [`PreparedIdentity::read_back`]), and mint
    /// its per-audience tokens.
    async fn prepare_identity(
        &self,
        project: &Project,
        deployment: &Deployment,
        actual: &[ActualContainer],
    ) -> Result<PreparedIdentity> {
        let env_name = self.resolve_environment_name(deployment).await?;
        let (credential, read_back) = self.resolve_credential(deployment, actual).await;

        // A read-back credential is already authoritative (a container is running
        // it), so persist its hash now — idempotent, only written when it differs.
        // A freshly-generated credential's hash is persisted only after it has
        // been delivered to a started container (see `create_container`).
        if read_back {
            let hash = sha256_hex(credential.as_bytes());
            if deployment.identity_credential_hash.as_deref() != Some(hash.as_str()) {
                db_deployments::set_identity_credential_hash(&self.db_pool, deployment.id, &hash)
                    .await
                    .context("Failed to persist identity credential hash")?;
            }
        }

        let tokens = self.mint_audience_tokens(project, deployment, env_name.as_deref())?;
        Ok(PreparedIdentity {
            credential,
            tokens,
            read_back,
        })
    }

    /// Recover the deployment's stable bootstrap credential from any of its
    /// existing containers (the Docker analogue of K8s reading it back from the
    /// observed Secret), falling back to a freshly generated one for a brand-new
    /// deployment. Reusing the observed value keeps the credential — and thus the
    /// persisted hash and every replica's credential file — stable across
    /// recreates, restarts, and scale-up. Returns `(credential, read_back)` where
    /// `read_back` is `true` when an existing container's credential was reused.
    async fn resolve_credential(
        &self,
        deployment: &Deployment,
        actual: &[ActualContainer],
    ) -> (String, bool) {
        for container in actual
            .iter()
            .filter(|c| container_belongs_to(c, deployment))
        {
            if let Some(credential) =
                super::identity::read_back_credential(&self.docker, &container.name).await
            {
                return (credential, true);
            }
        }
        (
            crate::server::workload_tokens::generate_bootstrap_credential(),
            false,
        )
    }

    /// Mint one Rise-signed workload JWT per `[identity].audiences` entry
    /// (filename → JWT). Empty when the deployment declares no (safe) audiences.
    /// Unsafe filenames (path-traversal defense) are dropped before minting so we
    /// never sign a token we'd then refuse to deliver. The actual signing uses the
    /// shared [`crate::server::workload_tokens::sign_audience_tokens`] helper, so
    /// the claim set can't drift from the K8s webhook.
    fn mint_audience_tokens(
        &self,
        project: &Project,
        deployment: &Deployment,
        env_name: Option<&str>,
    ) -> Result<BTreeMap<String, String>> {
        let audiences: BTreeMap<String, String> =
            serde_json::from_value(deployment.identity_audiences.clone()).unwrap_or_default();
        let safe: BTreeMap<String, String> = audiences
            .into_iter()
            .filter(|(filename, _)| {
                let ok = super::identity::is_safe_token_filename(filename);
                if !ok {
                    warn!(
                        deployment_id = %deployment.deployment_id,
                        filename = %filename,
                        "Skipping [identity] audience with an unsafe token filename"
                    );
                }
                ok
            })
            .collect();
        if safe.is_empty() {
            return Ok(BTreeMap::new());
        }

        let sub = workload_subject(&project.name, env_name);
        let info = WorkloadSubjectInfo {
            sub: &sub,
            project: &project.name,
            environment: env_name.unwrap_or(NO_ENVIRONMENT),
            deployment_group: &deployment.deployment_group,
            deployment_id: &deployment.deployment_id,
        };
        crate::server::workload_tokens::sign_audience_tokens(
            &self.jwt_signer,
            &info,
            &safe,
            self.identity_token_ttl_seconds,
        )
    }

    /// Re-mint and re-upload the `[identity]` token files for long-running
    /// containers whose tokens are past half their TTL — parity with the K8s
    /// webhook's re-mint behavior, keeping mounted token files fresh without
    /// recreating the workload. The bootstrap credential is left untouched (it is
    /// stable and already present), so only the token files are rewritten.
    async fn refresh_identity_tokens(
        &self,
        project: &Project,
        deployments: &[Deployment],
        actual: &[ActualContainer],
    ) {
        let refresh_after =
            chrono::Duration::seconds((self.identity_token_ttl_seconds / 2).max(1) as i64);
        for deployment in deployments {
            if !should_have_infrastructure(deployment) {
                continue;
            }
            // Cheap pre-check: skip deployments with no audiences (no token files).
            let has_audiences = deployment
                .identity_audiences
                .as_object()
                .map(|m| !m.is_empty())
                .unwrap_or(false);
            if !has_audiences {
                continue;
            }
            // Per-deployment staleness gate, keyed by the globally-unique UUID
            // (in-memory; a missing entry — e.g. after a restart — counts as stale
            // and triggers one refresh).
            let uuid = deployment.id.to_string();
            let stale = {
                let map = self
                    .identity_refresh
                    .lock()
                    .expect("identity_refresh mutex poisoned");
                map.get(&uuid)
                    .map(|ts| Utc::now().signed_duration_since(*ts) >= refresh_after)
                    .unwrap_or(true)
            };
            if !stale {
                continue;
            }
            // Target the deployment's live containers (running OR restarting —
            // a restarting container keeps its filesystem, so the upload lands and
            // is picked up once it is back up). Just-created containers already
            // carry fresh tokens.
            let live: Vec<&ActualContainer> = actual
                .iter()
                .filter(|c| {
                    container_belongs_to(c, deployment)
                        && matches!(c.state.as_deref(), Some("running") | Some("restarting"))
                })
                .collect();
            if live.is_empty() {
                continue;
            }

            let env_name = match self.resolve_environment_name(deployment).await {
                Ok(name) => name,
                Err(e) => {
                    warn!(deployment_id = %deployment.deployment_id, "Identity refresh: failed to resolve environment: {:?}", e);
                    continue;
                }
            };
            let tokens = match self.mint_audience_tokens(project, deployment, env_name.as_deref()) {
                Ok(tokens) if !tokens.is_empty() => tokens,
                Ok(_) => continue,
                Err(e) => {
                    warn!(deployment_id = %deployment.deployment_id, "Identity refresh: failed to mint tokens: {:?}", e);
                    continue;
                }
            };
            let tar = match super::identity::build_identity_tar(None, &tokens) {
                Ok(tar) => tar,
                Err(e) => {
                    warn!(deployment_id = %deployment.deployment_id, "Identity refresh: failed to build tar: {:?}", e);
                    continue;
                }
            };

            // Mark the deployment fresh only if EVERY live container received the
            // new tokens. A partial failure leaves the entry stale so the failed
            // container is retried next tick rather than masking the failure and
            // letting that one container's tokens expire.
            let mut all_delivered = true;
            for container in &live {
                if let Err(e) =
                    super::identity::upload_identity(&self.docker, &container.name, tar.clone())
                        .await
                {
                    warn!(
                        container = %container.name,
                        "Identity refresh: failed to upload tokens: {:?}", e
                    );
                    all_delivered = false;
                }
            }
            if all_delivered {
                self.identity_refresh
                    .lock()
                    .expect("identity_refresh mutex poisoned")
                    .insert(uuid, Utc::now());
            }
        }
    }

    /// Self-heal deployments whose bootstrap credential was never established —
    /// i.e. a running deployment with a NULL `identity_credential_hash`, which
    /// means a prior tick's create failed to deliver the files or to persist the
    /// hash (e.g. a transient DB error). Identity is otherwise only set up on
    /// Create/Recreate, so without this a healthy container left with no credential
    /// would never recover until an unrelated recreate. This mirrors the K8s
    /// webhook, which re-establishes identity on every sync.
    ///
    /// Bounded to deployments that actually need it (NULL hash + at least one live
    /// container), so the happy path (hash already persisted on create) pays only
    /// the cheap `is_some()` check.
    async fn remediate_missing_identity(
        &self,
        project: &Project,
        deployments: &[Deployment],
        actual: &[ActualContainer],
    ) {
        for deployment in deployments {
            if !should_have_infrastructure(deployment)
                || deployment.identity_credential_hash.is_some()
            {
                continue;
            }
            let live: Vec<&ActualContainer> = actual
                .iter()
                .filter(|c| {
                    container_belongs_to(c, deployment)
                        && matches!(c.state.as_deref(), Some("running") | Some("restarting"))
                })
                .collect();
            if live.is_empty() {
                continue;
            }

            let prepared = match self.prepare_identity(project, deployment, actual).await {
                Ok(prepared) => prepared,
                Err(e) => {
                    warn!(deployment_id = %deployment.deployment_id, "Identity remediation: prepare failed: {:?}", e);
                    continue;
                }
            };
            // A read-back credential means the files are already present and
            // `prepare_identity` just persisted the hash — nothing left to do.
            if prepared.read_back {
                continue;
            }
            // Fresh credential: deliver it (and any tokens) to every live
            // container, then persist the hash only if ALL deliveries succeeded —
            // so the hash is never recorded for a credential not on every replica.
            let tar = match super::identity::build_identity_tar(
                Some(&prepared.credential),
                &prepared.tokens,
            ) {
                Ok(tar) => tar,
                Err(e) => {
                    warn!(deployment_id = %deployment.deployment_id, "Identity remediation: failed to build tar: {:?}", e);
                    continue;
                }
            };
            let mut all_delivered = true;
            for container in &live {
                if let Err(e) =
                    super::identity::upload_identity(&self.docker, &container.name, tar.clone())
                        .await
                {
                    warn!(container = %container.name, "Identity remediation: failed to deliver: {:?}", e);
                    all_delivered = false;
                }
            }
            if all_delivered {
                let hash = sha256_hex(prepared.credential.as_bytes());
                if let Err(e) = db_deployments::set_identity_credential_hash(
                    &self.db_pool,
                    deployment.id,
                    &hash,
                )
                .await
                {
                    warn!(deployment_id = %deployment.deployment_id, "Identity remediation: failed to persist hash: {:?}", e);
                } else if !prepared.tokens.is_empty() {
                    self.identity_refresh
                        .lock()
                        .expect("identity_refresh mutex poisoned")
                        .insert(deployment.id.to_string(), Utc::now());
                }
            }
        }
    }

    /// Evict stale `identity_refresh` entries so the map cannot grow without bound
    /// as deployments come and go. An entry older than the token TTL belongs to a
    /// deployment that is gone (or stopped refreshing): an active deployment
    /// refreshes at half-TTL, so its entry is always younger than the TTL and is
    /// never evicted. Evicting a still-live entry would be harmless anyway (it
    /// would just trigger one extra refresh).
    fn prune_identity_refresh(&self) {
        let ttl = chrono::Duration::seconds(self.identity_token_ttl_seconds.max(1) as i64);
        let now = Utc::now();
        self.identity_refresh
            .lock()
            .expect("identity_refresh mutex poisoned")
            .retain(|_, ts| now.signed_duration_since(*ts) < ttl);
    }

    /// Resolve a deployment's environment name (`None` for the no-environment
    /// case), shared by identity preparation and refresh.
    async fn resolve_environment_name(&self, deployment: &Deployment) -> Result<Option<String>> {
        match deployment.environment_id {
            Some(env_id) => Ok(db_environments::find_by_id(&self.db_pool, env_id)
                .await?
                .map(|e| e.name)),
            None => Ok(None),
        }
    }

    async fn pull_image(&self, image: &str) -> Result<()> {
        use bollard::auth::DockerCredentials;
        use bollard::image::CreateImageOptions;

        let creds = match self.registry_provider.get_pull_credentials().await {
            Ok((username, password)) if !username.is_empty() || !password.is_empty() => {
                Some(DockerCredentials {
                    username: Some(username),
                    password: Some(password),
                    serveraddress: Some(self.registry_provider.registry_host().to_string()),
                    ..Default::default()
                })
            }
            Ok(_) => None,
            Err(e) => {
                warn!(
                    "Failed to obtain pull credentials, pulling anonymously: {:?}",
                    e
                );
                None
            }
        };

        let options = Some(CreateImageOptions {
            from_image: image.to_string(),
            ..Default::default()
        });
        let mut stream = self.docker.create_image(options, None, creds);
        while let Some(item) = stream.next().await {
            item.map_err(|e| anyhow::anyhow!("Image pull failed for {}: {:?}", image, e))?;
        }
        Ok(())
    }

    /// Force-remove a container by id. Returns `Err` on failure (logged here too)
    /// so the Recreate path can refuse to create the replacement when the old
    /// container is still present — otherwise two same-identity containers would
    /// share the alias + router, both serving (incl. the stale image). GC callers
    /// that don't care about the outcome can ignore the result.
    ///
    /// A `404` (container already gone — an out-of-band removal race) is treated
    /// as SUCCESS: delete is idempotent, the desired end-state (container absent)
    /// already holds, so the Recreate path proceeds and GC stays quiet. Any other
    /// failure still returns `Err`, so the Recreate guard keeps fencing a genuinely
    /// stuck remove.
    async fn remove_container(&self, id: &str) -> Result<()> {
        use bollard::container::RemoveContainerOptions;
        match self
            .docker
            .remove_container(
                id,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await
        {
            Ok(()) => Ok(()),
            Err(e) if is_not_found(&e) => {
                // Already gone (out-of-band removal race): idempotent success.
                debug!(container_id = %id, "Container already removed (404); treating as success");
                Ok(())
            }
            Err(e) => {
                warn!(container_id = %id, "Failed to remove container: {:?}", e);
                Err(anyhow::anyhow!("Failed to remove container {id}: {e:?}"))
            }
        }
    }

    // ── Health → status ────────────────────────────────────────────────

    /// Fetch and aggregate Traefik `serverStatus` across the service name(s) a
    /// container's labels emit (one bare-base service for a single-route
    /// container, per-route `{base}-{idx}` services for a multi-route one),
    /// memoized per service for the reconcile pass via `cache`.
    ///
    /// Returns `Some(merged map)` if AT LEAST ONE queried service reported a
    /// serverStatus (the per-route services share the same `http://{ip}:{port}`
    /// servers, so merging them is safe and a server reported UP by any route's
    /// health check counts as UP). `None` when there is no Traefik API, no
    /// service names, or every queried service returned `None` (unreachable /
    /// non-200 / no HC labels) — the caller then falls back to Rise's own probe.
    async fn fetch_server_status_aggregated(
        &self,
        service_names: &[String],
        cache: &mut HashMap<String, Option<HashMap<String, bool>>>,
    ) -> Option<HashMap<String, bool>> {
        let client = self.traefik_api.as_ref()?;
        let mut merged: HashMap<String, bool> = HashMap::new();
        let mut any = false;
        for service in service_names {
            let status = match cache.get(service) {
                Some(cached) => cached.clone(),
                None => {
                    let fetched = client.server_status(service).await;
                    cache.insert(service.clone(), fetched.clone());
                    fetched
                }
            };
            if let Some(map) = status {
                any = true;
                for (server, up) in map {
                    // A server is UP if ANY route's health check reports it UP.
                    let entry = merged.entry(server).or_insert(false);
                    *entry = *entry || up;
                }
            }
        }
        if any {
            Some(merged)
        } else {
            None
        }
    }

    /// Resolve the deployment's primary ingress hosts — the same set
    /// `compute_desired_for_deployment` feeds into each route's `hosts`. Used by
    /// the `HealthRolling` readiness pass to decide whether the container emits a
    /// Traefik router at all (no host → no router → no `serverStatus` to query).
    async fn primary_hosts_for_deployment(
        &self,
        project: &Project,
        deployment: &Deployment,
    ) -> Result<Vec<String>> {
        let environment = if let Some(env_id) = deployment.environment_id {
            db_environments::find_by_id(&self.db_pool, env_id).await?
        } else {
            None
        };
        let all_environments = db_environments::list_for_project(&self.db_pool, project.id).await?;
        let custom_domains =
            db_custom_domains::list_project_custom_domains(&self.db_pool, project.id).await?;
        Ok(self
            .resource_builder
            .primary_ingress_hosts(
                project,
                &deployment.deployment_group,
                environment.as_ref().filter(|env| {
                    env.primary_deployment_group.as_deref() == Some(&deployment.deployment_group)
                }),
                &custom_domains,
                true,
                &all_environments,
            )
            .into_iter()
            .map(|h| h.host)
            .collect())
    }

    async fn reconcile_health(
        &self,
        project: &Project,
        deployment: &Deployment,
        server_status_cache: &mut HashMap<String, Option<HashMap<String, bool>>>,
    ) -> Result<()> {
        // Only probe states where health is meaningful.
        if !matches!(
            deployment.status,
            DeploymentStatus::Deploying | DeploymentStatus::Healthy | DeploymentStatus::Unhealthy
        ) {
            return Ok(());
        }

        let (container_specs, route_specs) = resolve_runtime_containers(deployment)?;
        // The live container's `--name` now carries a per-recreate `_g{n}`
        // suffix we can't reconstruct from the spec alone. Enumerate the actual
        // containers once and index them by their stable identity tuple so each
        // spec can be resolved to its REAL (generation-ful) name + id, which we
        // then inspect by. A spec with no matching actual is simply not-ready.
        let actual = self.list_actual_containers(project).await?;
        let actual_by_identity: HashMap<String, &ActualContainer> = actual
            .iter()
            .filter_map(|a| a.identity().map(|id| (id, a)))
            .collect();
        // One pod entry per REPLICA container, in (spec, replica) order. Each
        // carries the live container's REAL (generation-ful) name where present,
        // else a replica-distinct stable fallback for a not-yet-created replica.
        // Fed straight into `build_controller_metadata` (no re-derivation).
        let mut pods: Vec<(String, Option<InspectedContainer>)> = Vec::new();
        // Every REPLICA of every spec must be ready for the deployment to be
        // healthy:
        //   - HTTP containers (with a port) must answer the probe — a real HTTP
        //     GET to the app (loopback published port when `publish_app_ports`
        //     is on, else the container IP), independent of Traefik routing, so
        //     a not-yet-active deployment can still become Healthy (then active);
        //   - workers (no port) must exist and be `running` on the daemon.
        // An empty spec set is never ready.
        let mut all_ready = !container_specs.is_empty();
        // Per-container "why not ready" detail, surfaced into the Unhealthy status
        // reason (and logged) so an operator sees *what* was probed and *how* it
        // failed — not just a generic "health probe failing".
        let mut not_ready_reasons: Vec<String> = Vec::new();
        let running_of =
            |i: &Option<InspectedContainer>| i.as_ref().map(|i| i.running).unwrap_or(false);
        let rolling =
            self.config.cutover_strategy == crate::server::settings::CutoverStrategy::HealthRolling;
        // Only the `HealthRolling` readiness pass consults `service_names_for_spec`
        // (and only it needs the host gate), so resolve the hosts once per tick
        // for this deployment, lazily — non-rolling groups pay nothing.
        let primary_hosts = if rolling {
            self.primary_hosts_for_deployment(project, deployment)
                .await?
        } else {
            Vec::new()
        };
        for spec in &container_specs {
            let replica_count = clamp_replicas(spec.replicas);
            // In `HealthRolling` mode, the readiness signal that drives the
            // Deploying→Healthy supersede (and Healthy→Unhealthy) is whether the
            // container's server is actually IN Traefik's rotation, not whether
            // Rise's own probe passes. Fetch the `serverStatus` for the SAME
            // Traefik service(s) the container's labels emit — a single-route
            // container has one bare-base service, a multi-route container has
            // per-route services (`{base}-{idx}`) — and aggregate them, so a
            // multi-route container's lookup doesn't 404 against a bare-base name
            // that was never registered. `None` (no Traefik API configured, the
            // call failed, or no HC labels) → `rolling_rotation_decision` returns
            // `FallBackToProbe` and we mirror with Rise's own probe.
            let service_names = service_names_for_spec(
                &project.name,
                &deployment.deployment_group,
                spec,
                &route_specs,
                &primary_hosts,
            );
            let has_health_path = effective_health_path(spec, &self.config.health_path).is_some();
            let server_status = if rolling {
                self.fetch_server_status_aggregated(&service_names, server_status_cache)
                    .await
            } else {
                None
            };
            // When the Traefik API IS available but no serverStatus came back for
            // a container that HAS a health path, the per-server health check is
            // degraded (service not yet registered, or HC labels missing) — warn
            // so operators see it, rather than silently falling back to the probe.
            if rolling && self.traefik_api.is_some() && has_health_path && server_status.is_none() {
                warn!(
                    deployment_id = %deployment.deployment_id,
                    container = %spec.name,
                    services = ?service_names,
                    "Traefik API reachable but returned no serverStatus for a container with a \
                     health path; falling back to Rise's own probe (Traefik per-server health \
                     check may be missing or the service is not yet registered)"
                );
            }
            for replica in 0..replica_count {
                // Label each container with its replica index only when there is
                // more than one, so single-replica diagnostics read unchanged.
                let label = if replica_count > 1 {
                    format!("{}[{}]", spec.name, replica)
                } else {
                    spec.name.clone()
                };
                let identity = identity_key(
                    &project.name,
                    &deployment.deployment_group,
                    &deployment.deployment_id,
                    &spec.name,
                    replica,
                );
                // Resolve this replica to its live container. When present,
                // inspect by the actual generation-ful name; when absent (not yet
                // created / mid-recreate), synthesize a replica-distinct pod name
                // (the replica-free stable name + `_r{n}`) and skip the inspect.
                let actual = actual_by_identity.get(&identity).copied();
                let name = match actual {
                    Some(a) => a.name.clone(),
                    None => format!(
                        "{}_r{replica}",
                        container_builder::stable_identity_name(
                            &self.config.container_prefix,
                            &project.name,
                            &deployment.deployment_group,
                            &deployment.deployment_id,
                            &spec.name,
                        )
                    ),
                };
                let inspected = match actual {
                    Some(a) => self.inspect_for_reconcile(&a.name, spec.port).await,
                    None => None,
                };
                let ready = if actual.is_none() {
                    // No live container matches this replica's identity (not yet
                    // created, or briefly absent mid-recreate) — definitively
                    // not-ready, with a clear reason.
                    not_ready_reasons.push(format!("'{label}' container not found"));
                    false
                } else {
                    match spec.port {
                        // HTTP container: honor the per-container `health_check`
                        // spec. An ABSENT `health_check`, or `disabled = true`
                        // (→ `effective_health_path` returns None), means no HTTP
                        // probe, so a *running* container is ready — mirroring
                        // K8s, where a Pod with no readiness probe is Ready once
                        // up. Otherwise probe `health_check.path` (or default).
                        Some(port) => {
                            let health_path = effective_health_path(spec, &self.config.health_path);
                            let has_health_path = health_path.is_some();
                            let running = running_of(&inspected);
                            // Whether Traefik's serverStatus authoritatively
                            // reports this replica's server, and if so UP/DOWN.
                            // `api_available` requires both a client AND a fetched
                            // status; `server_up` is None when the URL is absent
                            // from the map (or no IP yet) so the decision reports
                            // the distinct absent-server reason.
                            let api_available =
                                self.traefik_api.is_some() && server_status.is_some();
                            // `server_status` is only ever `Some` when a Traefik
                            // client exists (see `fetch_server_status_aggregated`,
                            // which returns `None` without one), so `server_up`
                            // can be `Some` only when `api_available` is true.
                            // When the API isn't available the verdict must defer
                            // to Rise's own probe (`None`). When it IS available,
                            // pass the per-server UP/DOWN straight through, and
                            // leave `server_up` as `None` when the URL is absent
                            // from the map (or there's no IP yet) — that ABSENT
                            // case is distinct from a reported-DOWN server and the
                            // verdict reports a different reason for each. Do NOT
                            // collapse absent → `Some(false)`: that would mislabel
                            // an unknown server as "DOWN".
                            let server_up = if api_available {
                                server_status.as_ref().and_then(|m| {
                                    inspected.as_ref().and_then(|i| i.ip.as_deref()).and_then(
                                        |ip| m.get(&format!("http://{ip}:{port}")).copied(),
                                    )
                                })
                            } else {
                                None
                            };
                            // Pure selection of the readiness verdict. `NeedsProbe`
                            // means the decision depends on Rise's own probe — we
                            // run it and use its detailed failure reason.
                            match replica_ready(
                                rolling,
                                has_health_path,
                                running,
                                api_available,
                                server_up,
                                None,
                            ) {
                                ReadyVerdict::Ready => true,
                                ReadyVerdict::NotReady(reason) => {
                                    debug!(
                                        deployment_id = %deployment.deployment_id,
                                        container = %label,
                                        "Replica not ready: {reason}"
                                    );
                                    not_ready_reasons.push(format!("'{label}' {reason}"));
                                    false
                                }
                                ReadyVerdict::NeedsProbe => {
                                    let path = health_path
                                        .as_deref()
                                        .expect("NeedsProbe implies a health path");
                                    match self.probe_container(inspected.as_ref(), port, path).await
                                    {
                                        Ok(()) => true,
                                        Err(reason) => {
                                            // Per-tick at debug to avoid noise while
                                            // an app is still warming up; the reason
                                            // is also folded into the Unhealthy
                                            // status.
                                            debug!(
                                                deployment_id = %deployment.deployment_id,
                                                container = %label,
                                                "Health probe failed: {reason}"
                                            );
                                            not_ready_reasons.push(format!("'{label}' {reason}"));
                                            false
                                        }
                                    }
                                }
                            }
                        }
                        // Worker: no HTTP endpoint, so the daemon's run state is
                        // the only liveness signal we have.
                        None => {
                            let running = running_of(&inspected);
                            if !running {
                                not_ready_reasons.push(format!("worker '{label}' not running"));
                            }
                            running
                        }
                    }
                };
                pods.push((name, inspected));
                if !ready {
                    all_ready = false;
                    // Keep inspecting the remaining replicas so the pod_status
                    // snapshot stays complete; only the readiness verdict short-
                    // circuits below via `all_ready`.
                }
            }
        }
        let is_ready = all_ready;
        // Human-readable rollup of the not-ready reasons for the status message.
        let unhealthy_reason = if not_ready_reasons.is_empty() {
            "Container health probe failing".to_string()
        } else {
            format!(
                "Container health probe failing: {}",
                not_ready_reasons.join("; ")
            )
        };

        // Snapshot controller_metadata in a K8s-pod-status-shaped blob so the
        // existing status APIs/UI render unchanged. Built from the per-replica
        // inspections captured above (no second inspect).
        let metadata = build_controller_metadata(&pods, &deployment.status, is_ready);
        if let Err(e) =
            db_deployments::update_controller_metadata(&self.db_pool, deployment.id, &metadata)
                .await
        {
            warn!(
                deployment_id = %deployment.deployment_id,
                "Failed to update controller metadata: {:?}", e
            );
        }

        match deployment.status {
            DeploymentStatus::Deploying if is_ready => {
                info!(
                    deployment_id = %deployment.deployment_id,
                    "Deployment is ready, marking as Healthy"
                );
                self.handle_deployment_became_healthy(project, deployment)
                    .await?;
            }
            DeploymentStatus::Healthy if !is_ready => {
                warn!(
                    deployment_id = %deployment.deployment_id,
                    "Healthy deployment is now unhealthy: {unhealthy_reason}"
                );
                db_deployments::mark_unhealthy(&self.db_pool, deployment.id, unhealthy_reason)
                    .await?;
                db_projects::update_calculated_status(&self.db_pool, project.id).await?;
            }
            DeploymentStatus::Unhealthy if is_ready => {
                info!(
                    deployment_id = %deployment.deployment_id,
                    "Unhealthy deployment has recovered, marking as Healthy"
                );
                db_deployments::mark_healthy(&self.db_pool, deployment.id).await?;
                db_projects::update_calculated_status(&self.db_pool, project.id).await?;
            }
            _ => {}
        }

        Ok(())
    }

    /// Probe each live replica's HTTP health for the rolling-recreate throttle,
    /// returning `identity_key(...) → healthy`. A replica is healthy when, per its
    /// desired spec, either:
    ///
    /// - it has a `health_path` (HTTP container with the probe enabled) and the
    ///   probe to its live container succeeds (`2xx–3xx`); or
    /// - it has no `health_path` (worker, or probe disabled) and its live
    ///   container is `running`.
    ///
    /// Containers with no desired match, or that aren't inspectable, are omitted
    /// (the throttle treats an absent entry as NOT healthy). Each container is
    /// inspected once. This is a SEPARATE pass from `reconcile_health` (which runs
    /// after apply); duplicating it keeps the throttle self-contained and only
    /// runs when a rollout recreate is actually pending.
    async fn probe_health_by_identity(
        &self,
        desired: &[DesiredContainer],
        actual: &[ActualContainer],
    ) -> HashMap<String, bool> {
        // identity → desired (for port + effective health path).
        let desired_by_identity: HashMap<String, &DesiredContainer> = desired
            .iter()
            .map(|d| {
                (
                    identity_key(
                        &d.project,
                        &d.deployment_group,
                        &d.deployment_id,
                        &d.container,
                        d.replica,
                    ),
                    d,
                )
            })
            .collect();

        let mut healthy: HashMap<String, bool> = HashMap::new();
        for a in actual {
            let Some(identity) = a.identity() else {
                continue;
            };
            let Some(d) = desired_by_identity.get(&identity) else {
                continue;
            };
            let inspected = self.inspect_for_reconcile(&a.name, d.port).await;
            let is_healthy = match (d.port, d.health_path.as_deref()) {
                // HTTP container with the probe enabled → real GET.
                (Some(port), Some(path)) => self
                    .probe_container(inspected.as_ref(), port, path)
                    .await
                    .is_ok(),
                // Worker, or probe disabled → run-state is the only signal.
                _ => inspected.as_ref().map(|i| i.running).unwrap_or(false),
            };
            healthy.insert(identity, is_healthy);
        }
        healthy
    }

    /// HTTP health probe against the app, using a single inspection captured by
    /// the caller (no extra `inspect_container`). The probe is ALWAYS a real GET
    /// to the application.
    ///
    /// When `publish_app_ports` is on and the container has a published loopback
    /// port, it probes `http://127.0.0.1:{published_host_port}{path}`
    /// (host-reachable on Docker Desktop AND Linux; hits the app directly, not
    /// Traefik/forwardAuth). Otherwise it probes the container's IP on the
    /// Traefik network (`http://{ip}:{port}{path}`) — today's behavior, correct
    /// for the containerized/production backend on the shared network.
    ///
    /// Reuses the reconciler's shared `reqwest::Client` and a `2xx–3xx`
    /// success criterion (5s timeout) — the same range Traefik's load-balancer
    /// health check treats as healthy, so the in-process probe and the Traefik
    /// health check agree (a `404`/`5xx` health endpoint is NOT healthy).
    /// Returns `Ok(())` when the app answers `2xx–3xx`, or `Err(reason)`
    /// describing exactly what was probed and how it failed (no target yet, bad
    /// status, or a connection/timeout error) so the reason can be surfaced into
    /// the deployment's Unhealthy status and logs.
    async fn probe_container(
        &self,
        inspected: Option<&InspectedContainer>,
        port: u16,
        health_path: &str,
    ) -> Result<(), String> {
        let Some(inspected) = inspected else {
            return Err("container not yet created/inspectable".to_string());
        };
        let url = if self.config.publish_app_ports {
            match inspected.published_host_port.as_deref() {
                Some(host_port) if !host_port.is_empty() => {
                    format!("http://127.0.0.1:{host_port}{health_path}")
                }
                _ => {
                    return Err(format!(
                        "app port {port} is not published to a loopback host port yet \
                         (publish_app_ports is on) — the container predates the setting and \
                         will be recreated automatically to gain the binding"
                    ));
                }
            }
        } else {
            match inspected.ip.as_deref() {
                Some(ip) if !ip.is_empty() => format!("http://{ip}:{port}{health_path}"),
                _ => {
                    return Err(format!(
                        "container has no IP on network '{}' yet",
                        self.config.traefik_network
                    ));
                }
            }
        };
        match self.http_client.get(&url).send().await {
            Ok(resp) => {
                let code = resp.status().as_u16();
                if (200..400).contains(&code) {
                    Ok(())
                } else {
                    Err(format!("GET {url} returned HTTP {code} (expected 2xx–3xx)"))
                }
            }
            Err(e) => Err(format!("GET {url} failed: {}", probe_error_detail(&e))),
        }
    }

    /// Single `inspect_container` for the reconcile pass: returns an owned
    /// snapshot reused by BOTH the health probe and the `pod_status` builder, so
    /// each container is inspected once per tick. `port` is the container's
    /// app port (if any), used to resolve the published loopback host port from
    /// the `network_settings.ports["{port}/tcp"]` mapping. Returns `None` when
    /// the container is missing / not yet created.
    async fn inspect_for_reconcile(
        &self,
        container_name: &str,
        port: Option<u16>,
    ) -> Option<InspectedContainer> {
        let inspect = match self.docker.inspect_container(container_name, None).await {
            Ok(inspect) => inspect,
            Err(e) => {
                debug!(
                    container = %container_name,
                    "Failed to inspect container for reconcile: {:?}", e
                );
                return None;
            }
        };

        let state = inspect.state.clone();
        // The bollard `ContainerStateStatusEnum` Displays as the lowercase API
        // string ("running", "exited", …), matching what we map on in the
        // pod_status builder.
        let status = state
            .as_ref()
            .and_then(|s| s.status.as_ref())
            .map(|s| s.to_string());
        let running = state.as_ref().and_then(|s| s.running).unwrap_or(false);
        let started_at = state.as_ref().and_then(|s| s.started_at.clone());
        let finished_at = state.as_ref().and_then(|s| s.finished_at.clone());
        let exit_code = state.as_ref().and_then(|s| s.exit_code);
        let error = state
            .as_ref()
            .and_then(|s| s.error.clone())
            .filter(|e| !e.is_empty());
        // `HealthStatusEnum` Displays as "none"/"starting"/"healthy"/"unhealthy".
        let health = state
            .as_ref()
            .and_then(|s| s.health.as_ref())
            .and_then(|h| h.status.as_ref())
            .map(|s| s.to_string());
        // restart_count is TOP-LEVEL on the inspect response, not inside state.
        let restart_count = inspect.restart_count;

        let ip = inspect
            .network_settings
            .as_ref()
            .and_then(|ns| ns.networks.as_ref())
            .and_then(|nets| nets.get(&self.config.traefik_network))
            .and_then(|ep| ep.ip_address.clone())
            .filter(|ip| !ip.is_empty());

        // The random host port Docker assigned for the published app port (only
        // present when `publish_app_ports` created a binding for this port).
        let published_host_port = port.and_then(|p| {
            inspect
                .network_settings
                .as_ref()
                .and_then(|ns| ns.ports.as_ref())
                .and_then(|ports| ports.get(&format!("{p}/tcp")))
                .and_then(|bindings| bindings.as_ref())
                .and_then(|bindings| bindings.first())
                .and_then(|b| b.host_port.clone())
                .filter(|hp| !hp.is_empty())
        });

        Some(InspectedContainer {
            status,
            running,
            started_at,
            finished_at,
            exit_code,
            restart_count,
            health,
            error,
            ip,
            published_host_port,
        })
    }

    /// Port of `webhook::handle_deployment_became_healthy`: mark healthy,
    /// supersede the prior active deployment, mark this one active.
    async fn handle_deployment_became_healthy(
        &self,
        project: &Project,
        deployment: &Deployment,
    ) -> Result<()> {
        let active_in_group = db_deployments::find_active_for_project_and_group(
            &self.db_pool,
            project.id,
            &deployment.deployment_group,
        )
        .await?;

        db_deployments::mark_healthy(&self.db_pool, deployment.id).await?;

        if let Some(old_active) = active_in_group {
            if old_active.id != deployment.id && !state_machine::is_terminal(&old_active.status) {
                info!(
                    "Deployment {} replacing {} in group '{}', marking old as Terminating",
                    deployment.deployment_id, old_active.deployment_id, deployment.deployment_group
                );
                db_deployments::mark_terminating(
                    &self.db_pool,
                    old_active.id,
                    TerminationReason::Superseded,
                )
                .await?;
            }
        }

        let others = db_deployments::find_non_terminal_for_project_and_group(
            &self.db_pool,
            project.id,
            &deployment.deployment_group,
        )
        .await?;
        for other in others {
            if other.id != deployment.id
                && state_machine::is_active(&other.status)
                && !state_machine::is_terminal(&other.status)
            {
                info!(
                    "Cleaning up non-active deployment {} in group '{}', marking as Terminating",
                    other.deployment_id, deployment.deployment_group
                );
                db_deployments::mark_terminating(
                    &self.db_pool,
                    other.id,
                    TerminationReason::Superseded,
                )
                .await?;
            }
        }

        db_deployments::mark_as_active(
            &self.db_pool,
            deployment.id,
            project.id,
            &deployment.deployment_group,
        )
        .await?;
        db_projects::update_calculated_status(&self.db_pool, project.id).await?;
        Ok(())
    }
}

/// Pure comparison of this controller's configured class against the
/// project's Organization's `deploymentControllerClass`. Mirrors the K8s
/// webhook's `check_controller_class`:
///
/// - An empty configured class (legacy / unconfigured Docker install) matches
///   every project.
/// - Otherwise the Organization must set exactly the same class; an unset or
///   differing Org class means the project belongs to another controller.
fn controller_class_matches(configured: &str, org_class: Option<&str>) -> bool {
    if configured.is_empty() {
        return true;
    }
    org_class == Some(configured)
}

/// Resolve a spec's requested replica count to the actual number of containers
/// to run: default 1 (unset), clamped to `[1, MAX_REPLICAS]`. Shared by desired
/// computation and health aggregation so both always agree on the replica count.
fn clamp_replicas(requested: Option<u32>) -> u32 {
    requested.unwrap_or(1).clamp(1, MAX_REPLICAS)
}

/// Whether an actual container belongs to `deployment`, matched by the
/// globally-unique `deployment-uuid` label so it can never conflate two
/// same-second `deployment_id`s (even within one group). Containers managed by
/// this controller always carry the label; one missing it (legacy) simply won't
/// match and is left alone by the identity paths.
fn container_belongs_to(c: &ActualContainer, deployment: &Deployment) -> bool {
    match c.deployment_uuid_label.as_deref() {
        Some(uuid) => uuid == deployment.id.to_string(),
        None => false,
    }
}

/// The set of deployment UUIDs that have at least one container being created or
/// recreated this tick — the deployments whose containers need workload-identity
/// material prepared and delivered. Resolved through the desired set so the key is
/// the globally-unique deployment UUID rather than the second-granular
/// `deployment_id` (which can collide across same-second deployments).
fn deployment_uuids_needing_identity(
    actions: &[ReconcileAction],
    desired: &[DesiredContainer],
) -> HashSet<String> {
    let touched: HashSet<&str> = actions
        .iter()
        .filter_map(|action| match action {
            ReconcileAction::Create { identity, .. }
            | ReconcileAction::Recreate { identity, .. } => Some(identity.as_str()),
            ReconcileAction::Remove { .. } => None,
        })
        .collect();
    if touched.is_empty() {
        return HashSet::new();
    }
    desired
        .iter()
        .filter(|d| {
            touched.contains(
                identity_key(
                    &d.project,
                    &d.deployment_group,
                    &d.deployment_id,
                    &d.container,
                    d.replica,
                )
                .as_str(),
            )
        })
        .map(|d| d.deployment_uuid.clone())
        .collect()
}

/// Whether a bollard error is the daemon's "no such container" (HTTP 404). Used
/// to treat an already-gone container as an idempotent removal success (the
/// desired end-state — container absent — already holds). Kept as a pure helper
/// so the classification is unit-testable without a live daemon.
fn is_not_found(e: &bollard::errors::Error) -> bool {
    matches!(
        e,
        bollard::errors::Error::DockerResponseServerError {
            status_code: 404,
            ..
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_class_matching() {
        // Empty configured class (legacy install) matches everything.
        assert!(controller_class_matches("", None));
        assert!(controller_class_matches("", Some("anything")));
        // Configured class requires an exact Org match.
        assert!(controller_class_matches(
            "docker.rise.dev/default",
            Some("docker.rise.dev/default")
        ));
        assert!(!controller_class_matches(
            "docker.rise.dev/default",
            Some("kubernetes.rise.dev/default")
        ));
        // Org with no class set is owned by no specific controller.
        assert!(!controller_class_matches("docker.rise.dev/default", None));
    }

    #[test]
    fn is_not_found_matches_404_only() {
        // A 404 from the daemon (container already gone) is classified as
        // not-found → `remove_container` treats it as idempotent success.
        assert!(is_not_found(
            &bollard::errors::Error::DockerResponseServerError {
                status_code: 404,
                message: "no such container".to_string(),
            }
        ));
        // Any other server error (e.g. 409 conflict / 500) is NOT not-found, so
        // the Recreate guard still fences a genuinely-stuck remove.
        assert!(!is_not_found(
            &bollard::errors::Error::DockerResponseServerError {
                status_code: 409,
                message: "removal already in progress".to_string(),
            }
        ));
        assert!(!is_not_found(
            &bollard::errors::Error::DockerResponseServerError {
                status_code: 500,
                message: "boom".to_string(),
            }
        ));
    }

    #[test]
    fn clamp_replicas_defaults_and_clamps() {
        assert_eq!(clamp_replicas(None), 1, "unset → 1");
        assert_eq!(clamp_replicas(Some(0)), 1, "0 → floored to 1");
        assert_eq!(clamp_replicas(Some(1)), 1);
        assert_eq!(clamp_replicas(Some(3)), 3, "in-range passes through");
        assert_eq!(clamp_replicas(Some(MAX_REPLICAS)), MAX_REPLICAS);
        assert_eq!(
            clamp_replicas(Some(MAX_REPLICAS + 100)),
            MAX_REPLICAS,
            "above max → clamped to MAX_REPLICAS"
        );
    }

    #[test]
    fn identity_targets_are_the_create_and_recreate_deployment_uuids() {
        use crate::server::deployment::controller::docker::test_helpers::{desired, identity_of};

        // A Create targeting the desired slot resolves to that deployment's UUID;
        // a Remove never needs identity material. Resolving through `desired`
        // (keyed by the globally-unique UUID) is what avoids the same-second
        // `deployment_id` collision a deployment_id-keyed map would suffer.
        let d = desired("app", "img:1", "h1");
        let actions = vec![
            ReconcileAction::Create {
                identity: identity_of(&d),
                name: "c1".to_string(),
                generation: 1,
            },
            ReconcileAction::Remove {
                id: "x".to_string(),
                name: "c3".to_string(),
            },
        ];
        let uuids = deployment_uuids_needing_identity(&actions, std::slice::from_ref(&d));
        assert_eq!(uuids.len(), 1);
        assert!(uuids.contains(&d.deployment_uuid));

        // An action with no matching desired container yields nothing.
        let orphan = vec![ReconcileAction::Create {
            identity: identity_key("other", "default", "20260101-000000", "app", 0),
            name: "c2".to_string(),
            generation: 1,
        }];
        assert!(deployment_uuids_needing_identity(&orphan, std::slice::from_ref(&d)).is_empty());
    }
}
