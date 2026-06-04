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
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tracing::{debug, error, info, warn};

use super::container_builder::{self, BuilderConfig, DesiredContainer, DesiredRoute};
use super::labels::{self, SUFFIX_ENV_HASH, SUFFIX_IMAGE, SUFFIX_MANAGED_BY};
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

/// Upper bound on the number of replicas the Docker backend will run for a
/// single container spec. A single-host daemon can run many containers behind
/// one Traefik LB service, but we cap it to avoid a runaway spec exhausting the
/// host; requests above this are clamped (with a warning).
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
        Self {
            docker,
            db_pool,
            resource_builder,
            registry_provider,
            encryption_provider,
            resource_store,
            http_client,
            config,
        }
    }

    /// Spawn the reconcile loop on the current Tokio runtime. The loop never
    /// panics: per-tick and per-project errors are logged and isolated.
    pub fn spawn(self) {
        let interval = std::time::Duration::from_secs(self.config.reconcile_interval_secs.max(1));
        tokio::spawn(async move {
            info!(
                interval_secs = interval.as_secs(),
                controller_class = %self.config.controller_class,
                "Docker reconciler started"
            );
            loop {
                if let Err(e) = self.tick().await {
                    error!("Docker reconcile tick failed: {:?}", e);
                }
                tokio::time::sleep(interval).await;
            }
        });
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
        for deployment in &non_terminal {
            if !should_have_infrastructure(deployment) {
                continue;
            }
            if let Err(e) = self.reconcile_health(project, deployment).await {
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

        // A container exists (so it can be health-probed) for any infra-bearing
        // deployment, but it is only *routable* when it is the active deployment
        // for its group — exactly mirroring the K8s path, which builds the
        // Ingress solely from `is_active` deployments (`active_by_group` in
        // `webhook.rs`). `is_active` is flipped on by `mark_as_active`, which
        // runs only after a deployment becomes Healthy. Without this gate a
        // still-Deploying/Pushed deployment would advertise the same `Host(...)`
        // rule as the live active one and Traefik would split production traffic
        // onto the not-yet-healthy container.
        let routable = deployment.is_active;

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
            // Pin PORT to this container's declared port.
            if let Some(port) = spec.port {
                upsert_env(&mut env, "PORT", &port.to_string());
            }
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
                        if let Err(e) = self.create_container(&d, &builder_cfg).await {
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
                        if let Err(e) = self.create_container(&d, &builder_cfg).await {
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

    async fn create_container(
        &self,
        desired: &DesiredContainer,
        builder_cfg: &BuilderConfig<'_>,
    ) -> Result<()> {
        use bollard::container::{CreateContainerOptions, StartContainerOptions};

        let built = container_builder::build_container(desired, builder_cfg);

        // Pull the image with registry auth (anonymous → None).
        self.pull_image(&desired.image).await?;

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

    async fn reconcile_health(&self, project: &Project, deployment: &Deployment) -> Result<()> {
        // Only probe states where health is meaningful.
        if !matches!(
            deployment.status,
            DeploymentStatus::Deploying | DeploymentStatus::Healthy | DeploymentStatus::Unhealthy
        ) {
            return Ok(());
        }

        let (container_specs, _routes) = resolve_runtime_containers(deployment)?;
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
        for spec in &container_specs {
            let replica_count = clamp_replicas(spec.replicas);
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
                        // spec. `disabled` (→ `effective_health_path` returns None)
                        // removes the probe, so a *running* container is ready —
                        // mirroring K8s, where `health_check = false` removes the
                        // probes. Otherwise probe `health_check.path` (or default).
                        Some(port) => match effective_health_path(spec, &self.config.health_path) {
                            Some(path) => {
                                match self.probe_container(inspected.as_ref(), port, &path).await {
                                    Ok(()) => true,
                                    Err(reason) => {
                                        // Per-tick at debug to avoid noise while an
                                        // app is still warming up; the reason is
                                        // also folded into the Unhealthy status.
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
                            None => {
                                let running = running_of(&inspected);
                                if !running {
                                    not_ready_reasons
                                        .push(format!("'{label}' not running (probe disabled)"));
                                }
                                running
                            }
                        },
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
    ///   probe to its live container succeeds (`< 500`); or
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
    /// Reuses the reconciler's shared `reqwest::Client` and its `status < 500`
    /// semantics (5s timeout). Returns `Ok(())` when the app answers `< 500`, or
    /// `Err(reason)` describing exactly what was probed and how it failed (no
    /// target yet, bad status, or a connection/timeout error) so the reason can
    /// be surfaced into the deployment's Unhealthy status and logs.
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
                if code < 500 {
                    Ok(())
                } else {
                    Err(format!("GET {url} returned HTTP {code}"))
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

// ── Pure pod_status builder ───────────────────────────────────────────────

/// Assemble the `controller_metadata` JSON for the deployment: a
/// K8s-pod-status-shaped `pod_status` block (so the Pods tab renders unchanged —
/// see `frontend/src/features/deployments.tsx`) plus the legacy sibling `health`
/// block. `pods` is one (name, inspection) entry PER REPLICA container across all
/// specs (so `desired_replicas` = sum of N over specs, `current_replicas` =
/// running count, `ready_replicas` = healthy count). Pure so it can be
/// unit-tested without a daemon. The container readiness verdict (`is_ready`) is
/// the same one driving the status transitions.
fn build_controller_metadata(
    pods: &[(String, Option<InspectedContainer>)],
    status: &DeploymentStatus,
    is_ready: bool,
) -> serde_json::Value {
    let pod_status = build_pod_status(pods, is_ready);
    let desired = pods.len();
    let ready_replicas = pod_status
        .get("ready_replicas")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;

    serde_json::json!({
        "pod_status": pod_status,
        "health": {
            "last_check": Utc::now().to_rfc3339(),
            // Healthy only when EVERY replica of EVERY spec is ready AND the
            // deployment isn't currently flagged Unhealthy.
            "healthy": ready_replicas == desired
                && *status != DeploymentStatus::Unhealthy,
        },
    })
}

/// Build the K8s-pod-status-shaped `pod_status` JSON from each container's
/// (name, inspection). Pure so it can be unit-tested without a daemon: one
/// container == one pod (as the Docker controller has always modeled it).
///
/// Mirrors the shape produced by the K8s path (`webhook.rs`) so the existing
/// frontend (`frontend/src/features/deployments.tsx`), API and DB need no
/// changes:
///   - top-level: `desired_replicas`, `current_replicas` (running), `ready_replicas`,
///     `pods`, `last_checked`.
///   - per pod: `name`, `phase` (running→Running, created/restarting→Pending,
///     exited/dead→Failed, else Unknown), `conditions: []`, `containers: [...]`.
///   - per container: `name`, `ready`, `restart_count`, `state` with
///     `state_type` (running|waiting|terminated), `started_at`, `finished_at`,
///     `exit_code`, `reason`.
///
/// `is_ready` is the reconciler's overall readiness verdict (every container
/// must be ready). A container's per-pod `ready` reflects that verdict only when
/// the container is actually running, so a crashed container never shows ready.
fn build_pod_status(
    named: &[(String, Option<InspectedContainer>)],
    is_ready: bool,
) -> serde_json::Value {
    let mut pods = Vec::with_capacity(named.len());
    let mut current_replicas = 0usize; // running
    let mut ready_replicas = 0usize;

    for (name, inspected) in named {
        let status = inspected
            .as_ref()
            .and_then(|i| i.status.as_deref())
            .unwrap_or("");
        let running = inspected.as_ref().map(|i| i.running).unwrap_or(false);
        if running {
            current_replicas += 1;
        }
        // A container is "ready" for the pod view when the deployment is ready
        // overall AND this container is running.
        let container_ready = is_ready && running;
        if container_ready {
            ready_replicas += 1;
        }

        let phase = match status {
            "running" => "Running",
            "created" | "restarting" => "Pending",
            "exited" | "dead" => "Failed",
            _ => "Unknown",
        };

        let restart_count = inspected
            .as_ref()
            .and_then(|i| i.restart_count)
            .unwrap_or(0);
        let started_at = inspected.as_ref().and_then(|i| i.started_at.clone());
        let finished_at = inspected.as_ref().and_then(|i| i.finished_at.clone());
        let exit_code = inspected.as_ref().and_then(|i| i.exit_code);
        let error = inspected.as_ref().and_then(|i| i.error.clone());
        let health = inspected.as_ref().and_then(|i| i.health.clone());

        // Map the Docker state to the K8s container-state shape the frontend's
        // `ContainerState` interface expects.
        let state = match status {
            "running" => serde_json::json!({
                "state_type": "running",
                "started_at": started_at,
                "finished_at": serde_json::Value::Null,
                "exit_code": serde_json::Value::Null,
                // Surface a Docker HEALTHCHECK verdict (when an image ships one)
                // as the running-state reason — e.g. "starting"/"unhealthy".
                // `none`/`healthy` (the common case; Rise injects no HEALTHCHECK)
                // leave the reason null.
                "reason": health
                    .filter(|h| h != "none" && h != "healthy"),
            }),
            "exited" | "dead" => serde_json::json!({
                "state_type": "terminated",
                "started_at": started_at,
                "finished_at": finished_at,
                "exit_code": exit_code,
                // Prefer the daemon's error string; otherwise synthesize from
                // the exit code so a non-zero exit still has a visible reason.
                "reason": error.clone().or_else(|| {
                    exit_code.map(|c| if c == 0 { "Completed".to_string() } else { format!("Error (exit {c})") })
                }),
            }),
            // created / restarting / unknown / missing → waiting.
            _ => serde_json::json!({
                "state_type": "waiting",
                "started_at": serde_json::Value::Null,
                "finished_at": serde_json::Value::Null,
                "exit_code": serde_json::Value::Null,
                "reason": if status.is_empty() { "ContainerCreating".to_string() } else { status.to_string() },
            }),
        };

        pods.push(serde_json::json!({
            "name": name,
            "phase": phase,
            "conditions": [],
            "containers": [{
                "name": name,
                "ready": container_ready,
                "restart_count": restart_count,
                "state": state,
            }],
        }));
    }

    serde_json::json!({
        "desired_replicas": named.len(),
        "current_replicas": current_replicas,
        "ready_replicas": ready_replicas,
        "pods": pods,
        "last_checked": Utc::now().to_rfc3339(),
    })
}

// ── Pure diff ───────────────────────────────────────────────────────────

/// Observed Rise-managed container, reduced to the fields the diff needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActualContainer {
    pub id: String,
    pub name: String,
    /// The `project` bookkeeping label. Part of the stable identity tuple used
    /// to match this container to a desired one across generations (the live
    /// `--name` carries a per-recreate `_g{n}` suffix and can't be matched
    /// directly). `None` for legacy containers missing the label.
    pub project: Option<String>,
    /// The `deployment-group` bookkeeping label (identity tuple member).
    pub deployment_group: Option<String>,
    /// The `container` bookkeeping label (identity tuple member).
    pub container: Option<String>,
    /// The `deployment-id` bookkeeping label stamped on the live container.
    /// Used by the diff to protect a container from GC when its owning
    /// deployment's desired computation failed this tick, and as a member of the
    /// stable identity tuple. `None` for legacy containers created before this
    /// label was read (treated as unprotected).
    pub deployment_id_label: Option<String>,
    /// Monotonic generation parsed from the `generation` label. The NEXT
    /// generation on a recreate is `generation + 1`. Defaults to `0` when the
    /// label is missing/legacy/unparseable, so the first recreate of a legacy
    /// container yields generation `1`.
    pub generation: u32,
    /// Zero-based replica index parsed from the `replica` label. Member of the
    /// stable identity tuple, so each replica is matched/recreated independently.
    /// Defaults to `0` when the label is missing/legacy/unparseable, so a
    /// pre-replica container maps onto replica 0 of its spec.
    pub replica: u32,
    pub image_label: Option<String>,
    pub env_hash_label: Option<String>,
    /// The `route-hash` recreate-signature label stamped on the live container
    /// (sha256 of its rendered Traefik label set plus its published-port intent).
    /// `None` for legacy containers created before this label existed. Compared
    /// against the desired container's `route_hash` so a routing transition (a
    /// deployment becoming or ceasing to be active) OR a published-port change
    /// forces a recreate — Docker can't mutate a running container's labels or
    /// port bindings in place.
    pub route_hash_label: Option<String>,
    /// Daemon-reported lifecycle state (e.g. "running", "exited", "created",
    /// "dead", "restarting", "paused"). `None` when the daemon didn't report
    /// it. A container that matches on image + env but is in a terminal-ish
    /// state (`exited`/`created`/`dead`/`None`) is recreated; transient managed
    /// states (`running`/`restarting`/`paused`) are left alone — see
    /// `ActualContainer::needs_recreate`.
    pub state: Option<String>,
}

impl ActualContainer {
    /// Stable identity of this container from its bookkeeping LABELS — the tuple
    /// (project, deployment_group, deployment_id, container). Used to match an
    /// actual container to a desired one WITHOUT relying on the live `--name`,
    /// which now carries a per-recreate `_g{n}` generation suffix. Returns
    /// `None` when any identity label is missing (a legacy/foreign container
    /// that can't be matched — it's only ever a removable orphan, if not
    /// protected).
    fn identity(&self) -> Option<String> {
        Some(identity_key(
            self.project.as_deref()?,
            self.deployment_group.as_deref()?,
            self.deployment_id_label.as_deref()?,
            self.container.as_deref()?,
            self.replica,
        ))
    }

    /// Spec-level key (identity minus replica) for grouping replicas of a spec.
    /// `None` when any spec-identity label is missing (a legacy/foreign
    /// container that can't be grouped).
    fn spec_identity(&self) -> Option<String> {
        Some(spec_key(
            self.project.as_deref()?,
            self.deployment_group.as_deref()?,
            self.deployment_id_label.as_deref()?,
            self.container.as_deref()?,
        ))
    }

    /// Whether the daemon reports this container in a terminal-ish state that
    /// won't self-recover, so the reconciler must recreate it.
    ///
    /// Only `exited`/`created`/`dead` (and an unreported state, `None`) qualify:
    /// these never start serving again on their own. Transient daemon-managed
    /// states — `running`, `restarting` (Docker's restart policy applying
    /// backoff after a crash) and `paused` (operator-initiated) — must NOT be
    /// force-recreated: doing so every reconcile tick would reset Docker's
    /// restart history and fight its exponential backoff. The health probe
    /// drives the Unhealthy status independently, so a crash-looping container
    /// is reflected in status without the reconciler churning it.
    fn needs_recreate(&self) -> bool {
        matches!(
            self.state.as_deref(),
            Some("exited") | Some("created") | Some("dead") | None
        )
    }
}

/// What to do with one container slot.
///
/// Create/Recreate carry the stable `identity` (so `apply_actions` can find the
/// desired container by its identity tuple), the resolved generation-ful `name`
/// (for logging/clarity), and the `generation` to stamp on the new container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileAction {
    /// No matching actual container — create + start it at `generation`.
    Create {
        identity: String,
        name: String,
        generation: u32,
    },
    /// Actual exists but drifted (image / env-hash / route-hash / not running)
    /// — replace it, bumping to `generation` (live generation + 1).
    Recreate {
        identity: String,
        name: String,
        existing_id: String,
        generation: u32,
    },
    /// Actual exists with no desired match — garbage-collect it.
    Remove { id: String, name: String },
}

/// Classify each desired/actual container into create / recreate / remove.
///
/// Pure: no daemon access. Matching is by the STABLE IDENTITY tuple (project,
/// deployment_group, deployment_id, container) read from bookkeeping labels —
/// NOT by container name, which now carries a per-recreate `_g{n}` generation
/// suffix. `container_prefix` is the configured prefix used only to render the
/// resolved generation-ful `name` carried on each Create/Recreate action (for
/// logging); `apply_actions` looks the desired container up by `identity`.
///
/// `protected_deployment_ids` is the set of deployment-ids whose desired
/// computation FAILED this tick. An actual container whose `deployment-id`
/// label is in this set is never classified as `Remove`, even though it has no
/// desired match — its absence from `desired` is an artifact of the transient
/// failure, not a genuine orphan. This keeps healthy, actively-serving
/// containers running across a brief KMS/DB hiccup, matching the K8s model
/// (children untouched on a failed sync). Containers for deployments that
/// successfully computed and are genuinely orphaned still get GC'd.
pub fn diff_desired_vs_actual(
    desired: &[DesiredContainer],
    actual: &[ActualContainer],
    container_prefix: &str,
    protected_deployment_ids: &HashSet<String>,
) -> Vec<ReconcileAction> {
    // Desired containers keyed by their STABLE IDENTITY tuple (project, group,
    // deployment-id, container) — NOT by their live `--name`, which now carries
    // a per-recreate `_g{n}` generation suffix that differs between an actual
    // container and the next-generation name we'd compute for it.
    let mut desired_by_identity: HashMap<String, &DesiredContainer> = HashMap::new();
    for d in desired {
        let key = identity_key(
            &d.project,
            &d.deployment_group,
            &d.deployment_id,
            &d.container,
            d.replica,
        );
        desired_by_identity.insert(key, d);
    }

    // Actual containers keyed by the identity tuple read from their bookkeeping
    // labels. Containers missing identity labels (`identity() == None`) are not
    // indexed here — they can never match a desired slot and are only ever
    // removable orphans (handled in the GC pass below).
    let actual_by_identity: HashMap<String, &ActualContainer> = actual
        .iter()
        .filter_map(|a| a.identity().map(|id| (id, a)))
        .collect();

    let mut actions = Vec::new();
    let mut matched_actual_ids: HashSet<String> = HashSet::new();

    for (identity, d) in &desired_by_identity {
        match actual_by_identity.get(identity) {
            None => {
                // Brand-new slot: create at generation 1.
                actions.push(ReconcileAction::Create {
                    identity: identity.clone(),
                    name: container_builder::container_name(
                        container_prefix,
                        &d.project,
                        &d.deployment_group,
                        &d.deployment_id,
                        &d.container,
                        d.replica,
                        1,
                    ),
                    generation: 1,
                });
            }
            Some(a) => {
                matched_actual_ids.insert(a.id.clone());
                let image_drift = a.image_label.as_deref() != Some(d.image.as_str());
                let env_drift = a.env_hash_label.as_deref() != Some(d.env_hash.as_str());
                // Recreate-signature drift: the live container's stamped
                // `route-hash` differs from the desired one. This makes
                // create-time-only properties a first-class part of the diff —
                // when a deployment becomes active (gains Traefik labels) or
                // stops being active (loses them), OR when its app port starts/
                // stops being published to a loopback host port, the hash changes
                // and forces a recreate to apply it. Docker can't mutate a running
                // container's labels or port bindings in place, so recreation is
                // the only way. Idempotent: once the stamped hash equals the
                // desired one nothing recreates. A legacy container missing the
                // label (`None`) is recreated once to gain it, then converges.
                //
                // NOTE: the generation is deliberately NOT part of any of these
                // drift checks (it isn't fed into image/env/route hashes either),
                // so a stable container produces ZERO actions regardless of its
                // `_g{n}` suffix — the key no-infinite-recreate invariant.
                let route_drift = a.route_hash_label.as_deref() != Some(d.route_hash.as_str());
                // A container that matches on image + env but is in a
                // terminal-ish state (created-but-never-started, exited, or
                // crash-give-up `dead`) must be recreated so the deployment
                // recovers. Transient managed states (`restarting`, `paused`)
                // are left to Docker's restart policy so its backoff can settle.
                let not_running = a.needs_recreate();
                if image_drift || env_drift || route_drift || not_running {
                    // Bump to the next generation so the replacement's name is
                    // visibly newer than the one it replaces.
                    let generation = a.generation + 1;
                    actions.push(ReconcileAction::Recreate {
                        identity: identity.clone(),
                        name: container_builder::container_name(
                            container_prefix,
                            &d.project,
                            &d.deployment_group,
                            &d.deployment_id,
                            &d.container,
                            d.replica,
                            generation,
                        ),
                        existing_id: a.id.clone(),
                        generation,
                    });
                }
            }
        }
    }

    for a in actual {
        if matched_actual_ids.contains(&a.id) {
            continue;
        }
        // Protect containers belonging to a deployment whose desired
        // computation failed this tick: treat them as matched rather than
        // orphaned, so a transient error can't GC a healthy container.
        if a.deployment_id_label
            .as_deref()
            .is_some_and(|id| protected_deployment_ids.contains(id))
        {
            continue;
        }
        actions.push(ReconcileAction::Remove {
            id: a.id.clone(),
            name: a.name.clone(),
        });
    }

    // Deterministic ordering for testability.
    actions.sort_by_key(action_key);
    actions
}

/// Apply the ROLLING-RECREATE throttle to a diff's actions, run AFTER
/// [`diff_desired_vs_actual`] and BEFORE `apply_actions`. Enforces, per spec
/// (grouped by the spec key = identity minus replica), that a *running but
/// drifted* replica is recreated at most ONE-AT-A-TIME and only while every
/// OTHER replica of that spec is currently HEALTHY — so a rollout never drops
/// more than one replica of capacity at once.
///
/// Pass-through rules (PER SPEC):
/// - **Create** / **Remove** → ALWAYS pass (initial rollout / scale-up add
///   capacity; scale-down / GC remove it). Never throttled.
/// - **Recreate** of a recovery replica — the matched live container's `state`
///   is NOT "running" (crashed / exited / created / missing) → ALWAYS pass; it's
///   already down, so recreating only restores capacity.
/// - **Recreate** of a rollout replica — the matched live container IS "running"
///   but drifted → emit AT MOST ONE per spec this tick, the lowest replica index,
///   and ONLY IF every OTHER replica of the spec is HEALTHY
///   (`healthy_by_identity`). If ANY sibling is unhealthy/starting, defer ALL
///   rollout recreates for the spec this tick.
///
/// INVARIANT: for a single spec with R running+drifted+healthy replicas this
/// yields exactly ONE rollout Recreate; with any sibling unhealthy it yields
/// ZERO; recovery Recreates and all Creates/Removes pass regardless.
///
/// `actual` provides each container's run state + identity (to map an action's
/// identity → its spec + replica index + run state). `healthy_by_identity` maps
/// an `identity_key(...)` to whether that live replica passed the HTTP health
/// probe this tick (absent → treated as NOT healthy).
fn filter_rolling_actions(
    actions: Vec<ReconcileAction>,
    actual: &[ActualContainer],
    healthy_by_identity: &HashMap<String, bool>,
) -> Vec<ReconcileAction> {
    // identity → the live container (for run state + replica/spec grouping).
    let actual_by_identity: HashMap<String, &ActualContainer> = actual
        .iter()
        .filter_map(|a| a.identity().map(|id| (id, a)))
        .collect();

    // Collect rollout-recreate candidates per spec; pass everything else through.
    // A candidate is (replica_index, action) where the matched live container is
    // running. We then admit at most one per spec, gated on sibling health.
    let mut passed: Vec<ReconcileAction> = Vec::new();
    // spec_key → Vec<(replica, identity, action)> of running+drifted recreates.
    let mut rollout_candidates: HashMap<String, Vec<(u32, String, ReconcileAction)>> =
        HashMap::new();

    for action in actions {
        match &action {
            ReconcileAction::Create { .. } | ReconcileAction::Remove { .. } => {
                // Unthrottled: capacity add (Create) / scale-down or GC (Remove).
                passed.push(action);
            }
            ReconcileAction::Recreate { identity, .. } => {
                match actual_by_identity.get(identity) {
                    Some(a) if a.state.as_deref() == Some("running") => {
                        // Rollout candidate: a running, drifted replica. Defer the
                        // admission decision until we've seen all of them per spec.
                        let key = a.spec_identity().unwrap_or_else(|| identity.clone());
                        rollout_candidates.entry(key).or_default().push((
                            a.replica,
                            identity.clone(),
                            action,
                        ));
                    }
                    // Recovery (not running) OR no matched live container (already
                    // gone) → pass unthrottled: recreating restores capacity.
                    _ => passed.push(action),
                }
            }
        }
    }

    // Admit at most one rollout recreate per spec, the lowest replica index, and
    // only when every OTHER replica of the spec is healthy.
    for (spec, mut candidates) in rollout_candidates {
        candidates.sort_by_key(|(replica, _, _)| *replica);
        // Identities of this spec's replicas slated for a rollout recreate — they
        // don't count as "must be healthy" siblings (they're already drifted and
        // about to be replaced), so a uniform drift across all replicas still
        // rolls one-by-one rather than deadlocking.
        let candidate_identities: HashSet<String> =
            candidates.iter().map(|(_, id, _)| id.clone()).collect();

        // Gate: every OTHER replica of this spec (a live replica NOT itself a
        // rollout candidate) must be healthy. If any such sibling is
        // unhealthy/starting, defer ALL rollout recreates for this spec.
        let siblings_healthy = actual
            .iter()
            .filter(|a| a.spec_identity().as_deref() == Some(spec.as_str()))
            .filter_map(|a| a.identity())
            .filter(|id| !candidate_identities.contains(id))
            .all(|id| healthy_by_identity.get(&id).copied().unwrap_or(false));

        if siblings_healthy {
            if let Some((_, _, action)) = candidates.into_iter().next() {
                passed.push(action);
            }
        }
        // else: defer ALL of this spec's rollout recreates this tick.
    }

    passed.sort_by_key(action_key);
    passed
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

fn action_key(a: &ReconcileAction) -> (u8, String) {
    match a {
        ReconcileAction::Create { name, .. } => (0, name.clone()),
        ReconcileAction::Recreate { name, .. } => (1, name.clone()),
        ReconcileAction::Remove { name, .. } => (2, name.clone()),
    }
}

/// Stable identity key for a container slot, joining the identity tuple
/// (project, deployment_group, deployment_id, container, replica) with a NUL
/// separator (which can't appear in any of these names). Built from desired
/// fields and from actual bookkeeping labels so the two match across name
/// generations. The replica index is the matching unit: each replica of a spec
/// is reconciled independently.
fn identity_key(
    project: &str,
    group: &str,
    deployment_id: &str,
    container: &str,
    replica: u32,
) -> String {
    format!("{project}\u{0}{group}\u{0}{deployment_id}\u{0}{container}\u{0}{replica}")
}

/// Spec-level key = the identity tuple MINUS the replica index
/// (project, group, deployment_id, container). Groups all replicas of a single
/// spec together — used by the rolling-recreate throttle to reason per-spec.
fn spec_key(project: &str, group: &str, deployment_id: &str, container: &str) -> String {
    format!("{project}\u{0}{group}\u{0}{deployment_id}\u{0}{container}")
}

/// Resolve a spec's requested replica count to the actual number of containers
/// to run: default 1 (unset), clamped to `[1, MAX_REPLICAS]`. Shared by desired
/// computation and health aggregation so both always agree on the replica count.
fn clamp_replicas(requested: Option<u32>) -> u32 {
    requested.unwrap_or(1).clamp(1, MAX_REPLICAS)
}

// ── Env merge helpers ─────────────────────────────────────────────────

/// Merge env for one container in final precedence:
/// base (plain + secret) → system env → per-container overrides. Later writes
/// win on key conflict.
/// Concise, human-readable detail for a probe `reqwest::Error`: prefer the
/// underlying transport source (e.g. "connection refused", "timed out") over
/// reqwest's verbose wrapper text, so the surfaced reason is actionable.
fn probe_error_detail(e: &reqwest::Error) -> String {
    use std::error::Error;
    if let Some(src) = e.source() {
        src.to_string()
    } else if e.is_timeout() {
        "timed out".to_string()
    } else if e.is_connect() {
        "connection error".to_string()
    } else {
        e.to_string()
    }
}

/// Effective HTTP health-probe path for a container, honoring its public
/// `health_check` spec (`rise.toml [containers.X.health_check]`):
///
/// - `disabled = true` → `None`: the probe is turned off (the caller treats a
///   *running* container as ready), matching the Kubernetes behavior where
///   `health_check = false` removes the readiness/liveness probes.
/// - `path` set → that path, normalized to a leading `/`.
/// - otherwise → the controller's default `health_path`.
///
/// Note: the reconcile loop probes once per tick, so the spec's fine-grained
/// timing/threshold knobs (`period_seconds`, `failure_threshold`, …) and the
/// separate `liveness_enabled`/`readiness_enabled` toggles are not applied on
/// Docker — only `path` and `disabled` are. See the deployment-backends feature
/// matrix.
fn effective_health_path(
    spec: &crate::server::deployment::models::ContainerSpec,
    default_path: &str,
) -> Option<String> {
    let hc = spec.health_check.as_ref();
    if hc.map(|h| h.disabled).unwrap_or(false) {
        return None;
    }
    match hc.and_then(|h| h.path.as_deref()).filter(|p| !p.is_empty()) {
        Some(p) if p.starts_with('/') => Some(p.to_string()),
        Some(p) => Some(format!("/{p}")),
        None => Some(default_path.to_string()),
    }
}

fn merge_container_env(
    base_env: &[(String, String)],
    system_env: &[(String, String)],
    injected_hosts: &[(String, String)],
    spec: &crate::server::deployment::models::ContainerSpec,
    env_name: Option<&str>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = base_env.to_vec();
    for (k, v) in system_env {
        upsert_env(&mut env, k, v);
    }
    // Cross-container service-discovery hosts (`RISE_CONTAINER_HOST__<NAME>`).
    // Only added when not already present, so user globals win; per-container
    // `env_overrides` below run last and can shadow them too — matching the K8s
    // precedence in `webhook.rs`.
    for (k, v) in injected_hosts {
        if !env.iter().any(|(ek, _)| ek == k) {
            env.push((k.clone(), v.clone()));
        }
    }
    for over in &spec.env_overrides {
        // Per-container secret overrides are rejected at request time.
        if over.is_secret {
            continue;
        }
        if let Some(ref target_env) = over.for_environment {
            if env_name != Some(target_env.as_str()) {
                continue;
            }
        }
        upsert_env(&mut env, &over.key, &over.value);
    }
    env
}

fn upsert_env(env: &mut Vec<(String, String)>, key: &str, value: &str) {
    if let Some(existing) = env.iter_mut().find(|(k, _)| k == key) {
        existing.1 = value.to_string();
    } else {
        env.push((key.to_string(), value.to_string()));
    }
}

/// Stable sha256 of a merged env vector, used as the drift label. Hashes the
/// *entire* set (plain + system/RISE_* + secret) over a deterministically
/// key-sorted copy with length-prefixed key/value framing, so reordering can't
/// change the digest while any add/edit/delete of any variable does. Editing or
/// deleting any env var therefore changes the `env-hash` label and forces the
/// reconciler to recreate the container.
pub fn hash_env(env: &[(String, String)]) -> String {
    let mut sorted: Vec<&(String, String)> = env.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let mut hasher = Sha256::new();
    for (k, v) in sorted {
        hasher.update((k.len() as u64).to_le_bytes());
        hasher.update(k.as_bytes());
        hasher.update((v.len() as u64).to_le_bytes());
        hasher.update(v.as_bytes());
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::deployment::controller::docker::container_builder::DesiredRoute;

    fn desired(container: &str, image: &str, hash: &str) -> DesiredContainer {
        DesiredContainer {
            project: "myapp".to_string(),
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
        }
    }

    /// Stable identity key for the `desired()` helper's slot, used to build the
    /// `identity` field of expected Create/Recreate actions.
    fn identity_of(d: &DesiredContainer) -> String {
        identity_key(
            &d.project,
            &d.deployment_group,
            &d.deployment_id,
            &d.container,
            d.replica,
        )
    }

    /// Empty protected-deployment-ids set for the common case where no
    /// deployment failed desired computation.
    fn no_protected() -> HashSet<String> {
        HashSet::new()
    }

    /// The resolved generation-ful name for the `desired()` slot at a given
    /// generation (the name a Create/Recreate action carries).
    fn name_of_gen(d: &DesiredContainer, generation: u32) -> String {
        container_builder::container_name(
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
    fn actual_for_gen(
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
            generation,
            replica: d.replica,
            image_label: Some(image.to_string()),
            env_hash_label: Some(env_hash.to_string()),
            route_hash_label: Some("rh-active".to_string()),
            state: Some("running".to_string()),
        }
    }

    /// A live container at generation 1 (the common case for most diff tests).
    fn actual_for(d: &DesiredContainer, image: &str, env_hash: &str) -> ActualContainer {
        actual_for_gen(d, image, env_hash, 1)
    }

    #[test]
    fn diff_creates_missing() {
        let d = desired("app", "img:1", "h1");
        let actions =
            diff_desired_vs_actual(std::slice::from_ref(&d), &[], "rise", &no_protected());
        assert_eq!(
            actions,
            vec![ReconcileAction::Create {
                identity: identity_of(&d),
                name: name_of_gen(&d, 1),
                generation: 1,
            }]
        );
    }

    #[test]
    fn diff_no_action_when_matched() {
        let d = desired("app", "img:1", "h1");
        let actual = vec![actual_for(&d, "img:1", "h1")];
        let actions = diff_desired_vs_actual(&[d], &actual, "rise", &no_protected());
        assert!(actions.is_empty());
    }

    #[test]
    fn diff_recreates_on_image_drift() {
        let d = desired("app", "img:2", "h1");
        let actual = vec![actual_for(&d, "img:1", "h1")];
        let actions =
            diff_desired_vs_actual(std::slice::from_ref(&d), &actual, "rise", &no_protected());
        assert_eq!(
            actions,
            vec![ReconcileAction::Recreate {
                identity: identity_of(&d),
                name: name_of_gen(&d, 2),
                existing_id: "cid".to_string(),
                generation: 2,
            }]
        );
    }

    #[test]
    fn diff_recreates_on_env_hash_drift() {
        let d = desired("app", "img:1", "h2");
        let actual = vec![actual_for(&d, "img:1", "h1")];
        let actions =
            diff_desired_vs_actual(std::slice::from_ref(&d), &actual, "rise", &no_protected());
        assert_eq!(
            actions,
            vec![ReconcileAction::Recreate {
                identity: identity_of(&d),
                name: name_of_gen(&d, 2),
                existing_id: "cid".to_string(),
                generation: 2,
            }]
        );
    }

    #[test]
    fn diff_recreates_when_matched_but_not_running() {
        // Image + env match, but the container exited / never started — it must
        // be recreated so the deployment recovers.
        let d = desired("app", "img:1", "h1");
        for state in [Some("exited"), Some("created"), Some("dead"), None] {
            let actual = vec![ActualContainer {
                state: state.map(str::to_string),
                ..actual_for(&d, "img:1", "h1")
            }];
            let actions =
                diff_desired_vs_actual(std::slice::from_ref(&d), &actual, "rise", &no_protected());
            assert_eq!(
                actions,
                vec![ReconcileAction::Recreate {
                    identity: identity_of(&d),
                    name: name_of_gen(&d, 2),
                    existing_id: "cid".to_string(),
                    generation: 2,
                }],
                "state {state:?} should force recreate"
            );
        }
    }

    #[test]
    fn diff_leaves_transient_states_to_docker() {
        // Image + env match and the container is in a daemon-managed transient
        // state: `running` (healthy), `restarting` (Docker's restart policy
        // applying backoff after a crash) or `paused` (operator-initiated).
        // None of these may be force-recreated — doing so each tick would reset
        // Docker's restart backoff. The health probe drives Unhealthy status
        // independently.
        let d = desired("app", "img:1", "h1");
        for state in [Some("running"), Some("restarting"), Some("paused")] {
            let actual = vec![ActualContainer {
                state: state.map(str::to_string),
                ..actual_for(&d, "img:1", "h1")
            }];
            let actions =
                diff_desired_vs_actual(std::slice::from_ref(&d), &actual, "rise", &no_protected());
            assert!(
                actions.is_empty(),
                "state {state:?} must not force a recreate"
            );
        }
    }

    #[test]
    fn diff_removes_orphan() {
        let actual = vec![ActualContainer {
            id: "old".to_string(),
            name: "rise_myapp_default_oldid_app_g1".to_string(),
            project: Some("myapp".to_string()),
            deployment_group: Some("default".to_string()),
            container: Some("app".to_string()),
            deployment_id_label: Some("oldid".to_string()),
            generation: 1,
            replica: 0,
            image_label: Some("img:1".to_string()),
            env_hash_label: Some("h1".to_string()),
            route_hash_label: Some("rh-active".to_string()),
            state: Some("running".to_string()),
        }];
        let actions = diff_desired_vs_actual(&[], &actual, "rise", &no_protected());
        assert_eq!(
            actions,
            vec![ReconcileAction::Remove {
                id: "old".to_string(),
                name: "rise_myapp_default_oldid_app_g1".to_string()
            }]
        );
    }

    #[test]
    fn diff_supersession_creates_new_removes_old() {
        // New deployment desired; old deployment's container still present.
        let new = desired("app", "img:2", "h2");
        let actual = vec![ActualContainer {
            id: "old".to_string(),
            name: "rise_myapp_default_oldid_app_g1".to_string(),
            project: Some("myapp".to_string()),
            deployment_group: Some("default".to_string()),
            container: Some("app".to_string()),
            deployment_id_label: Some("oldid".to_string()),
            generation: 1,
            replica: 0,
            image_label: Some("img:1".to_string()),
            env_hash_label: Some("h1".to_string()),
            route_hash_label: Some("rh-active".to_string()),
            state: Some("running".to_string()),
        }];
        let actions =
            diff_desired_vs_actual(std::slice::from_ref(&new), &actual, "rise", &no_protected());
        assert_eq!(
            actions,
            vec![
                ReconcileAction::Create {
                    identity: identity_of(&new),
                    name: name_of_gen(&new, 1),
                    generation: 1,
                },
                ReconcileAction::Remove {
                    id: "old".to_string(),
                    name: "rise_myapp_default_oldid_app_g1".to_string()
                },
            ]
        );
    }

    #[test]
    fn diff_protects_container_when_its_deployment_failed_to_compute() {
        // A deployment whose desired computation failed this tick (e.g. a brief
        // KMS/DB hiccup) is absent from `desired`, so its still-running container
        // looks orphaned. With its deployment-id protected, the diff must NOT
        // remove it — matching the K8s model (children untouched on failed sync).
        let actual = vec![ActualContainer {
            id: "live".to_string(),
            name: "rise_myapp_default_protectedid_app_g1".to_string(),
            project: Some("myapp".to_string()),
            deployment_group: Some("default".to_string()),
            container: Some("app".to_string()),
            deployment_id_label: Some("protectedid".to_string()),
            generation: 1,
            replica: 0,
            image_label: Some("img:1".to_string()),
            env_hash_label: Some("h1".to_string()),
            route_hash_label: Some("rh-active".to_string()),
            state: Some("running".to_string()),
        }];
        let mut protected = HashSet::new();
        protected.insert("protectedid".to_string());
        let actions = diff_desired_vs_actual(&[], &actual, "rise", &protected);
        assert!(
            actions.is_empty(),
            "protected deployment's container must not be GC'd"
        );
    }

    #[test]
    fn diff_still_gcs_unprotected_orphan_alongside_protected() {
        // Only the failed deployment's container is protected; a genuinely
        // orphaned container belonging to a different, successfully-computed
        // deployment is still removed.
        let actual = vec![
            ActualContainer {
                id: "protected".to_string(),
                name: "rise_myapp_default_protectedid_app_g1".to_string(),
                project: Some("myapp".to_string()),
                deployment_group: Some("default".to_string()),
                container: Some("app".to_string()),
                deployment_id_label: Some("protectedid".to_string()),
                generation: 1,
                replica: 0,
                image_label: Some("img:1".to_string()),
                env_hash_label: Some("h1".to_string()),
                route_hash_label: Some("rh-active".to_string()),
                state: Some("running".to_string()),
            },
            ActualContainer {
                id: "orphan".to_string(),
                name: "rise_myapp_default_orphanid_app_g1".to_string(),
                project: Some("myapp".to_string()),
                deployment_group: Some("default".to_string()),
                container: Some("app".to_string()),
                deployment_id_label: Some("orphanid".to_string()),
                generation: 1,
                replica: 0,
                image_label: Some("img:1".to_string()),
                env_hash_label: Some("h1".to_string()),
                route_hash_label: Some("rh-active".to_string()),
                state: Some("running".to_string()),
            },
        ];
        let mut protected = HashSet::new();
        protected.insert("protectedid".to_string());
        let actions = diff_desired_vs_actual(&[], &actual, "rise", &protected);
        assert_eq!(
            actions,
            vec![ReconcileAction::Remove {
                id: "orphan".to_string(),
                name: "rise_myapp_default_orphanid_app_g1".to_string()
            }],
            "only the unprotected orphan should be removed"
        );
    }

    #[test]
    fn diff_recreates_on_route_hash_drift_active_gains_labels() {
        // A deployment that just became active: desired now carries the active
        // route-hash, but the live container was created while non-routable and
        // still bears the empty/non-routable hash. Image + env match, but the
        // routing changed — Docker can't edit labels in place, so the container
        // must be recreated WITH the Traefik labels.
        let d = desired("app", "img:1", "h1"); // route_hash = "rh-active"
        let actual = vec![ActualContainer {
            // Was created non-routable (empty route-hash).
            route_hash_label: Some(String::new()),
            ..actual_for(&d, "img:1", "h1")
        }];
        let actions =
            diff_desired_vs_actual(std::slice::from_ref(&d), &actual, "rise", &no_protected());
        assert_eq!(
            actions,
            vec![ReconcileAction::Recreate {
                identity: identity_of(&d),
                name: name_of_gen(&d, 2),
                existing_id: "cid".to_string(),
                generation: 2,
            }]
        );
    }

    #[test]
    fn diff_recreates_on_route_hash_drift_deactivated_loses_labels() {
        // A deployment that stopped being active: desired is now non-routable
        // (empty route-hash) while the live container still carries the active
        // routing hash. Image + env match, but its Traefik labels must be
        // removed — force a recreate.
        let mut d = desired("app", "img:1", "h1");
        d.routable = false;
        d.route_hash = String::new();
        let actual = vec![ActualContainer {
            route_hash_label: Some("rh-active".to_string()),
            ..actual_for(&d, "img:1", "h1")
        }];
        let actions =
            diff_desired_vs_actual(std::slice::from_ref(&d), &actual, "rise", &no_protected());
        assert_eq!(
            actions,
            vec![ReconcileAction::Recreate {
                identity: identity_of(&d),
                name: name_of_gen(&d, 2),
                existing_id: "cid".to_string(),
                generation: 2,
            }]
        );
    }

    #[test]
    fn diff_no_action_when_route_image_env_all_match() {
        // Idempotence: once image + env + route-hash all match and the container
        // is running, the diff emits NO action — no recreate oscillation.
        let d = desired("app", "img:1", "h1"); // route_hash = "rh-active"
        let actual = vec![actual_for(&d, "img:1", "h1")];
        let actions =
            diff_desired_vs_actual(std::slice::from_ref(&d), &actual, "rise", &no_protected());
        assert!(actions.is_empty(), "matching container must not recreate");
    }

    #[test]
    fn diff_no_action_when_matched_with_different_generation_suffix() {
        // The KEY anti-infinite-recreate test: a stable container (no image/env/
        // route drift, running) that was recreated several times — so its live
        // name carries `_g5` and its generation label is 5 — must still match the
        // desired slot by IDENTITY and produce ZERO actions. The generation never
        // feeds any matching key or hash, so the `_g{n}` suffix is irrelevant.
        let d = desired("app", "img:1", "h1"); // route_hash = "rh-active"
        let actual = vec![actual_for_gen(&d, "img:1", "h1", 5)];
        assert!(
            actual[0].name.ends_with("_g5"),
            "fixture must carry a non-1 generation suffix"
        );
        let actions =
            diff_desired_vs_actual(std::slice::from_ref(&d), &actual, "rise", &no_protected());
        assert!(
            actions.is_empty(),
            "a stable container must not recreate regardless of its _g{{n}} suffix"
        );
    }

    #[test]
    fn diff_recreate_bumps_generation_from_actual() {
        // A drifted match at generation 5 recreates at generation 6 (actual + 1),
        // and the carried name reflects the bumped generation.
        let d = desired("app", "img:2", "h1"); // image drift
        let actual = vec![actual_for_gen(&d, "img:1", "h1", 5)];
        let actions =
            diff_desired_vs_actual(std::slice::from_ref(&d), &actual, "rise", &no_protected());
        assert_eq!(
            actions,
            vec![ReconcileAction::Recreate {
                identity: identity_of(&d),
                name: name_of_gen(&d, 6),
                existing_id: "cid".to_string(),
                generation: 6,
            }]
        );
    }

    #[test]
    fn diff_create_uses_generation_one() {
        // A missing slot always creates at generation 1.
        let d = desired("app", "img:1", "h1");
        let actions =
            diff_desired_vs_actual(std::slice::from_ref(&d), &[], "rise", &no_protected());
        assert_eq!(
            actions,
            vec![ReconcileAction::Create {
                identity: identity_of(&d),
                name: name_of_gen(&d, 1),
                generation: 1,
            }]
        );
    }

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
    fn merge_env_precedence() {
        use crate::server::deployment::models::{ContainerSpec, EnvOverride};
        let base = vec![("FOO".to_string(), "base".to_string())];
        let system = vec![
            ("FOO".to_string(), "system".to_string()),
            ("RISE_APP_URL".to_string(), "url".to_string()),
        ];
        let spec = ContainerSpec {
            name: "app".to_string(),
            image: None,
            port: Some(8080),
            replicas: None,
            cpu: None,
            memory: None,
            env_overrides: vec![EnvOverride {
                key: "FOO".to_string(),
                value: "override".to_string(),
                is_secret: false,
                is_protected: None,
                source: None,
                for_environment: None,
            }],
            health_check: None,
        };
        let merged = merge_container_env(&base, &system, &[], &spec, None);
        let foo = merged.iter().find(|(k, _)| k == "FOO").unwrap();
        assert_eq!(foo.1, "override");
        assert!(merged
            .iter()
            .any(|(k, v)| k == "RISE_APP_URL" && v == "url"));
    }

    #[test]
    fn effective_health_path_honors_spec() {
        use crate::server::deployment::models::{ContainerSpec, HealthCheckSpec};
        let base = ContainerSpec {
            name: "app".to_string(),
            image: None,
            port: Some(8080),
            replicas: None,
            cpu: None,
            memory: None,
            env_overrides: vec![],
            health_check: None,
        };
        // No spec → controller default.
        assert_eq!(
            effective_health_path(&base, "/").as_deref(),
            Some("/"),
            "default path"
        );
        // disabled → None (probe turned off).
        let disabled = ContainerSpec {
            health_check: Some(HealthCheckSpec {
                disabled: true,
                ..Default::default()
            }),
            ..base.clone()
        };
        assert_eq!(effective_health_path(&disabled, "/"), None, "disabled");
        // explicit path used verbatim.
        let with_path = ContainerSpec {
            health_check: Some(HealthCheckSpec {
                path: Some("/healthz".to_string()),
                ..Default::default()
            }),
            ..base.clone()
        };
        assert_eq!(
            effective_health_path(&with_path, "/").as_deref(),
            Some("/healthz")
        );
        // path missing a leading slash is normalized.
        let no_slash = ContainerSpec {
            health_check: Some(HealthCheckSpec {
                path: Some("healthz".to_string()),
                ..Default::default()
            }),
            ..base.clone()
        };
        assert_eq!(
            effective_health_path(&no_slash, "/").as_deref(),
            Some("/healthz")
        );
        // empty path falls back to the default.
        let empty = ContainerSpec {
            health_check: Some(HealthCheckSpec {
                path: Some(String::new()),
                ..Default::default()
            }),
            ..base.clone()
        };
        assert_eq!(
            effective_health_path(&empty, "/livez").as_deref(),
            Some("/livez")
        );
    }

    #[test]
    fn merge_env_injects_container_hosts_unless_user_set() {
        use crate::server::deployment::models::{ContainerSpec, EnvOverride};
        // A user global and a per-container override both targeting the same
        // discovery key must win over the auto-injected sibling host.
        let base = vec![(
            "RISE_CONTAINER_HOST__API".to_string(),
            "user-global:1".to_string(),
        )];
        // Injected hosts use the GROUP-scoped, deployment-id-FREE alias
        // (`{prefix}_{project}_{group}_{container}`).
        let injected = vec![
            (
                "RISE_CONTAINER_HOST__API".to_string(),
                "rise_myapp_default_api:8080".to_string(),
            ),
            (
                "RISE_CONTAINER_HOST__REDIS".to_string(),
                "rise_myapp_default_redis:6379".to_string(),
            ),
        ];
        let spec = ContainerSpec {
            name: "web".to_string(),
            image: None,
            port: Some(8080),
            replicas: None,
            cpu: None,
            memory: None,
            env_overrides: vec![EnvOverride {
                key: "RISE_CONTAINER_HOST__REDIS".to_string(),
                value: "override-redis:6379".to_string(),
                is_secret: false,
                is_protected: None,
                source: None,
                for_environment: None,
            }],
            health_check: None,
        };
        let merged = merge_container_env(&base, &[], &injected, &spec, None);
        // User global wins; injected value is not appended a second time.
        let api: Vec<_> = merged
            .iter()
            .filter(|(k, _)| k == "RISE_CONTAINER_HOST__API")
            .collect();
        assert_eq!(api.len(), 1);
        assert_eq!(api[0].1, "user-global:1");
        // Per-container override shadows the injected sibling host.
        let redis = merged
            .iter()
            .find(|(k, _)| k == "RISE_CONTAINER_HOST__REDIS")
            .unwrap();
        assert_eq!(redis.1, "override-redis:6379");
    }

    #[test]
    fn merge_env_skips_non_matching_environment_override() {
        use crate::server::deployment::models::{ContainerSpec, EnvOverride};
        let spec = ContainerSpec {
            name: "app".to_string(),
            image: None,
            port: None,
            replicas: None,
            cpu: None,
            memory: None,
            env_overrides: vec![EnvOverride {
                key: "ONLY_PROD".to_string(),
                value: "1".to_string(),
                is_secret: false,
                is_protected: None,
                source: None,
                for_environment: Some("production".to_string()),
            }],
            health_check: None,
        };
        let merged = merge_container_env(&[], &[], &[], &spec, Some("staging"));
        assert!(!merged.iter().any(|(k, _)| k == "ONLY_PROD"));
        let merged_prod = merge_container_env(&[], &[], &[], &spec, Some("production"));
        assert!(merged_prod.iter().any(|(k, _)| k == "ONLY_PROD"));
    }

    #[test]
    fn hash_env_is_order_independent_but_value_sensitive() {
        let a = vec![
            ("A".to_string(), "1".to_string()),
            ("B".to_string(), "2".to_string()),
        ];
        let b = vec![
            ("B".to_string(), "2".to_string()),
            ("A".to_string(), "1".to_string()),
        ];
        // Reordering the same set yields the same hash.
        assert_eq!(hash_env(&a), hash_env(&b));
        // Changing a value changes the hash.
        let changed = vec![
            ("A".to_string(), "1".to_string()),
            ("B".to_string(), "3".to_string()),
        ];
        assert_ne!(hash_env(&a), hash_env(&changed));
        // Deleting a var changes the hash.
        let deleted = vec![("A".to_string(), "1".to_string())];
        assert_ne!(hash_env(&a), hash_env(&deleted));
        // Adding a plain var changes the hash (the core drift bug this fixes).
        let mut added = a.clone();
        added.push(("C".to_string(), "3".to_string()));
        assert_ne!(hash_env(&a), hash_env(&added));
    }

    #[test]
    fn hash_env_avoids_delimiter_collisions() {
        // Length-prefixed framing means `{A:"B", : "C"}`-style splits can't
        // collide with `{A:"BC"}`-style merges.
        let split = vec![
            ("A".to_string(), "B".to_string()),
            ("C".to_string(), "D".to_string()),
        ];
        let merged = vec![("A".to_string(), "BCD".to_string())];
        assert_ne!(hash_env(&split), hash_env(&merged));
    }

    fn inspected_running() -> InspectedContainer {
        InspectedContainer {
            status: Some("running".to_string()),
            running: true,
            started_at: Some("2026-01-01T12:00:00Z".to_string()),
            finished_at: None,
            exit_code: None,
            restart_count: Some(0),
            health: None,
            error: None,
            ip: Some("172.20.0.5".to_string()),
            published_host_port: Some("49153".to_string()),
        }
    }

    fn inspected_exited(code: i64) -> InspectedContainer {
        InspectedContainer {
            status: Some("exited".to_string()),
            running: false,
            started_at: Some("2026-01-01T12:00:00Z".to_string()),
            finished_at: Some("2026-01-01T12:00:05Z".to_string()),
            exit_code: Some(code),
            restart_count: Some(2),
            health: None,
            error: None,
            ip: None,
            published_host_port: None,
        }
    }

    #[test]
    fn build_pod_status_running_maps_running_phase_and_state() {
        let named = vec![("app".to_string(), Some(inspected_running()))];
        let ps = build_pod_status(&named, true);

        assert_eq!(ps["desired_replicas"], 1);
        assert_eq!(ps["current_replicas"], 1);
        assert_eq!(ps["ready_replicas"], 1);

        let pod = &ps["pods"][0];
        assert_eq!(pod["name"], "app");
        assert_eq!(pod["phase"], "Running");
        assert_eq!(pod["conditions"], serde_json::json!([]));

        let c = &pod["containers"][0];
        assert_eq!(c["name"], "app");
        assert_eq!(c["ready"], true);
        assert_eq!(c["restart_count"], 0);
        assert_eq!(c["state"]["state_type"], "running");
        assert_eq!(c["state"]["started_at"], "2026-01-01T12:00:00Z");
    }

    #[test]
    fn build_pod_status_exited_maps_failed_phase_and_terminated_state() {
        let named = vec![("app".to_string(), Some(inspected_exited(1)))];
        // Even if the overall verdict were true, a non-running container is not
        // counted ready; here the verdict is false anyway.
        let ps = build_pod_status(&named, false);

        assert_eq!(ps["desired_replicas"], 1);
        assert_eq!(ps["current_replicas"], 0);
        assert_eq!(ps["ready_replicas"], 0);

        let pod = &ps["pods"][0];
        assert_eq!(pod["phase"], "Failed");

        let c = &pod["containers"][0];
        assert_eq!(c["ready"], false);
        assert_eq!(c["restart_count"], 2);
        assert_eq!(c["state"]["state_type"], "terminated");
        assert_eq!(c["state"]["exit_code"], 1);
        assert_eq!(c["state"]["finished_at"], "2026-01-01T12:00:05Z");
        // Non-zero exit synthesizes a reason when the daemon gave none.
        assert_eq!(c["state"]["reason"], "Error (exit 1)");
    }

    #[test]
    fn build_pod_status_missing_inspection_is_waiting_unknown() {
        // A container with no inspection (not yet created) → Unknown phase,
        // waiting state, not running, not ready.
        let named = vec![("app".to_string(), None)];
        let ps = build_pod_status(&named, true);
        assert_eq!(ps["current_replicas"], 0);
        assert_eq!(ps["ready_replicas"], 0);
        let pod = &ps["pods"][0];
        assert_eq!(pod["phase"], "Unknown");
        assert_eq!(pod["containers"][0]["state"]["state_type"], "waiting");
        assert_eq!(pod["containers"][0]["state"]["reason"], "ContainerCreating");
    }

    #[test]
    fn build_pod_status_running_but_not_ready_overall_is_not_container_ready() {
        // The container is running but the deployment verdict is false (another
        // container failing): this container is current but not ready.
        let named = vec![("app".to_string(), Some(inspected_running()))];
        let ps = build_pod_status(&named, false);
        assert_eq!(ps["current_replicas"], 1);
        assert_eq!(ps["ready_replicas"], 0);
        assert_eq!(ps["pods"][0]["containers"][0]["ready"], false);
    }

    // ── Replicas: diff scale up/down + rolling-recreate throttle ───────────

    /// Build the `desired()` slot for a specific replica index.
    fn desired_replica(replica: u32) -> DesiredContainer {
        let mut d = desired("app", "img:1", "h1");
        d.replica = replica;
        d
    }

    /// A live (matched) container for the given replica/state/image. Carries the
    /// full identity-label set including the replica so the diff matches it.
    fn actual_replica(replica: u32, state: &str, image: &str) -> ActualContainer {
        let d = desired_replica(replica);
        ActualContainer {
            id: format!("cid-r{replica}"),
            state: Some(state.to_string()),
            ..actual_for(&d, image, "h1")
        }
    }

    #[test]
    fn diff_scale_up_creates_new_replica_indices() {
        // Desired 3 replicas (r0..r2); only r0 exists → r1 and r2 are Created,
        // r0 is left untouched (image/env/route all match).
        let desired_all = vec![desired_replica(0), desired_replica(1), desired_replica(2)];
        let actual = vec![actual_replica(0, "running", "img:1")];
        let actions = diff_desired_vs_actual(&desired_all, &actual, "rise", &no_protected());
        let creates: Vec<u32> = actions
            .iter()
            .filter_map(|a| match a {
                ReconcileAction::Create { name, .. } if name.contains("_r1_") => Some(1),
                ReconcileAction::Create { name, .. } if name.contains("_r2_") => Some(2),
                _ => None,
            })
            .collect();
        assert_eq!(
            creates,
            vec![1, 2],
            "scale-up creates the new replica slots"
        );
        assert!(
            !actions.iter().any(|a| matches!(
                a,
                ReconcileAction::Recreate { .. } | ReconcileAction::Remove { .. }
            )),
            "scale-up must not recreate or remove the existing replica"
        );
    }

    #[test]
    fn diff_scale_down_removes_surplus_replicas() {
        // Desired 1 replica (r0); r0..r2 exist → r1 and r2 are surplus → Removed.
        let desired_all = vec![desired_replica(0)];
        let actual = vec![
            actual_replica(0, "running", "img:1"),
            actual_replica(1, "running", "img:1"),
            actual_replica(2, "running", "img:1"),
        ];
        let actions = diff_desired_vs_actual(&desired_all, &actual, "rise", &no_protected());
        let mut removed: Vec<String> = actions
            .iter()
            .filter_map(|a| match a {
                ReconcileAction::Remove { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        removed.sort();
        assert_eq!(removed, vec!["cid-r1".to_string(), "cid-r2".to_string()]);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, ReconcileAction::Create { .. })),
            "scale-down adds no containers"
        );
    }

    #[test]
    fn diff_scale_down_respects_protected_deployment_ids() {
        // A surplus replica whose deployment-id is protected is NOT removed.
        let desired_all = vec![desired_replica(0)];
        let actual = vec![
            actual_replica(0, "running", "img:1"),
            actual_replica(1, "running", "img:1"),
        ];
        let mut protected = HashSet::new();
        protected.insert("20260101-120000".to_string()); // desired()'s deployment_id
        let actions = diff_desired_vs_actual(&desired_all, &actual, "rise", &protected);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, ReconcileAction::Remove { .. })),
            "protected surplus replica must not be GC'd"
        );
    }

    /// Health map marking every given identity healthy.
    fn all_healthy(actual: &[ActualContainer]) -> HashMap<String, bool> {
        actual
            .iter()
            .filter_map(|a| a.identity().map(|id| (id, true)))
            .collect()
    }

    #[test]
    fn rolling_throttle_running_drifted_emits_exactly_one_recreate() {
        // INVARIANT (a): 3 running + drifted (image) + healthy replicas →
        // the diff produces 3 Recreates, but the rolling throttle admits exactly
        // ONE (the lowest replica index, r0).
        let desired_all = vec![desired_replica(0), desired_replica(1), desired_replica(2)];
        let actual = vec![
            actual_replica(0, "running", "img:OLD"),
            actual_replica(1, "running", "img:OLD"),
            actual_replica(2, "running", "img:OLD"),
        ];
        let raw = diff_desired_vs_actual(&desired_all, &actual, "rise", &no_protected());
        assert_eq!(
            raw.iter()
                .filter(|a| matches!(a, ReconcileAction::Recreate { .. }))
                .count(),
            3,
            "diff alone wants to recreate all three"
        );
        let healthy = all_healthy(&actual);
        let throttled = filter_rolling_actions(raw, &actual, &healthy);
        let recreates: Vec<&ReconcileAction> = throttled
            .iter()
            .filter(|a| matches!(a, ReconcileAction::Recreate { .. }))
            .collect();
        assert_eq!(recreates.len(), 1, "rolling admits exactly one per spec");
        match recreates[0] {
            ReconcileAction::Recreate { name, .. } => {
                assert!(
                    name.contains("_r0_"),
                    "lowest replica index rolls first: {name}"
                );
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn rolling_throttle_defers_all_when_a_sibling_is_unhealthy() {
        // INVARIANT (b): mid-rollout. r0 was already recreated (now matches
        // desired, so it is NOT a candidate) but is still STARTING (unhealthy);
        // r1 and r2 are still drifted (running, img:OLD → rollout candidates).
        // Because the non-candidate sibling r0 is unhealthy, ALL rollout recreates
        // for the spec are deferred this tick → ZERO Recreates. This is the gate
        // that makes the rollout wait for the previous replica to come back up.
        let desired_all = vec![desired_replica(0), desired_replica(1), desired_replica(2)];
        let actual = vec![
            actual_replica(0, "running", "img:1"), // already updated, matches
            actual_replica(1, "running", "img:OLD"), // drifted → candidate
            actual_replica(2, "running", "img:OLD"), // drifted → candidate
        ];
        let raw = diff_desired_vs_actual(&desired_all, &actual, "rise", &no_protected());
        // r0 (the just-recreated, non-candidate sibling) is still unhealthy.
        let mut healthy = all_healthy(&actual);
        healthy.insert(actual[0].identity().unwrap(), false);
        let throttled = filter_rolling_actions(raw, &actual, &healthy);
        assert_eq!(
            throttled
                .iter()
                .filter(|a| matches!(a, ReconcileAction::Recreate { .. }))
                .count(),
            0,
            "an unhealthy (still-starting) sibling defers ALL rollout recreates"
        );
    }

    #[test]
    fn rolling_throttle_uniform_drift_rolls_one_at_a_time() {
        // When EVERY replica is drifted (all candidates), there are no
        // non-candidate siblings to gate on, so the rollout begins: exactly one
        // (r0) is recreated this tick. On the next tick r0 matches desired and
        // becomes the sibling whose health gates the rest — yielding the
        // one-at-a-time roll across ticks.
        let desired_all = vec![desired_replica(0), desired_replica(1), desired_replica(2)];
        let actual = vec![
            actual_replica(0, "running", "img:OLD"),
            actual_replica(1, "running", "img:OLD"),
            actual_replica(2, "running", "img:OLD"),
        ];
        let raw = diff_desired_vs_actual(&desired_all, &actual, "rise", &no_protected());
        let healthy = all_healthy(&actual);
        let throttled = filter_rolling_actions(raw, &actual, &healthy);
        assert_eq!(
            throttled
                .iter()
                .filter(|a| matches!(a, ReconcileAction::Recreate { .. }))
                .count(),
            1,
            "uniform drift still rolls one replica at a time"
        );
    }

    #[test]
    fn rolling_throttle_recovery_recreate_passes_even_with_unhealthy_sibling() {
        // INVARIANT (c): a CRASHED (state != running) replica's Recreate is a
        // recovery — it passes UNTHROTTLED even though a sibling is unhealthy.
        // Here r0 has exited; r1 is running-drifted-unhealthy. Only r0's recovery
        // recreate is admitted (r1's rollout recreate is gated out).
        let desired_all = vec![desired_replica(0), desired_replica(1)];
        let actual = vec![
            actual_replica(0, "exited", "img:1"),    // crashed → recovery
            actual_replica(1, "running", "img:OLD"), // running-drifted → rollout
        ];
        let raw = diff_desired_vs_actual(&desired_all, &actual, "rise", &no_protected());
        // The crashed r0 is (realistically) unhealthy; it is r1's only sibling, so
        // r1's rollout recreate is GATED OUT. r0's recovery recreate still passes.
        let mut healthy = all_healthy(&actual);
        healthy.insert(actual[0].identity().unwrap(), false);
        let throttled = filter_rolling_actions(raw, &actual, &healthy);
        let recreates: Vec<&ReconcileAction> = throttled
            .iter()
            .filter(|a| matches!(a, ReconcileAction::Recreate { .. }))
            .collect();
        assert_eq!(recreates.len(), 1, "only the recovery recreate passes");
        match recreates[0] {
            ReconcileAction::Recreate { name, .. } => {
                assert!(
                    name.contains("_r0_"),
                    "recovery (crashed) replica passes: {name}"
                );
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn rolling_throttle_creates_and_removes_always_pass() {
        // INVARIANT (d): Create (scale-up) and Remove (scale-down/GC) actions are
        // never throttled — capacity changes apply immediately. Mix a Create
        // (r2 missing), a Remove (surplus r3), and a running-drifted r0 (rollout)
        // with r1 healthy. The Create + Remove pass; exactly one rollout recreate
        // passes too (r0, siblings healthy).
        let desired_all = vec![desired_replica(0), desired_replica(1), desired_replica(2)];
        let actual = vec![
            actual_replica(0, "running", "img:OLD"), // rollout recreate
            actual_replica(1, "running", "img:1"),   // matches → no action
            actual_replica(3, "running", "img:1"),   // surplus → Remove
        ];
        let raw = diff_desired_vs_actual(&desired_all, &actual, "rise", &no_protected());
        let healthy = all_healthy(&actual);
        let throttled = filter_rolling_actions(raw, &actual, &healthy);
        assert!(
            throttled.iter().any(
                |a| matches!(a, ReconcileAction::Create { name, .. } if name.contains("_r2_"))
            ),
            "scale-up Create passes"
        );
        assert!(
            throttled
                .iter()
                .any(|a| matches!(a, ReconcileAction::Remove { id, .. } if id == "cid-r3")),
            "scale-down Remove passes"
        );
        assert_eq!(
            throttled
                .iter()
                .filter(|a| matches!(a, ReconcileAction::Recreate { .. }))
                .count(),
            1,
            "exactly one rollout recreate (r0) admitted"
        );
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
    fn build_controller_metadata_aggregates_across_replicas() {
        // 3 replicas: 2 running+ready, 1 exited → desired=3, current=2, ready=2,
        // and three pod entries (one per replica container).
        let pods = vec![
            (
                "rise_myapp_default_d_app_r0_g1".to_string(),
                Some(inspected_running()),
            ),
            (
                "rise_myapp_default_d_app_r1_g1".to_string(),
                Some(inspected_running()),
            ),
            (
                "rise_myapp_default_d_app_r2_g1".to_string(),
                Some(inspected_exited(1)),
            ),
        ];
        // Not fully ready (one replica down).
        let meta = build_controller_metadata(&pods, &DeploymentStatus::Deploying, false);
        let ps = &meta["pod_status"];
        assert_eq!(ps["desired_replicas"], 3);
        assert_eq!(ps["current_replicas"], 2);
        assert_eq!(ps["ready_replicas"], 0); // is_ready=false → no container ready
        assert_eq!(ps["pods"].as_array().unwrap().len(), 3);
        assert_eq!(meta["health"]["healthy"], false);
    }
}
