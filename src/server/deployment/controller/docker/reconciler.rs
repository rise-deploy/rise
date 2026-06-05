//! In-process reconcile loop for the Docker backend.
//!
//! Docker has no Metacontroller, so this loop replicates the webhook's
//! responsibilities against the Docker daemon: status-machine transitions,
//! desired-vs-actual container diffing, GC, HTTP health probing, supersession,
//! and `controller_metadata` snapshots.
//!
//! The diff itself ([`diff_desired_vs_actual`]) is a pure function so it can be
//! unit-tested without a daemon.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Result;
use bollard::Docker;
use chrono::Utc;
use futures::StreamExt;
use rise_runtime_sync::{with_leader_election, LeaderElection, LEASE_DURATION};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use super::container_builder::{self, BuilderConfig, DesiredContainer, DesiredRoute};
use super::diff::{diff_desired_vs_actual, identity_key, ActualContainer, ReconcileAction};
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
};
use crate::server::encryption::EncryptionProvider;
use crate::server::registry::RegistryProvider;

use crate::db::custom_domains as db_custom_domains;
use crate::db::deployments as db_deployments;
use crate::db::env_vars as db_env_vars;
use crate::db::environments as db_environments;
use crate::db::projects as db_projects;

/// Duration a deployment can be in Deploying state before timing out.
/// Mirrors `webhook::DEPLOYING_TIMEOUT_MINUTES`.
const DEPLOYING_TIMEOUT_MINUTES: i64 = 5;
/// Duration a deployment can be in pre-Pushed states before timing out.
/// Mirrors `webhook::PRE_PUSHED_TIMEOUT_MINUTES`.
const PRE_PUSHED_TIMEOUT_MINUTES: i64 = 10;

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

/// Owned snapshot of one `inspect_container` call, captured once per reconcile
/// tick and reused by both the health probe and the `pod_status` builder. All
/// fields are owned so the value can be stored in a map and unit-tested without
/// a live daemon.
#[derive(Debug, Clone)]
pub struct InspectedContainer {
    /// `state.status` as the lowercase Docker API string ("running", "exited",
    /// "created", "restarting", "dead", …). `None` if absent.
    pub status: Option<String>,
    /// `state.running`.
    pub running: bool,
    /// `state.started_at` (RFC3339 string from the daemon).
    pub started_at: Option<String>,
    /// `state.finished_at`.
    pub finished_at: Option<String>,
    /// `state.exit_code`.
    pub exit_code: Option<i64>,
    /// TOP-LEVEL `restart_count` from the inspect response (not inside state).
    pub restart_count: Option<i64>,
    /// `state.health.status` ("none"/"starting"/"healthy"/"unhealthy").
    pub health: Option<String>,
    /// `state.error` (non-empty only).
    pub error: Option<String>,
    /// IPv4 address on the configured Traefik network (the non-published probe
    /// target). `None` if not attached/assigned yet.
    pub ip: Option<String>,
    /// The random `127.0.0.1` host port Docker published for the app port (only
    /// when `publish_app_ports` created a binding). The published probe target.
    pub published_host_port: Option<String>,
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
    config: ReconcilerConfig,
}

impl DockerReconciler {
    pub fn new(
        docker: Docker,
        db_pool: PgPool,
        resource_builder: Arc<ResourceBuilder>,
        registry_provider: Arc<dyn RegistryProvider>,
        encryption_provider: Option<Arc<dyn EncryptionProvider>>,
        resource_store: Arc<dyn rise_resource_store::ResourceStore>,
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
    /// tick entirely (logged at debug) and never touches the Docker daemon. The
    /// loop never panics: per-tick and per-project errors are logged and
    /// isolated.
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
            if !election.is_leader() {
                debug!("Not the Docker reconciler leader; skipping tick");
                continue;
            }
            if let Err(e) = self.tick().await {
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
    async fn tick(&self) -> Result<()> {
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
            if let Err(e) = self.reconcile_project(&project).await {
                error!(project = %project.name, "Failed to reconcile project: {:?}", e);
            }
        }
        Ok(())
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

    async fn reconcile_project(&self, project: &Project) -> Result<()> {
        let non_terminal =
            db_deployments::list_non_terminal_for_project(&self.db_pool, project.id).await?;

        // 1. Status transitions (port of webhook::perform_status_transitions).
        for deployment in &non_terminal {
            self.perform_status_transition(project, deployment).await?;
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
        self.apply_actions(project, &desired, &actions).await?;

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
        let routable = routable_for(self.config.cutover_strategy, deployment.is_active);

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
                        // Plain create (no pre-pull) pulls the image itself.
                        if let Err(e) = self.create_container(&d, &builder_cfg, false).await {
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
                        self.remove_container(existing_id).await;
                        // The image was just pulled above, BEFORE the destructive
                        // remove, so skip the pull inside `create_container` — a
                        // transient pull failure must not strand us with the old
                        // container already gone. The image is pulled exactly once.
                        if let Err(e) = self.create_container(&d, &builder_cfg, true).await {
                            error!(
                                project = %project.name,
                                container = %name,
                                "Failed to recreate container: {:?}", e
                            );
                        }
                    } else {
                        // No desired match (shouldn't happen for Recreate) — GC.
                        self.remove_container(existing_id).await;
                    }
                }
                ReconcileAction::Remove { id, name } => {
                    debug!(project = %project.name, container = %name, "GC removing container");
                    self.remove_container(id).await;
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
        self.docker
            .start_container(&built.name, None::<StartContainerOptions<String>>)
            .await?;
        info!(container = %built.name, image = %desired.image, "Created and started container");
        Ok(())
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

    async fn remove_container(&self, id: &str) {
        use bollard::container::RemoveContainerOptions;
        if let Err(e) = self
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
            warn!(container_id = %id, "Failed to remove container: {:?}", e);
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
                            // to Rise's own probe (`None`); when it is, pass the
                            // per-server UP/DOWN (or `None` for an absent server)
                            // straight through.
                            let server_up = if api_available {
                                server_status.as_ref().and_then(|m| {
                                    inspected.as_ref().and_then(|i| i.ip.as_deref()).map(|ip| {
                                        m.get(&format!("http://{ip}:{port}"))
                                            .copied()
                                            .unwrap_or(false)
                                    })
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
pub fn controller_class_matches(configured: &str, org_class: Option<&str>) -> bool {
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
}
