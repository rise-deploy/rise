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
use super::labels::{self, SUFFIX_ENV_SECRET_HASH, SUFFIX_IMAGE, SUFFIX_MANAGED_BY};
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
}

/// Background reconciler. Holds everything needed to converge Docker state with
/// the DB on each tick.
pub struct DockerReconciler {
    docker: Docker,
    db_pool: PgPool,
    resource_builder: Arc<ResourceBuilder>,
    registry_provider: Arc<dyn RegistryProvider>,
    encryption_provider: Option<Arc<dyn EncryptionProvider>>,
    config: ReconcilerConfig,
}

impl DockerReconciler {
    pub fn new(
        docker: Docker,
        db_pool: PgPool,
        resource_builder: Arc<ResourceBuilder>,
        registry_provider: Arc<dyn RegistryProvider>,
        encryption_provider: Option<Arc<dyn EncryptionProvider>>,
        config: ReconcilerConfig,
    ) -> Self {
        Self {
            docker,
            db_pool,
            resource_builder,
            registry_provider,
            encryption_provider,
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

    /// Run a single reconcile pass over all projects. Errors are isolated per
    /// project so one bad project can't stall the rest.
    async fn tick(&self) -> Result<()> {
        let projects = db_projects::list(&self.db_pool, None).await?;
        for project in projects {
            if let Err(e) = self.reconcile_project(&project).await {
                error!(project = %project.name, "Failed to reconcile project: {:?}", e);
            }
        }
        Ok(())
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
        let mut desired: Vec<DesiredContainer> = Vec::new();
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
                        "Failed to compute desired containers: {:?}", e
                    );
                }
            }
        }

        // 3. Enumerate actual Rise containers for this project, diff, apply.
        let actual = self.list_actual_containers(project).await?;
        let actions = diff_desired_vs_actual(&desired, &actual, &self.config.container_prefix);
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
        // visible in `docker inspect`). Hash them for drift detection.
        let mut secret_hasher = Sha256::new();
        for (key, value) in &resolved.secret_env_vars {
            let plain = String::from_utf8_lossy(&value.0).to_string();
            secret_hasher.update((key.len() as u64).to_le_bytes());
            secret_hasher.update(key.as_bytes());
            secret_hasher.update((value.0.len() as u64).to_le_bytes());
            secret_hasher.update(&value.0);
            base_env.push((key.clone(), plain));
        }
        let env_secret_hash: String = secret_hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();

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

        // Build a desired container per spec. Clamp replicas to 1 for now.
        // TODO(replicas): run N containers sharing one Traefik service to
        // load-balance. Until then a single container serves each spec.
        let mut out = Vec::with_capacity(container_specs.len());
        for spec in &container_specs {
            let mut env = merge_container_env(&base_env, &system_env, spec, env_name.as_deref());
            // Pin PORT to this container's declared port.
            if let Some(port) = spec.port {
                upsert_env(&mut env, "PORT", &port.to_string());
            }

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

            out.push(DesiredContainer {
                project: project.name.clone(),
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
                env_secret_hash: env_secret_hash.clone(),
                routes: container_routes,
                health_path: Some(self.config.health_path.clone()),
            });
        }

        Ok(out)
    }

    // ── Actual containers + diff application ────────────────────────────

    async fn list_actual_containers(&self, project: &Project) -> Result<Vec<ActualContainer>> {
        use bollard::container::ListContainersOptions;
        let ns = &self.config.label_namespace;
        let mut filters: HashMap<String, Vec<String>> = HashMap::new();
        filters.insert(
            "label".to_string(),
            vec![
                format!("{}={}", labels::ns_key(ns, SUFFIX_MANAGED_BY), "rise"),
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
                    image_label: labels.get(&labels::ns_key(ns, SUFFIX_IMAGE)).cloned(),
                    env_secret_hash_label: labels
                        .get(&labels::ns_key(ns, SUFFIX_ENV_SECRET_HASH))
                        .cloned(),
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
        let builder_cfg = BuilderConfig {
            label_namespace: &self.config.label_namespace,
            controller_class: &self.config.controller_class,
            container_prefix: &self.config.container_prefix,
            traefik_network: &self.config.traefik_network,
            traefik_entrypoint: &self.config.traefik_entrypoint,
            traefik_certresolver: self.config.traefik_certresolver.as_deref(),
        };
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
                    self.remove_container(existing_id).await;
                    if let Some(d) = by_name.get(name.as_str()) {
                        if let Err(e) = self.create_container(d, &builder_cfg).await {
                            error!(
                                project = %project.name,
                                container = %name,
                                "Failed to recreate container: {:?}", e
                            );
                        }
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
        // Routable containers must all answer the HTTP probe for the deployment
        // to be considered healthy. Workers (no port) count as ready.
        let mut all_ready = true;
        let mut probed_any = false;
        for spec in &container_specs {
            let Some(port) = spec.port else {
                continue;
            };
            probed_any = true;
            let name = container_builder::container_name(
                &self.config.container_prefix,
                &project.name,
                &deployment.deployment_group,
                &deployment.deployment_id,
                &spec.name,
            );
            if !self.probe_container(&name, port).await {
                all_ready = false;
                break;
            }
        }
        // A deployment with no routable container (all workers) is ready once
        // its containers exist (they were created in the diff pass).
        let is_ready = all_ready && (probed_any || !container_specs.is_empty());

        // Snapshot controller_metadata in a K8s-pod-status-shaped blob so the
        // existing status APIs/UI render.
        let metadata = serde_json::json!({
            "pod_status": {
                "pods": container_specs.iter().map(|s| serde_json::json!({
                    "name": container_builder::container_name(
                        &self.config.container_prefix,
                        &project.name,
                        &deployment.deployment_group,
                        &deployment.deployment_id,
                        &s.name,
                    ),
                    "ready": is_ready,
                    "phase": if is_ready { "Running" } else { "Pending" },
                })).collect::<Vec<_>>(),
            },
            "health": {
                "last_check": Utc::now().to_rfc3339(),
                "healthy": is_ready && deployment.status != DeploymentStatus::Unhealthy,
            },
        });
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

    /// HTTP health probe over the shared Traefik network using the container's
    /// name as hostname (Docker DNS resolves it within the network).
    async fn probe_container(&self, container_name: &str, port: u16) -> bool {
        let url = format!(
            "http://{}:{}{}",
            container_name, port, self.config.health_path
        );
        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                warn!("Failed to build health probe client: {:?}", e);
                return false;
            }
        };
        match client.get(&url).send().await {
            Ok(resp) => resp.status().as_u16() < 500,
            Err(_) => false,
        }
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

// ── Pure diff ───────────────────────────────────────────────────────────

/// Observed Rise-managed container, reduced to the fields the diff needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActualContainer {
    pub id: String,
    pub name: String,
    pub image_label: Option<String>,
    pub env_secret_hash_label: Option<String>,
}

/// What to do with one container slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileAction {
    /// No matching actual container — create + start it.
    Create { name: String },
    /// Actual exists but drifted (image or env-secret-hash) — replace it.
    Recreate { name: String, existing_id: String },
    /// Actual exists with no desired match — garbage-collect it.
    Remove { id: String, name: String },
}

/// Classify each desired/actual container into create / recreate / remove.
///
/// Pure: no daemon access. `container_prefix` is the configured prefix used to
/// derive each desired container's deterministic name so it matches the names
/// stamped by `apply_actions` / `build_container`.
pub fn diff_desired_vs_actual(
    desired: &[DesiredContainer],
    actual: &[ActualContainer],
    container_prefix: &str,
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
                let env_drift =
                    a.env_secret_hash_label.as_deref() != Some(d.env_secret_hash.as_str());
                if image_drift || env_drift {
                    actions.push(ReconcileAction::Recreate {
                        name: name.clone(),
                        existing_id: a.id.clone(),
                    });
                }
            }
        }
    }

    for a in actual {
        if !matched_actual.contains(&a.name) {
            actions.push(ReconcileAction::Remove {
                id: a.id.clone(),
                name: a.name.clone(),
            });
        }
    }

    // Deterministic ordering for testability.
    actions.sort_by_key(action_key);
    actions
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::deployment::controller::docker::container_builder::DesiredRoute;

    fn desired(container: &str, image: &str, hash: &str) -> DesiredContainer {
        DesiredContainer {
            project: "myapp".to_string(),
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
            env_secret_hash: hash.to_string(),
            routes: vec![DesiredRoute {
                hosts: vec!["myapp.rise.dev".to_string()],
                path_prefix: None,
            }],
            health_path: Some("/".to_string()),
        }
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

    #[test]
    fn diff_creates_missing() {
        let d = desired("app", "img:1", "h1");
        let actions = diff_desired_vs_actual(std::slice::from_ref(&d), &[], "rise");
        assert_eq!(actions, vec![ReconcileAction::Create { name: name_of(&d) }]);
    }

    #[test]
    fn diff_no_action_when_matched() {
        let d = desired("app", "img:1", "h1");
        let actual = vec![ActualContainer {
            id: "cid".to_string(),
            name: name_of(&d),
            image_label: Some("img:1".to_string()),
            env_secret_hash_label: Some("h1".to_string()),
        }];
        let actions = diff_desired_vs_actual(&[d], &actual, "rise");
        assert!(actions.is_empty());
    }

    #[test]
    fn diff_recreates_on_image_drift() {
        let d = desired("app", "img:2", "h1");
        let actual = vec![ActualContainer {
            id: "cid".to_string(),
            name: name_of(&d),
            image_label: Some("img:1".to_string()),
            env_secret_hash_label: Some("h1".to_string()),
        }];
        let actions = diff_desired_vs_actual(std::slice::from_ref(&d), &actual, "rise");
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
        let actual = vec![ActualContainer {
            id: "cid".to_string(),
            name: name_of(&d),
            image_label: Some("img:1".to_string()),
            env_secret_hash_label: Some("h1".to_string()),
        }];
        let actions = diff_desired_vs_actual(std::slice::from_ref(&d), &actual, "rise");
        assert_eq!(
            actions,
            vec![ReconcileAction::Recreate {
                name: name_of(&d),
                existing_id: "cid".to_string()
            }]
        );
    }

    #[test]
    fn diff_removes_orphan() {
        let actual = vec![ActualContainer {
            id: "old".to_string(),
            name: "rise_myapp_default_oldid_app".to_string(),
            image_label: Some("img:1".to_string()),
            env_secret_hash_label: Some("h1".to_string()),
        }];
        let actions = diff_desired_vs_actual(&[], &actual, "rise");
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
            image_label: Some("img:1".to_string()),
            env_secret_hash_label: Some("h1".to_string()),
        }];
        let actions = diff_desired_vs_actual(std::slice::from_ref(&new), &actual, "rise");
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
}
