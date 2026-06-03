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

        // 3. Enumerate actual Rise containers for this project, diff, apply.
        let actual = self.list_actual_containers(project).await?;
        let actions = diff_desired_vs_actual(
            &desired,
            &actual,
            &self.config.container_prefix,
            &protected_deployment_ids,
        );
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

        // Build a desired container per spec. Clamp replicas to 1 for now.
        // TODO(replicas): run N containers sharing one Traefik service to
        // load-balance. Until then a single container serves each spec.
        let mut out = Vec::with_capacity(container_specs.len());
        for spec in &container_specs {
            // A Docker deployment runs exactly one container per spec; requested
            // replicas>1 are silently served by a single container today. Warn so
            // users aren't unknowingly under-provisioned.
            if spec.replicas.is_some_and(|r| r > 1) {
                warn!(
                    deployment_id = %deployment.deployment_id,
                    container = %spec.name,
                    requested = spec.replicas.unwrap_or(1),
                    "Docker backend does not support replicas>1; running a single container"
                );
            }
            let mut env = merge_container_env(&base_env, &system_env, spec, env_name.as_deref());
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

            let mut desired = DesiredContainer {
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
            };
            // Precompute the route-hash from the same render the builder uses so
            // the diff can detect routing transitions (active↔inactive) that
            // Docker can't apply to a running container's labels in place.
            desired.route_hash = container_builder::route_hash_for(&desired, &self.builder_cfg());
            out.push(desired);
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
                    deployment_id_label: labels
                        .get(&labels::ns_key(ns, labels::SUFFIX_DEPLOYMENT_ID))
                        .cloned(),
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
        let by_name: HashMap<String, &DesiredContainer> = desired
            .iter()
            .map(|d| {
                let name = container_builder::container_name(
                    &self.config.container_prefix,
                    &d.project,
                    &d.deployment_group,
                    &d.deployment_id,
                    &d.container,
                );
                (name, d)
            })
            .collect();

        for action in actions {
            match action {
                ReconcileAction::Create { name } => {
                    if let Some(d) = by_name.get(name.as_str()) {
                        if let Err(e) = self.create_container(d, &builder_cfg).await {
                            error!(
                                project = %project.name,
                                container = %name,
                                "Failed to create container: {:?}", e
                            );
                        }
                    }
                }
                ReconcileAction::Recreate { name, existing_id } => {
                    // The replacement reuses the old container's deterministic
                    // name, so we can't create-then-swap without a temp name.
                    // To keep the outage window minimal we pre-pull the image
                    // (the slowest, most failure-prone step) *before* removing
                    // the running container — a pull failure then leaves the old
                    // container in place rather than tearing it down first.
                    //
                    // TODO(create-then-swap): create the replacement under a
                    // temporary name, confirm it started, then atomically remove
                    // the old container and rename the new one. Requires the
                    // diff/health paths to tolerate the transient temp name.
                    if let Some(d) = by_name.get(name.as_str()) {
                        if let Err(e) = self.pull_image(&d.image).await {
                            error!(
                                project = %project.name,
                                container = %name,
                                "Failed to pull image for recreate; leaving existing container in place: {:?}", e
                            );
                            continue;
                        }
                        self.remove_container(existing_id).await;
                        if let Err(e) = self.create_container(d, &builder_cfg).await {
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
        // Inspect each container ONCE and reuse the result for both the health
        // probe (Issue 1) and the pod_status metadata builder (Issue 2). The
        // map is keyed by the deterministic container name.
        let mut inspected_by_name: HashMap<String, Option<InspectedContainer>> = HashMap::new();
        // Every container must be ready for the deployment to be healthy:
        //   - HTTP containers (with a port) must answer the probe — a real HTTP
        //     GET to the app (loopback published port when `publish_app_ports`
        //     is on, else the container IP), independent of Traefik routing, so
        //     a not-yet-active deployment can still become Healthy (then active);
        //   - workers (no port) must exist and be `running` on the daemon.
        // An empty spec set is never ready.
        let mut all_ready = !container_specs.is_empty();
        for spec in &container_specs {
            let name = container_builder::container_name(
                &self.config.container_prefix,
                &project.name,
                &deployment.deployment_group,
                &deployment.deployment_id,
                &spec.name,
            );
            let inspected = self.inspect_for_reconcile(&name, spec.port).await;
            let ready = match spec.port {
                Some(port) => self.probe_container(inspected.as_ref(), port).await,
                // Worker: no HTTP endpoint, so the daemon's run state is the
                // only liveness signal we have.
                None => inspected.as_ref().map(|i| i.running).unwrap_or(false),
            };
            inspected_by_name.insert(name, inspected);
            if !ready {
                all_ready = false;
                // Keep inspecting the remaining containers so the pod_status
                // snapshot stays complete; only the readiness verdict short-
                // circuits below via `all_ready`.
            }
        }
        let is_ready = all_ready;

        // Snapshot controller_metadata in a K8s-pod-status-shaped blob so the
        // existing status APIs/UI render unchanged. Built from the inspections
        // captured above (no second inspect).
        let metadata = self.build_controller_metadata(
            project,
            deployment,
            &container_specs,
            &inspected_by_name,
            is_ready,
        );
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
                    "Healthy deployment is now unhealthy"
                );
                db_deployments::mark_unhealthy(
                    &self.db_pool,
                    deployment.id,
                    "Container health probe failing".to_string(),
                )
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
    /// semantics (5s timeout). A missing inspection / address (container not yet
    /// attached or started) is treated as not-ready rather than an error.
    async fn probe_container(&self, inspected: Option<&InspectedContainer>, port: u16) -> bool {
        let Some(inspected) = inspected else {
            return false;
        };
        let url = if self.config.publish_app_ports {
            match inspected.published_host_port.as_deref() {
                Some(host_port) if !host_port.is_empty() => {
                    format!("http://127.0.0.1:{}{}", host_port, self.config.health_path)
                }
                _ => {
                    debug!("Container has no published loopback port yet; treating as not-ready");
                    return false;
                }
            }
        } else {
            match inspected.ip.as_deref() {
                Some(ip) if !ip.is_empty() => {
                    format!("http://{}:{}{}", ip, port, self.config.health_path)
                }
                _ => {
                    debug!(
                        network = %self.config.traefik_network,
                        "Container has no IP on the Traefik network yet; treating as not-ready"
                    );
                    return false;
                }
            }
        };
        match self.http_client.get(&url).send().await {
            Ok(resp) => resp.status().as_u16() < 500,
            Err(_) => false,
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

    /// Assemble the `controller_metadata` JSON for the deployment: a
    /// K8s-pod-status-shaped `pod_status` block (so the Pods tab renders
    /// unchanged — see `frontend/src/features/deployments.tsx`) plus the legacy
    /// sibling `health` block. The container readiness verdict (`is_ready`) is
    /// the same one driving the status transitions.
    fn build_controller_metadata(
        &self,
        project: &Project,
        deployment: &Deployment,
        container_specs: &[crate::server::deployment::models::ContainerSpec],
        inspected_by_name: &HashMap<String, Option<InspectedContainer>>,
        is_ready: bool,
    ) -> serde_json::Value {
        // Map each spec to its (name, inspection) so the pure builder can
        // produce the pod_status without touching `self`.
        let named: Vec<(String, Option<InspectedContainer>)> = container_specs
            .iter()
            .map(|s| {
                let name = container_builder::container_name(
                    &self.config.container_prefix,
                    &project.name,
                    &deployment.deployment_group,
                    &deployment.deployment_id,
                    &s.name,
                );
                let inspected = inspected_by_name.get(&name).cloned().flatten();
                (name, inspected)
            })
            .collect();

        let pod_status = build_pod_status(&named, is_ready);
        let desired = container_specs.len();
        let ready_replicas = pod_status
            .get("ready_replicas")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;

        serde_json::json!({
            "pod_status": pod_status,
            "health": {
                "last_check": Utc::now().to_rfc3339(),
                // Healthy only when every desired container is ready AND the
                // deployment isn't currently flagged Unhealthy.
                "healthy": ready_replicas == desired
                    && deployment.status != DeploymentStatus::Unhealthy,
            },
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
    /// The `deployment-id` bookkeeping label stamped on the live container.
    /// Used by the diff to protect a container from GC when its owning
    /// deployment's desired computation failed this tick. `None` for legacy
    /// containers created before this label was read (treated as unprotected).
    pub deployment_id_label: Option<String>,
    pub image_label: Option<String>,
    pub env_hash_label: Option<String>,
    /// The `route-hash` bookkeeping label stamped on the live container (sha256
    /// of its rendered Traefik label set; empty string when non-routable).
    /// `None` for legacy containers created before this label existed. Compared
    /// against the desired container's `route_hash` so a routing transition —
    /// e.g. a deployment becoming or ceasing to be active — forces a recreate
    /// that adds or removes the Traefik labels (Docker can't mutate them in
    /// place on a running container).
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileAction {
    /// No matching actual container — create + start it.
    Create { name: String },
    /// Actual exists but drifted (image or env-hash) — replace it.
    Recreate { name: String, existing_id: String },
    /// Actual exists with no desired match — garbage-collect it.
    Remove { id: String, name: String },
}

/// Classify each desired/actual container into create / recreate / remove.
///
/// Pure: no daemon access. `container_prefix` is the configured prefix used to
/// derive each desired container's deterministic name so it matches the names
/// stamped by `apply_actions` / `build_container`.
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
    // Desired containers keyed by their deterministic name.
    let mut desired_by_name: HashMap<String, &DesiredContainer> = HashMap::new();
    for d in desired {
        let name = container_builder::container_name(
            container_prefix,
            &d.project,
            &d.deployment_group,
            &d.deployment_id,
            &d.container,
        );
        desired_by_name.insert(name, d);
    }

    let actual_by_name: HashMap<&str, &ActualContainer> =
        actual.iter().map(|a| (a.name.as_str(), a)).collect();

    let mut actions = Vec::new();
    let mut matched_actual: HashSet<String> = HashSet::new();

    for (name, d) in &desired_by_name {
        match actual_by_name.get(name.as_str()) {
            None => actions.push(ReconcileAction::Create { name: name.clone() }),
            Some(a) => {
                matched_actual.insert(name.clone());
                let image_drift = a.image_label.as_deref() != Some(d.image.as_str());
                let env_drift = a.env_hash_label.as_deref() != Some(d.env_hash.as_str());
                // Routing drift: the live container's stamped `route-hash`
                // differs from the desired one. This is what makes routability a
                // first-class part of the diff — when a deployment becomes active
                // (gains Traefik labels) or stops being active (loses them) the
                // hash changes, forcing a recreate WITH/WITHOUT the labels.
                // Docker can't mutate a running container's labels in place, so
                // recreation is the only way to apply the change. Idempotent:
                // once the stamped hash equals the desired one nothing recreates.
                // A legacy container missing the label (`None`) is recreated once
                // to gain it, then converges.
                let route_drift = a.route_hash_label.as_deref() != Some(d.route_hash.as_str());
                // A container that matches on image + env but is in a
                // terminal-ish state (created-but-never-started, exited, or
                // crash-give-up `dead`) must be recreated so the deployment
                // recovers. Transient managed states (`restarting`, `paused`)
                // are left to Docker's restart policy so its backoff can settle.
                let not_running = a.needs_recreate();
                if image_drift || env_drift || route_drift || not_running {
                    actions.push(ReconcileAction::Recreate {
                        name: name.clone(),
                        existing_id: a.id.clone(),
                    });
                }
            }
        }
    }

    for a in actual {
        if matched_actual.contains(&a.name) {
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
        ReconcileAction::Create { name } => (0, name.clone()),
        ReconcileAction::Recreate { name, .. } => (1, name.clone()),
        ReconcileAction::Remove { name, .. } => (2, name.clone()),
    }
}

// ── Env merge helpers ─────────────────────────────────────────────────

/// Merge env for one container in final precedence:
/// base (plain + secret) → system env → per-container overrides. Later writes
/// win on key conflict.
fn merge_container_env(
    base_env: &[(String, String)],
    system_env: &[(String, String)],
    spec: &crate::server::deployment::models::ContainerSpec,
    env_name: Option<&str>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = base_env.to_vec();
    for (k, v) in system_env {
        upsert_env(&mut env, k, v);
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
        }
    }

    /// Empty protected-deployment-ids set for the common case where no
    /// deployment failed desired computation.
    fn no_protected() -> HashSet<String> {
        HashSet::new()
    }

    fn name_of(d: &DesiredContainer) -> String {
        container_builder::container_name(
            "rise",
            &d.project,
            &d.deployment_group,
            &d.deployment_id,
            &d.container,
        )
    }

    /// A live container belonging to the `desired()` helper's deployment.
    fn actual_for(d: &DesiredContainer, image: &str, env_hash: &str) -> ActualContainer {
        ActualContainer {
            id: "cid".to_string(),
            name: name_of(d),
            deployment_id_label: Some(d.deployment_id.clone()),
            image_label: Some(image.to_string()),
            env_hash_label: Some(env_hash.to_string()),
            route_hash_label: Some("rh-active".to_string()),
            state: Some("running".to_string()),
        }
    }

    #[test]
    fn diff_creates_missing() {
        let d = desired("app", "img:1", "h1");
        let actions =
            diff_desired_vs_actual(std::slice::from_ref(&d), &[], "rise", &no_protected());
        assert_eq!(actions, vec![ReconcileAction::Create { name: name_of(&d) }]);
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
                name: name_of(&d),
                existing_id: "cid".to_string()
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
                name: name_of(&d),
                existing_id: "cid".to_string()
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
                    name: name_of(&d),
                    existing_id: "cid".to_string()
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
            name: "rise_myapp_default_oldid_app".to_string(),
            deployment_id_label: Some("oldid".to_string()),
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
                name: "rise_myapp_default_oldid_app".to_string()
            }]
        );
    }

    #[test]
    fn diff_supersession_creates_new_removes_old() {
        // New deployment desired; old deployment's container still present.
        let new = desired("app", "img:2", "h2");
        let actual = vec![ActualContainer {
            id: "old".to_string(),
            name: "rise_myapp_default_oldid_app".to_string(),
            deployment_id_label: Some("oldid".to_string()),
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
                    name: name_of(&new)
                },
                ReconcileAction::Remove {
                    id: "old".to_string(),
                    name: "rise_myapp_default_oldid_app".to_string()
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
            name: "rise_myapp_default_protectedid_app".to_string(),
            deployment_id_label: Some("protectedid".to_string()),
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
                name: "rise_myapp_default_protectedid_app".to_string(),
                deployment_id_label: Some("protectedid".to_string()),
                image_label: Some("img:1".to_string()),
                env_hash_label: Some("h1".to_string()),
                route_hash_label: Some("rh-active".to_string()),
                state: Some("running".to_string()),
            },
            ActualContainer {
                id: "orphan".to_string(),
                name: "rise_myapp_default_orphanid_app".to_string(),
                deployment_id_label: Some("orphanid".to_string()),
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
                name: "rise_myapp_default_orphanid_app".to_string()
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
                name: name_of(&d),
                existing_id: "cid".to_string()
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
            id: "cid".to_string(),
            name: name_of(&d),
            deployment_id_label: Some(d.deployment_id.clone()),
            image_label: Some("img:1".to_string()),
            env_hash_label: Some("h1".to_string()),
            route_hash_label: Some("rh-active".to_string()),
            state: Some("running".to_string()),
        }];
        let actions =
            diff_desired_vs_actual(std::slice::from_ref(&d), &actual, "rise", &no_protected());
        assert_eq!(
            actions,
            vec![ReconcileAction::Recreate {
                name: name_of(&d),
                existing_id: "cid".to_string()
            }]
        );
    }

    #[test]
    fn diff_no_action_when_route_image_env_all_match() {
        // Idempotence: once image + env + route-hash all match and the container
        // is running, the diff emits NO action — no recreate oscillation.
        let d = desired("app", "img:1", "h1"); // route_hash = "rh-active"
        let actual = vec![ActualContainer {
            id: "cid".to_string(),
            name: name_of(&d),
            deployment_id_label: Some(d.deployment_id.clone()),
            image_label: Some("img:1".to_string()),
            env_hash_label: Some("h1".to_string()),
            route_hash_label: Some("rh-active".to_string()),
            state: Some("running".to_string()),
        }];
        let actions =
            diff_desired_vs_actual(std::slice::from_ref(&d), &actual, "rise", &no_protected());
        assert!(actions.is_empty(), "matching container must not recreate");
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
        let merged = merge_container_env(&base, &system, &spec, None);
        let foo = merged.iter().find(|(k, _)| k == "FOO").unwrap();
        assert_eq!(foo.1, "override");
        assert!(merged
            .iter()
            .any(|(k, v)| k == "RISE_APP_URL" && v == "url"));
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
        let merged = merge_container_env(&[], &[], &spec, Some("staging"));
        assert!(!merged.iter().any(|(k, _)| k == "ONLY_PROD"));
        let merged_prod = merge_container_env(&[], &[], &spec, Some("production"));
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
}
