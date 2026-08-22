//! The in-process ECS reconcile loop.
//!
//! Structurally this mirrors `rise_backend_docker::reconciler::DockerReconciler`
//! — the same leader-elected loop, the same per-project error isolation, the same
//! three-read rhythm around status transitions, and the same fail-safe leadership
//! re-verification. Two things differ, both because ECS is a remote, throttled,
//! polled control plane rather than a local socket:
//!
//! 1. **Actions are declarative.** Drift is answered with `UpdateService`, and
//!    ECS performs the rolling replacement. Nothing is destroyed to change
//!    anything, so there is no remove-then-create window — which matters far more
//!    here than on Docker, where a container starts in a second rather than tens
//!    of them.
//! 2. **API calls are budgeted.** `RegisterTaskDefinition` sustains 1 request per
//!    second and service modifications 5; a naive port of the Docker loop (which
//!    lists twice per tick and inspects every container) would throttle a
//!    moderately sized install. So each tick makes one listing pass and shares it
//!    between drift detection, readiness and `pod_status`, and a task definition
//!    is registered only when its content hash actually moves.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, Result};
use rise_backend_core::models::{Deployment, DeploymentStatus, Project, TerminationReason};
use rise_backend_core::{
    build_controller_metadata, effective_health_path, hash_env, merge_container_env,
    pin_system_env, resolve_deployment_env_vars, resolve_runtime_containers, rise_system_env_vars,
    should_have_infrastructure, spec_key, DeploymentStore, DeploymentUrlBuilder, DesiredContainer,
    DesiredRoute, EncryptionProvider, InspectedContainer, RegistryProvider,
};
use rise_backend_traefik::{replica_ready, ReadyVerdict, TraefikApiClient};
use rise_deployment_spec::request_spec::{ContainerSpec, RouteSpec};
use rise_runtime_sync::{
    with_leader_election, LeaderElection, LeaderStatus, LeaseError, LEASE_DURATION,
};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::aws_error_detail;
use crate::service::{self, ActualService, DesiredService, ServiceAction};
use crate::ssm;
use crate::tags::ServiceTags;
use crate::task_definition::{self, SecretRef, TaskDefinitionConfig, TaskDefinitionSpec};

/// Leadership must remain valid for at least this long before we start a
/// project's destructive work. Mirrors the Docker reconciler.
const PER_PROJECT_MIN_VALIDITY: std::time::Duration = std::time::Duration::from_secs(5);

/// Upper bound on `desiredCount` for one service. ECS itself allows 5,000, but a
/// runaway `replicas` value would consume the account's Fargate vCPU quota (6 by
/// default on a fresh account) and starve every other workload. The configured
/// `deployment_constraints.max_replicas` is the real limit; this is a backstop.
const MAX_REPLICAS: u32 = 100;

/// `DescribeServices` accepts at most 10 services per call.
const DESCRIBE_SERVICES_BATCH: usize = 10;

/// Tag suffix carrying the content hash of the task definition a service runs.
/// Stamped alongside the bookkeeping tags so drift detection is a tag comparison
/// rather than a `DescribeTaskDefinition` per service per tick.
const TASK_DEFINITION_HASH_SUFFIX: &str = "task-definition-hash";

/// Owned controller configuration.
#[derive(Clone)]
pub struct ReconcilerConfig {
    pub cluster: String,
    pub region: String,
    pub subnets: Vec<String>,
    pub security_groups: Vec<String>,
    pub assign_public_ip: bool,
    pub execution_role_arn: Option<String>,
    pub task_role_arn: Option<String>,
    pub log_group: Option<String>,
    pub resource_prefix: String,
    pub ssm_parameter_prefix: String,
    pub ssm_kms_key_id: Option<String>,
    pub cpu_architecture: String,
    pub controller_class: String,
    pub label_namespace: String,
    pub reconcile_interval_secs: u64,
    pub health_path: String,
    pub public_url: String,
    pub auth_backend_url: String,
    pub access_classes: HashMap<String, rise_backend_core::AccessRequirement>,
    pub traefik_entrypoint: String,
    pub traefik_certresolver: Option<String>,
    pub traefik_api_url: Option<String>,
}

/// Background reconciler converging the ECS cluster with the database.
pub struct EcsReconciler {
    ecs: aws_sdk_ecs::Client,
    ssm: aws_sdk_ssm::Client,
    store: Arc<dyn DeploymentStore>,
    /// Retained solely for leader election; all deployment state goes through
    /// [`Self::store`].
    db_pool: PgPool,
    url_builder: Arc<DeploymentUrlBuilder>,
    #[allow(dead_code)]
    registry_provider: Arc<dyn RegistryProvider>,
    encryption_provider: Option<Arc<dyn EncryptionProvider>>,
    resource_store: Arc<dyn rise_resource_api::ResourceStore>,
    /// Traefik API client for the authoritative readiness signal. `None` when no
    /// `traefik_api_url` is configured — health-checked deployments then have no
    /// readiness signal and never become Healthy (there is deliberately no
    /// fallback; see the Docker backend's rationale).
    traefik_api: Option<TraefikApiClient>,
    config: ReconcilerConfig,
}

impl EcsReconciler {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        ecs: aws_sdk_ecs::Client,
        ssm: aws_sdk_ssm::Client,
        store: Arc<dyn DeploymentStore>,
        db_pool: PgPool,
        url_builder: Arc<DeploymentUrlBuilder>,
        registry_provider: Arc<dyn RegistryProvider>,
        encryption_provider: Option<Arc<dyn EncryptionProvider>>,
        resource_store: Arc<dyn rise_resource_api::ResourceStore>,
        config: ReconcilerConfig,
    ) -> Self {
        // The ECS provider registers Traefik services as `{name}@ecs`; querying
        // `@docker` would 404 and silently yield no readiness signal at all.
        let traefik_api = config
            .traefik_api_url
            .as_deref()
            .and_then(|url| TraefikApiClient::new(url, "ecs"));
        Self {
            ecs,
            ssm,
            store,
            db_pool,
            url_builder,
            registry_provider,
            encryption_provider,
            resource_store,
            traefik_api,
            config,
        }
    }

    const LEASE_NAME: &'static str = "ecs-deployment-reconciler";

    /// Run the reconcile loop under a leader election until `shutdown` is
    /// cancelled, releasing the lease on exit so a peer can take over promptly.
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

    /// Spawn [`run`](Self::run), returning the handle so the caller can await a
    /// graceful lease release on shutdown.
    pub fn spawn(self, shutdown: CancellationToken) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            if let Err(e) = self.run(shutdown).await {
                error!("ECS reconciler exited with error: {:?}", e);
            }
        })
    }

    async fn reconcile_loop(&self, election: LeaderElection, shutdown: CancellationToken) {
        let interval = std::time::Duration::from_secs(self.config.reconcile_interval_secs.max(1));
        info!(
            interval_secs = interval.as_secs(),
            cluster = %self.config.cluster,
            controller_class = %self.config.controller_class,
            "ECS reconciler started"
        );
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tokio::time::sleep(interval) => {}
            }
            if !election.is_leader() {
                debug!("Not the ECS reconciler leader; skipping tick");
                continue;
            }
            if let Err(e) = self.tick(&election).await {
                error!("ECS reconcile tick failed: {:?}", e);
            }
        }
    }

    /// Re-verify leadership before destructive work.
    ///
    /// **Fails safe on a DB error** rather than falling back to the cached
    /// `is_leader()` flag. The reasoning transfers verbatim from the Docker
    /// backend and applies with equal force here: the ECS API stays reachable
    /// during a DB-only partition, so a partitioned former leader trusting a
    /// stale flag would mutate the same cluster a DB-reachable peer is
    /// legitimately driving. ECS operations are reversible and retried on the
    /// next tick, so skipping is the safe choice.
    async fn confirm_leadership(&self, election: &LeaderElection) -> bool {
        match election.ensure_leader_for(PER_PROJECT_MIN_VALIDITY).await {
            Ok(LeaderStatus::Leader) => true,
            Ok(LeaderStatus::NotLeader) => {
                warn!("Lost ECS reconciler leader lease mid-tick; aborting remaining projects");
                false
            }
            Err(LeaseError::Db(e)) => {
                warn!(
                    error = ?e,
                    "Leader verification DB error; failing safe and skipping the tick \
                     (the ECS API stays reachable during a DB partition, so trusting a \
                     stale cached leader flag could split-brain the cluster)"
                );
                false
            }
            Err(e @ LeaseError::InvalidMinValidity { .. }) => {
                error!(error = ?e, "ensure_leader_for misconfigured; aborting tick");
                false
            }
        }
    }

    async fn tick(&self, election: &LeaderElection) -> Result<()> {
        let projects = self.store.list_projects(None).await?;
        let mut org_class_cache: HashMap<Uuid, Option<String>> = HashMap::new();

        for project in projects {
            match self.owns_project(&project, &mut org_class_cache).await {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    warn!(project = %project.name, "Failed to resolve ownership: {:?}", e);
                    continue;
                }
            }
            // The lease is global, so losing it aborts the whole tick rather
            // than just this project.
            if !self.confirm_leadership(election).await {
                return Ok(());
            }
            if let Err(e) = self.reconcile_project(&project, election).await {
                error!(project = %project.name, "Failed to reconcile project: {:?}", e);
            }
        }
        Ok(())
    }

    /// Whether this controller owns `project` — i.e. the project's Organization's
    /// `deploymentControllerClass` matches ours.
    async fn owns_project(
        &self,
        project: &Project,
        org_class_cache: &mut HashMap<Uuid, Option<String>>,
    ) -> Result<bool> {
        let Some(org_uid) = self.store.organization_uid_for_project(project.id).await? else {
            warn!(
                project = %project.name,
                "Project has no organization linkage; skipping in ECS reconciler"
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

    async fn resolve_org_controller_class(
        &self,
        org_uid: Uuid,
        cache: &mut HashMap<Uuid, Option<String>>,
    ) -> Result<Option<String>> {
        if let Some(cached) = cache.get(&org_uid) {
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
        cache.insert(org_uid, class.clone());
        Ok(class)
    }

    async fn reconcile_project(&self, project: &Project, election: &LeaderElection) -> Result<()> {
        // 1. Status transitions, isolated per deployment.
        let non_terminal = self
            .store
            .list_non_terminal_deployments_for_project(project.id)
            .await?;
        for deployment in &non_terminal {
            if let Err(e) = self.perform_status_transition(project, deployment).await {
                warn!(
                    deployment = %deployment.deployment_id,
                    "Status transition failed: {:?}", e
                );
            }
        }
        // Re-read: `Pushed -> Deploying` must be visible to the desired set.
        let non_terminal = self
            .store
            .list_non_terminal_deployments_for_project(project.id)
            .await?;

        // 2. Desired state. A deployment whose desired state cannot be computed
        // is PROTECTED rather than dropped: its services would otherwise look
        // orphaned and be deleted, taking a healthy app offline over a transient
        // decryption or read error.
        let mut desired: Vec<(DesiredService, TaskDefinitionSpec, DesiredContainer)> = Vec::new();
        let mut protected_deployment_ids: HashSet<String> = HashSet::new();
        for deployment in &non_terminal {
            if !should_have_infrastructure(deployment) {
                continue;
            }
            match self
                .compute_desired_for_deployment(project, deployment)
                .await
            {
                Ok(mut entries) => desired.append(&mut entries),
                Err(e) => {
                    warn!(
                        deployment = %deployment.deployment_id,
                        "Failed to compute desired state; protecting its services from GC: {:?}", e
                    );
                    protected_deployment_ids.insert(deployment.deployment_id.clone());
                }
            }
        }

        // 3. Observe, diff, apply.
        let actual = self.list_actual_services(project).await?;
        let desired_services: Vec<DesiredService> =
            desired.iter().map(|(d, _, _)| d.clone()).collect();
        let actions = service::diff_services(&desired_services, &actual, &protected_deployment_ids);

        if !actions.is_empty() && !self.confirm_leadership(election).await {
            return Ok(());
        }
        self.apply_actions(project, &desired, &actions, election)
            .await;

        // 4. Health and status, from a fresh read of the deployment rows.
        let non_terminal = self
            .store
            .list_non_terminal_deployments_for_project(project.id)
            .await?;
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
                    deployment = %deployment.deployment_id,
                    "Health reconciliation failed: {:?}", e
                );
            }
        }
        Ok(())
    }

    /// Drive the time- and state-based transitions that do not depend on the
    /// runtime: timeouts, expiry, cancellation and termination completion.
    /// Identical in shape and thresholds to the Docker backend so a deployment's
    /// lifecycle reads the same on every backend.
    async fn perform_status_transition(
        &self,
        project: &Project,
        deployment: &Deployment,
    ) -> Result<()> {
        use rise_backend_core::{DEPLOYING_TIMEOUT_MINUTES, PRE_PUSHED_TIMEOUT_MINUTES};
        let now = chrono::Utc::now();

        match deployment.status {
            DeploymentStatus::Pending | DeploymentStatus::Building | DeploymentStatus::Pushing => {
                if now - deployment.created_at
                    > chrono::Duration::minutes(PRE_PUSHED_TIMEOUT_MINUTES)
                {
                    self.store
                        .mark_deployment_failed(
                            deployment.id,
                            "Deployment timed out before the image was pushed — the CLI was \
                             most likely interrupted during build or push.",
                        )
                        .await?;
                    self.store
                        .update_project_calculated_status(project.id)
                        .await?;
                }
            }
            DeploymentStatus::Cancelling => {
                self.store.mark_deployment_cancelled(deployment.id).await?;
                self.store
                    .update_project_calculated_status(project.id)
                    .await?;
            }
            DeploymentStatus::Terminating => {
                self.complete_termination(project, deployment).await?;
            }
            DeploymentStatus::Pushed => {
                self.store
                    .update_deployment_status(deployment.id, DeploymentStatus::Deploying)
                    .await?;
                self.store
                    .update_project_calculated_status(project.id)
                    .await?;
            }
            DeploymentStatus::Deploying => {
                if let Some(started) = deployment.deploying_started_at {
                    if now - started > chrono::Duration::minutes(DEPLOYING_TIMEOUT_MINUTES) {
                        self.store
                            .mark_deployment_failed(
                                deployment.id,
                                &format!(
                                    "Deployment timed out after {} seconds in Deploying state",
                                    DEPLOYING_TIMEOUT_MINUTES * 60
                                ),
                            )
                            .await?;
                        self.store
                            .update_project_calculated_status(project.id)
                            .await?;
                    }
                }
            }
            _ => {}
        }

        if let Some(expires_at) = deployment.expires_at {
            if expires_at <= now
                && !matches!(
                    deployment.status,
                    DeploymentStatus::Terminating | DeploymentStatus::Cancelling
                )
            {
                self.store
                    .mark_deployment_terminating(deployment.id, TerminationReason::Expired)
                    .await?;
                self.store
                    .update_project_calculated_status(project.id)
                    .await?;
            }
        }
        Ok(())
    }

    async fn complete_termination(&self, project: &Project, deployment: &Deployment) -> Result<()> {
        match deployment.termination_reason {
            Some(TerminationReason::Superseded) => {
                self.store.mark_deployment_superseded(deployment.id).await?;
            }
            Some(TerminationReason::UserStopped) => {
                self.store.mark_deployment_stopped(deployment.id).await?;
            }
            Some(TerminationReason::Expired) => {
                self.store.mark_deployment_expired(deployment.id).await?;
            }
            Some(TerminationReason::Failed) => {
                let message = deployment
                    .error_message
                    .clone()
                    .unwrap_or_else(|| "Deployment failed".to_string());
                self.store
                    .mark_deployment_failed(deployment.id, &message)
                    .await?;
            }
            Some(TerminationReason::Cancelled) => {
                self.store.mark_deployment_cancelled(deployment.id).await?;
            }
            None => {
                self.store.mark_deployment_stopped(deployment.id).await?;
            }
        }
        self.store
            .update_project_calculated_status(project.id)
            .await?;
        Ok(())
    }

    // ── desired state ─────────────────────────────────────────────────────

    /// Compute the services (and their task definitions) one deployment wants.
    ///
    /// Mirrors the Docker backend's desired computation with two deliberate
    /// differences: secrets stay *out* of the plain environment (they go to SSM),
    /// and there is no per-replica fan-out — the replica count becomes the
    /// service's `desiredCount`.
    async fn compute_desired_for_deployment(
        &self,
        project: &Project,
        deployment: &Deployment,
    ) -> Result<Vec<(DesiredService, TaskDefinitionSpec, DesiredContainer)>> {
        let (container_specs, route_specs) = resolve_runtime_containers(deployment)?;

        // Fail closed on the features v1 does not implement, rather than
        // deploying something that looks fine and is quietly broken.
        self.reject_unsupported(deployment, &container_specs)?;

        let environment = if let Some(env_id) = deployment.environment_id {
            self.store.find_environment(env_id).await?
        } else {
            None
        };
        let env_name = environment.as_ref().map(|e| e.name.clone());
        let all_environments = self.store.list_environments_for_project(project.id).await?;
        let custom_domains = self.store.list_project_custom_domains(project.id).await?;

        let urls = self.url_builder.compute_deployment_urls(
            project,
            deployment,
            environment.as_ref(),
            &all_environments,
            &custom_domains,
        );
        let primary_hosts = self.url_builder.primary_ingress_hosts(
            project,
            &deployment.deployment_group,
            environment.as_ref().filter(|e| {
                e.primary_deployment_group.as_deref() == Some(&deployment.deployment_group)
            }),
            &custom_domains,
            true,
            &all_environments,
        );
        let primary_hosts: Vec<String> = primary_hosts.into_iter().map(|h| h.host).collect();

        // Secrets are kept SEPARATE from plain env — the whole point of D7. They
        // still ride in the merged env for hashing (so editing a secret rolls the
        // deployment) but are stripped from the task definition later.
        let raw_env_vars = self.store.list_deployment_env_vars(deployment.id).await?;
        let resolved =
            resolve_deployment_env_vars(raw_env_vars, self.encryption_provider.as_deref()).await?;
        let secret_plaintext: Vec<(String, Vec<u8>)> = resolved
            .secret_env_vars
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let mut base_env = resolved.plain_env_vars.clone();
        for (key, value) in &secret_plaintext {
            base_env.push((key.clone(), String::from_utf8_lossy(value).to_string()));
        }
        let system_env = rise_system_env_vars(
            &self.config.public_url,
            &deployment.deployment_group,
            &urls,
            env_name.as_deref(),
        );

        let source_deployment_id = match deployment.rolled_back_from_deployment_id {
            Some(source_id) => self
                .store
                .find_deployment(source_id)
                .await?
                .map(|d| d.deployment_id),
            None => None,
        };
        let base_image =
            self.url_builder
                .resolve_image(project, deployment, source_deployment_id.as_deref());

        let mut out = Vec::new();
        for spec in &container_specs {
            let entry = self
                .desired_for_spec(
                    project,
                    deployment,
                    spec,
                    &route_specs,
                    &primary_hosts,
                    &base_env,
                    &system_env,
                    &secret_plaintext,
                    env_name.as_deref(),
                    &base_image,
                )
                .await?;
            out.push(entry);
        }
        Ok(out)
    }

    /// Reject deployments that request a capability this backend does not yet
    /// implement.
    ///
    /// Explicit rejection, never silent degradation: a multi-container app whose
    /// siblings cannot resolve each other, or a workload whose identity token
    /// files never appear, would deploy "successfully" and then fail in ways that
    /// look like application bugs.
    fn reject_unsupported(
        &self,
        deployment: &Deployment,
        container_specs: &[ContainerSpec],
    ) -> Result<()> {
        if container_specs.len() > 1 {
            anyhow::bail!(
                "the ECS backend does not yet support multi-container deployments ({} \
                 containers requested). Cross-container discovery needs AWS Cloud Map \
                 service registration, which is not implemented — RISE_CONTAINER_HOST__* \
                 would be absent and the containers could not reach each other. Deploy the \
                 containers as separate Rise projects, or use the Kubernetes or Docker \
                 backend.",
                container_specs.len()
            );
        }
        let requests_identity = deployment
            .identity_audiences
            .as_object()
            .is_some_and(|m| !m.is_empty());
        if requests_identity {
            anyhow::bail!(
                "the ECS backend does not yet deliver workload-identity tokens, but this \
                 deployment declares `[identity].audiences`. The token files would never \
                 appear at /var/run/secrets/rise/identity/, so the workload would fail at \
                 runtime. Remove the `[identity]` section, or use the Kubernetes or Docker \
                 backend."
            );
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn desired_for_spec(
        &self,
        project: &Project,
        deployment: &Deployment,
        spec: &ContainerSpec,
        route_specs: &[RouteSpec],
        primary_hosts: &[String],
        base_env: &[(String, String)],
        system_env: &[(String, String)],
        secret_plaintext: &[(String, Vec<u8>)],
        env_name: Option<&str>,
        base_image: &str,
    ) -> Result<(DesiredService, TaskDefinitionSpec, DesiredContainer)> {
        let replica_count = clamp_replicas(spec.replicas);
        if let Some(requested) = spec.replicas {
            if requested > MAX_REPLICAS {
                warn!(
                    project = %project.name,
                    container = %spec.name,
                    requested,
                    max = MAX_REPLICAS,
                    "Requested replicas exceeds the ECS controller cap; clamping"
                );
            }
        }

        let mut env = merge_container_env(base_env, system_env, &[], spec, env_name);
        pin_system_env(&mut env, &spec.name, spec.port);
        // The hash covers the FULL env including secret plaintext. If it covered
        // only what reaches the task definition, editing a secret would leave the
        // hash unchanged and the deployment would never roll to pick up the new
        // value sitting in SSM.
        let env_hash = hash_env(&env);

        let container_routes: Vec<DesiredRoute> = route_specs
            .iter()
            .filter(|r| r.container == spec.name)
            .map(|r| DesiredRoute {
                hosts: primary_hosts.to_vec(),
                path_prefix: Some(r.path.clone()),
                access: r.access.clone(),
            })
            .collect();

        let routable = !rise_backend_traefik::routes_withheld(
            &project.access_class,
            &self.config.access_classes,
            &self.config.auth_backend_url,
            route_specs
                .iter()
                .filter(|r| r.container == spec.name)
                .map(|r| r.access.as_ref()),
        ) && !primary_hosts.is_empty();

        let mut desired_container = DesiredContainer {
            project: project.name.clone(),
            access_class: project.access_class.clone(),
            deployment_group: deployment.deployment_group.clone(),
            deployment_id: deployment.deployment_id.clone(),
            deployment_uuid: deployment.id.to_string(),
            container: spec.name.clone(),
            environment: env_name.map(str::to_string),
            image: spec.image.clone().unwrap_or_else(|| base_image.to_string()),
            port: spec.port,
            cpu: spec.cpu.clone().unwrap_or_else(|| deployment.cpu.clone()),
            memory: spec
                .memory
                .clone()
                .unwrap_or_else(|| deployment.memory.clone()),
            env,
            env_hash: env_hash.clone(),
            routes: container_routes,
            routable,
            route_hash: String::new(),
            generation: 1,
            replica: 0,
            health_path: spec
                .port
                .and_then(|_| effective_health_path(spec, &self.config.health_path)),
            health_check_interval_secs: spec.health_check.as_ref().and_then(|hc| hc.period_seconds),
            health_check_timeout_secs: spec.health_check.as_ref().and_then(|hc| hc.timeout_seconds),
        };
        // ECS has no `publish_app_ports` analogue, so the route hash folds in
        // `false` for that dimension.
        desired_container.route_hash = rise_backend_traefik::route_hash_for(
            &desired_container,
            &self.traefik_render_config(),
            false,
        );

        // Secret env vars become SSM parameter references.
        let secrets: Vec<SecretRef> = secret_plaintext
            .iter()
            .map(|(key, _)| SecretRef {
                name: key.clone(),
                value_from: ssm::parameter_name(
                    &self.config.ssm_parameter_prefix,
                    &project.name,
                    &deployment.deployment_group,
                    &deployment.deployment_id,
                    key,
                ),
            })
            .collect();
        for (key, value) in secret_plaintext {
            ssm::validate(key, value)?;
        }

        let task_def = task_definition::build(
            &desired_container,
            &secrets,
            &TaskDefinitionConfig {
                resource_prefix: &self.config.resource_prefix,
                cpu_architecture: &self.config.cpu_architecture,
                execution_role_arn: self.config.execution_role_arn.as_deref(),
                task_role_arn: self.config.task_role_arn.as_deref(),
                log_group: self.config.log_group.as_deref(),
                region: &self.config.region,
                traefik: self.traefik_render_config(),
            },
        )?;

        if task_def.size.rounded_up {
            info!(
                project = %project.name,
                container = %spec.name,
                requested_cpu = %desired_container.cpu,
                requested_memory = %desired_container.memory,
                resolved_cpu = %task_def.cpu,
                resolved_memory_mib = %task_def.memory,
                "Rounded the requested resources up to the nearest valid Fargate task size"
            );
        }

        let desired_service = DesiredService {
            name: service::service_name(
                &self.config.resource_prefix,
                &project.name,
                &deployment.deployment_group,
                &deployment.deployment_id,
                &spec.name,
            ),
            key: spec_key(
                &project.name,
                &deployment.deployment_group,
                &deployment.deployment_id,
                &spec.name,
            ),
            family: task_def.family.clone(),
            task_definition_hash: task_def.content_hash(),
            desired_count: replica_count as i32,
            tags: ServiceTags {
                project: project.name.clone(),
                deployment_group: deployment.deployment_group.clone(),
                deployment_id: deployment.deployment_id.clone(),
                deployment_uuid: deployment.id.to_string(),
                container: spec.name.clone(),
                environment: env_name.map(str::to_string),
                env_hash,
                image: desired_container.image.clone(),
                route_hash: desired_container.route_hash.clone(),
            },
        };
        Ok((desired_service, task_def, desired_container))
    }

    // ── observing the cluster ─────────────────────────────────────────────

    /// List this project's Rise-managed services.
    ///
    /// Discovery is by **tag**, never by parsing names: a name is lossy (long
    /// project names are truncated with a hash suffix) while tags are exact, and
    /// mistaking another controller's service for ours would mean deleting it.
    ///
    /// One `ListServices` plus batched `DescribeServices` (10 per call, the API
    /// maximum) per project per tick — the result is shared with readiness and
    /// `pod_status` rather than re-fetched, because service reads sustain only
    /// 20 requests/second.
    async fn list_actual_services(&self, project: &Project) -> Result<Vec<ActualService>> {
        let mut arns: Vec<String> = Vec::new();
        let mut next_token: Option<String> = None;
        loop {
            let mut req = self.ecs.list_services().cluster(&self.config.cluster);
            if let Some(token) = &next_token {
                req = req.next_token(token);
            }
            let out = req
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("ListServices failed: {}", aws_error_detail(&e)))?;
            arns.extend(out.service_arns().iter().cloned());
            next_token = out.next_token().map(str::to_string);
            if next_token.is_none() {
                break;
            }
        }

        let mut services = Vec::new();
        for chunk in arns.chunks(DESCRIBE_SERVICES_BATCH) {
            let mut req = self
                .ecs
                .describe_services()
                .cluster(&self.config.cluster)
                .include(aws_sdk_ecs::types::ServiceField::Tags);
            for arn in chunk {
                req = req.services(arn);
            }
            let out = req.send().await.map_err(|e| {
                anyhow::anyhow!("DescribeServices failed: {}", aws_error_detail(&e))
            })?;

            for svc in out.services() {
                let tags: HashMap<String, String> = svc
                    .tags()
                    .iter()
                    .filter_map(|t| Some((t.key()?.to_string(), t.value()?.to_string())))
                    .collect();
                if !ServiceTags::is_managed(
                    &tags,
                    &self.config.label_namespace,
                    &self.config.controller_class,
                ) {
                    continue;
                }
                let Some(parsed) = ServiceTags::parse(&tags, &self.config.label_namespace) else {
                    continue;
                };
                if parsed.project != project.name {
                    continue;
                }
                // A service being deleted still appears in DescribeServices for a
                // while; treating it as live would make the diff think the
                // desired state is already satisfied.
                if svc
                    .status()
                    .is_some_and(|s| s == "INACTIVE" || s == "DRAINING")
                {
                    continue;
                }
                services.push(ActualService {
                    name: svc.service_name().unwrap_or_default().to_string(),
                    key: Some(spec_key(
                        &parsed.project,
                        &parsed.deployment_group,
                        &parsed.deployment_id,
                        &parsed.container,
                    )),
                    task_definition_arn: svc.task_definition().unwrap_or_default().to_string(),
                    task_definition_hash: tags
                        .get(&rise_backend_core::labels::ns_key(
                            &self.config.label_namespace,
                            TASK_DEFINITION_HASH_SUFFIX,
                        ))
                        .cloned()
                        .unwrap_or_default(),
                    desired_count: svc.desired_count(),
                    running_count: svc.running_count(),
                    deployment_id: Some(parsed.deployment_id),
                });
            }
        }
        Ok(services)
    }

    // ── applying ──────────────────────────────────────────────────────────

    /// Apply the diff's actions, isolating failures per action so one bad
    /// deployment cannot stall the others, and re-verifying leadership before
    /// each one (a create can take seconds, exceeding the validity window).
    async fn apply_actions(
        &self,
        project: &Project,
        desired: &[(DesiredService, TaskDefinitionSpec, DesiredContainer)],
        actions: &[ServiceAction],
        election: &LeaderElection,
    ) {
        let by_key: HashMap<&str, &(DesiredService, TaskDefinitionSpec, DesiredContainer)> =
            desired.iter().map(|e| (e.0.key.as_str(), e)).collect();

        for action in actions {
            if !self.confirm_leadership(election).await {
                return;
            }
            let result = match action {
                ServiceAction::Create { key, name } => match by_key.get(key.as_str()) {
                    Some(entry) => self.create_service(project, entry, name).await,
                    None => continue,
                },
                ServiceAction::UpdateTaskDefinition { key, name } => {
                    match by_key.get(key.as_str()) {
                        Some(entry) => self.update_service(project, entry, name).await,
                        None => continue,
                    }
                }
                ServiceAction::UpdateDesiredCount {
                    name,
                    desired_count,
                    ..
                } => self.scale_service(name, *desired_count).await,
                ServiceAction::Delete { name } => self.delete_service(name).await,
            };
            if let Err(e) = result {
                error!(action = ?action, "ECS action failed; will retry next tick: {:?}", e);
            }
        }
    }

    /// Write a deployment's secret parameters, then register the task definition.
    /// Secrets go first: ECS resolves them at task start, so a task definition
    /// referencing a parameter that does not exist yet would fail to start.
    async fn prepare_task_definition(
        &self,
        project: &Project,
        entry: &(DesiredService, TaskDefinitionSpec, DesiredContainer),
    ) -> Result<String> {
        let (_, task_def, desired_container) = entry;
        self.put_secrets(project, desired_container, task_def)
            .await?;
        self.register_task_definition(task_def).await
    }

    /// Write the deployment's secret env vars to SSM as `SecureString`s.
    async fn put_secrets(
        &self,
        project: &Project,
        desired_container: &DesiredContainer,
        task_def: &TaskDefinitionSpec,
    ) -> Result<()> {
        let secrets = &task_def.containers[0].secrets;
        if secrets.is_empty() {
            return Ok(());
        }
        let by_name: HashMap<&str, &str> = desired_container
            .env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        for secret in secrets {
            let Some(value) = by_name.get(secret.name.as_str()) else {
                continue;
            };
            let mut req = self
                .ssm
                .put_parameter()
                .name(&secret.value_from)
                .value(*value)
                .r#type(aws_sdk_ssm::types::ParameterType::SecureString)
                .overwrite(true);
            if let Some(key_id) = &self.config.ssm_kms_key_id {
                req = req.key_id(key_id);
            }
            req.send().await.map_err(|e| {
                anyhow::anyhow!(
                    "Failed to write secret {:?} for project {} to SSM: {}",
                    secret.name,
                    project.name,
                    aws_error_detail(&e)
                )
            })?;
        }
        debug!(
            project = %project.name,
            count = secrets.len(),
            "Wrote secret env vars to SSM Parameter Store"
        );
        Ok(())
    }

    /// Delete a retired deployment's secret parameters.
    ///
    /// Best-effort and by exact name: deleting by path prefix would be simpler,
    /// but `DeleteParameters` has no prefix form and a hand-rolled
    /// `GetParametersByPath` + delete risks reaching a sibling deployment's
    /// subtree if the prefix is ever computed wrong. Leftovers are harmless
    /// (they cost nothing on the Standard tier) whereas over-deleting breaks a
    /// live deployment.
    async fn delete_secrets_for(&self, tags: &ServiceTags) -> Result<()> {
        let prefix = ssm::deployment_path_prefix(
            &self.config.ssm_parameter_prefix,
            &tags.project,
            &tags.deployment_group,
            &tags.deployment_id,
        );
        let out = self
            .ssm
            .get_parameters_by_path()
            .path(&prefix)
            .recursive(true)
            .send()
            .await;
        let names: Vec<String> = match out {
            Ok(out) => out
                .parameters()
                .iter()
                .filter_map(|p| p.name().map(str::to_string))
                .collect(),
            Err(e) => {
                debug!(path = %prefix, "Could not list SSM parameters: {}", aws_error_detail(&e));
                return Ok(());
            }
        };
        if names.is_empty() {
            return Ok(());
        }
        let mut req = self.ssm.delete_parameters();
        for name in &names {
            req = req.names(name);
        }
        if let Err(e) = req.send().await {
            warn!(path = %prefix, "Failed to delete SSM parameters: {}", aws_error_detail(&e));
        }
        Ok(())
    }

    async fn register_task_definition(&self, spec: &TaskDefinitionSpec) -> Result<String> {
        use aws_sdk_ecs::types as ecs_types;

        let container = &spec.containers[0];
        let mut cd = ecs_types::ContainerDefinition::builder()
            .name(&container.name)
            .image(&container.image)
            .essential(true);

        for (k, v) in &container.environment {
            cd = cd.environment(ecs_types::KeyValuePair::builder().name(k).value(v).build());
        }
        for secret in &container.secrets {
            cd = cd.secrets(
                ecs_types::Secret::builder()
                    .name(&secret.name)
                    .value_from(&secret.value_from)
                    .build()
                    .map_err(|e| anyhow::anyhow!("invalid secret reference: {e}"))?,
            );
        }
        for (k, v) in &container.docker_labels {
            cd = cd.docker_labels(k, v);
        }
        if let Some(port) = container.port {
            cd = cd.port_mappings(
                ecs_types::PortMapping::builder()
                    .container_port(port as i32)
                    .protocol(ecs_types::TransportProtocol::Tcp)
                    .build(),
            );
        }
        if let Some(log) = &container.log_config {
            cd = cd.log_configuration(
                ecs_types::LogConfiguration::builder()
                    .log_driver(ecs_types::LogDriver::Awslogs)
                    .options("awslogs-group", &log.log_group)
                    .options("awslogs-region", &log.region)
                    .options("awslogs-stream-prefix", &log.stream_prefix)
                    .options("awslogs-create-group", "true")
                    .build()
                    .map_err(|e| anyhow::anyhow!("invalid log configuration: {e}"))?,
            );
        }

        let mut req = self
            .ecs
            .register_task_definition()
            .family(&spec.family)
            .network_mode(ecs_types::NetworkMode::Awsvpc)
            .requires_compatibilities(ecs_types::Compatibility::Fargate)
            .cpu(&spec.cpu)
            .memory(&spec.memory)
            .runtime_platform(
                ecs_types::RuntimePlatform::builder()
                    .cpu_architecture(ecs_types::CpuArchitecture::from(
                        spec.cpu_architecture.as_str(),
                    ))
                    .operating_system_family(ecs_types::OsFamily::Linux)
                    .build(),
            )
            .container_definitions(cd.build());
        if let Some(arn) = &spec.execution_role_arn {
            req = req.execution_role_arn(arn);
        }
        if let Some(arn) = &spec.task_role_arn {
            req = req.task_role_arn(arn);
        }

        let out = req.send().await.map_err(|e| {
            anyhow::anyhow!(
                "RegisterTaskDefinition for family {:?} failed: {}",
                spec.family,
                aws_error_detail(&e)
            )
        })?;
        out.task_definition()
            .and_then(|td| td.task_definition_arn())
            .map(str::to_string)
            .context("RegisterTaskDefinition returned no ARN")
    }

    fn network_configuration(&self) -> Result<aws_sdk_ecs::types::NetworkConfiguration> {
        use aws_sdk_ecs::types as ecs_types;
        let mut vpc = ecs_types::AwsVpcConfiguration::builder().assign_public_ip(
            if self.config.assign_public_ip {
                ecs_types::AssignPublicIp::Enabled
            } else {
                ecs_types::AssignPublicIp::Disabled
            },
        );
        for subnet in &self.config.subnets {
            vpc = vpc.subnets(subnet);
        }
        for sg in &self.config.security_groups {
            vpc = vpc.security_groups(sg);
        }
        Ok(ecs_types::NetworkConfiguration::builder()
            .awsvpc_configuration(
                vpc.build()
                    .map_err(|e| anyhow::anyhow!("invalid awsvpc configuration: {e}"))?,
            )
            .build())
    }

    /// Tags for a service, including the task-definition content hash so the
    /// next tick can tell whether the running service already carries the
    /// desired content without describing the task definition itself.
    fn service_tag_list(&self, desired: &DesiredService) -> Vec<aws_sdk_ecs::types::Tag> {
        let mut tags = desired
            .tags
            .render(&self.config.label_namespace, &self.config.controller_class);
        tags.insert(
            rise_backend_core::labels::ns_key(
                &self.config.label_namespace,
                TASK_DEFINITION_HASH_SUFFIX,
            ),
            desired.task_definition_hash.clone(),
        );
        tags.into_iter()
            .map(|(k, v)| aws_sdk_ecs::types::Tag::builder().key(k).value(v).build())
            .collect()
    }

    async fn create_service(
        &self,
        project: &Project,
        entry: &(DesiredService, TaskDefinitionSpec, DesiredContainer),
        name: &str,
    ) -> Result<()> {
        let task_definition_arn = self.prepare_task_definition(project, entry).await?;
        let (desired, _, _) = entry;

        let mut req = self
            .ecs
            .create_service()
            .cluster(&self.config.cluster)
            .service_name(name)
            .task_definition(&task_definition_arn)
            .desired_count(desired.desired_count)
            .launch_type(aws_sdk_ecs::types::LaunchType::Fargate)
            .network_configuration(self.network_configuration()?)
            // Tags must propagate to the tasks: Traefik's ECS provider and any
            // operator debugging both look at task tags, not just service tags.
            .propagate_tags(aws_sdk_ecs::types::PropagateTags::Service)
            .enable_ecs_managed_tags(true);
        for tag in self.service_tag_list(desired) {
            req = req.tags(tag);
        }

        req.send().await.map_err(|e| {
            anyhow::anyhow!("CreateService {:?} failed: {}", name, aws_error_detail(&e))
        })?;
        info!(
            project = %project.name,
            service = %name,
            replicas = desired.desired_count,
            "Created ECS service"
        );
        Ok(())
    }

    /// Point an existing service at a new task-definition revision.
    ///
    /// ECS performs the rolling replacement itself, honouring the service's
    /// deployment configuration — nothing is destroyed, so there is no window in
    /// which the deployment serves no traffic.
    async fn update_service(
        &self,
        project: &Project,
        entry: &(DesiredService, TaskDefinitionSpec, DesiredContainer),
        name: &str,
    ) -> Result<()> {
        let task_definition_arn = self.prepare_task_definition(project, entry).await?;
        let (desired, _, _) = entry;

        self.ecs
            .update_service()
            .cluster(&self.config.cluster)
            .service(name)
            .task_definition(&task_definition_arn)
            .desired_count(desired.desired_count)
            .network_configuration(self.network_configuration()?)
            .send()
            .await
            .map_err(|e| {
                anyhow::anyhow!("UpdateService {:?} failed: {}", name, aws_error_detail(&e))
            })?;

        // Re-tag so the next tick sees the new content hash and converges.
        // Without this the service would look permanently drifted and we would
        // register a revision every tick.
        let mut req = self.ecs.tag_resource().resource_arn(self.service_arn(name));
        for tag in self.service_tag_list(desired) {
            req = req.tags(tag);
        }
        if let Err(e) = req.send().await {
            warn!(
                service = %name,
                "Failed to update service tags; the next tick will re-register the task \
                 definition until this succeeds: {}",
                aws_error_detail(&e)
            );
        }

        info!(project = %project.name, service = %name, "Updated ECS service task definition");
        Ok(())
    }

    async fn scale_service(&self, name: &str, desired_count: i32) -> Result<()> {
        self.ecs
            .update_service()
            .cluster(&self.config.cluster)
            .service(name)
            .desired_count(desired_count)
            .send()
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "UpdateService (scale) {:?} failed: {}",
                    name,
                    aws_error_detail(&e)
                )
            })?;
        info!(service = %name, desired_count, "Scaled ECS service");
        Ok(())
    }

    /// Retire a service: scale to zero, then delete.
    ///
    /// `DeleteService` refuses a service with running tasks unless forced;
    /// scaling to zero first lets ECS drain them, which is what keeps a cutover
    /// from cutting live connections.
    async fn delete_service(&self, name: &str) -> Result<()> {
        if let Err(e) = self
            .ecs
            .update_service()
            .cluster(&self.config.cluster)
            .service(name)
            .desired_count(0)
            .send()
            .await
        {
            debug!(service = %name, "Scale-to-zero before delete failed: {}", aws_error_detail(&e));
        }
        self.ecs
            .delete_service()
            .cluster(&self.config.cluster)
            .service(name)
            .force(true)
            .send()
            .await
            .map_err(|e| {
                anyhow::anyhow!("DeleteService {:?} failed: {}", name, aws_error_detail(&e))
            })?;
        info!(service = %name, "Deleted ECS service");
        Ok(())
    }

    fn service_arn(&self, name: &str) -> String {
        // `TagResource` accepts a service ARN; the short form is not enough. The
        // cluster ARN prefix is not known here, so use the name-qualified form
        // ECS accepts for services in the configured cluster.
        format!("{}/{}", self.config.cluster, name)
    }

    // ── health and status ─────────────────────────────────────────────────

    /// Decide whether a deployment is ready and drive the state machine.
    ///
    /// Readiness uses the same rule as the Docker backend: for a health-checked
    /// container, Traefik's `serverStatus` is **authoritative with no fallback**,
    /// so "ready" means "Traefik is actually routing to it" and the outgoing
    /// deployment is never retired while the incoming one is invisible to the
    /// router.
    async fn reconcile_health(
        &self,
        project: &Project,
        deployment: &Deployment,
        server_status_cache: &mut HashMap<String, Option<HashMap<String, bool>>>,
    ) -> Result<()> {
        if !matches!(
            deployment.status,
            DeploymentStatus::Deploying | DeploymentStatus::Healthy | DeploymentStatus::Unhealthy
        ) {
            return Ok(());
        }

        let (container_specs, route_specs) = resolve_runtime_containers(deployment)?;
        let services = self.list_actual_services(project).await?;
        let by_key: HashMap<&str, &ActualService> = services
            .iter()
            .filter_map(|s| s.key.as_deref().map(|k| (k, s)))
            .collect();

        let custom_domains = self.store.list_project_custom_domains(project.id).await?;
        let all_environments = self.store.list_environments_for_project(project.id).await?;
        let environment = if let Some(env_id) = deployment.environment_id {
            self.store.find_environment(env_id).await?
        } else {
            None
        };
        let primary_hosts = self.url_builder.primary_ingress_hosts(
            project,
            &deployment.deployment_group,
            environment.as_ref().filter(|e| {
                e.primary_deployment_group.as_deref() == Some(&deployment.deployment_group)
            }),
            &custom_domains,
            true,
            &all_environments,
        );
        let primary_hosts: Vec<String> = primary_hosts.into_iter().map(|h| h.host).collect();

        // An empty spec set is never ready — otherwise a corrupt deployment row
        // would report Healthy while running nothing.
        let mut all_ready = !container_specs.is_empty();
        let mut reasons: Vec<String> = Vec::new();
        let mut pods: Vec<(String, Option<InspectedContainer>)> = Vec::new();

        for spec in &container_specs {
            let key = spec_key(
                &project.name,
                &deployment.deployment_group,
                &deployment.deployment_id,
                &spec.name,
            );
            let Some(actual) = by_key.get(key.as_str()) else {
                all_ready = false;
                reasons.push(format!("service for '{}' not found", spec.name));
                pods.push((spec.name.clone(), None));
                continue;
            };

            let router_withheld = rise_backend_traefik::routes_withheld(
                &project.access_class,
                &self.config.access_classes,
                &self.config.auth_backend_url,
                route_specs
                    .iter()
                    .filter(|r| r.container == spec.name)
                    .map(|r| r.access.as_ref()),
            ) || primary_hosts.is_empty();
            let has_health_path = effective_health_path(spec, &self.config.health_path).is_some();

            let service_names = rise_backend_traefik::service_names_for_spec(
                &project.name,
                &deployment.deployment_group,
                spec,
                &route_specs,
                &primary_hosts,
            );
            let server_status = self
                .fetch_server_status_aggregated(&service_names, server_status_cache)
                .await;

            if has_health_path && self.traefik_api.is_none() {
                warn!(
                    project = %project.name,
                    container = %spec.name,
                    "This project declares a health_check but \
                     `deployment_controller.traefik_api_url` is unset. Traefik's serverStatus \
                     is the authoritative readiness signal with no fallback, so this \
                     deployment can never become Healthy."
                );
            }

            let tasks = self.describe_service_tasks(&actual.name).await;
            let api_available = self.traefik_api.is_some() && server_status.is_some();

            let expected = actual.desired_count.max(1) as usize;
            for idx in 0..expected {
                let label = if expected == 1 {
                    spec.name.clone()
                } else {
                    format!("{}[{}]", spec.name, idx)
                };
                let task = tasks.get(idx);
                let inspected = task.map(|t| t.inspected.clone());
                let running = task.is_some_and(|t| t.running);

                let verdict = match (task, spec.port) {
                    (None, _) => ReadyVerdict::NotReady(format!("'{label}' task not running")),
                    (Some(task), Some(port)) => {
                        // Absent from serverStatus must be `None`, NOT `Some(false)`:
                        // "Traefik has not seen this server yet" is a different
                        // state from "Traefik says it is DOWN", and collapsing
                        // them would make a starting task look permanently failed.
                        let server_up = api_available.then(|| {
                            server_status.as_ref().and_then(|status| {
                                task.ip.as_ref().and_then(|ip| {
                                    status.get(&format!("http://{ip}:{port}")).copied()
                                })
                            })
                        });
                        replica_ready(
                            router_withheld,
                            has_health_path,
                            running,
                            api_available,
                            server_up.flatten(),
                        )
                    }
                    (Some(_), None) => {
                        // A port-less worker has no router and no probe: running
                        // is the only signal available.
                        if running {
                            ReadyVerdict::Ready
                        } else {
                            ReadyVerdict::NotReady(format!("worker '{label}' not running"))
                        }
                    }
                };

                if let ReadyVerdict::NotReady(reason) = verdict {
                    all_ready = false;
                    reasons.push(format!("'{label}': {reason}"));
                }
                // Always recorded, even when the verdict short-circuits, so the
                // Pods tab shows the full picture rather than a truncated one.
                pods.push((task.map(|t| t.name.clone()).unwrap_or(label), inspected));
            }
        }

        let metadata = build_controller_metadata(&pods, &deployment.status, all_ready);
        if let Err(e) = self
            .store
            .update_deployment_controller_metadata(deployment.id, &metadata)
            .await
        {
            warn!(
                deployment = %deployment.deployment_id,
                "Failed to persist controller metadata: {:?}", e
            );
        }

        let unhealthy_reason = if reasons.is_empty() {
            "Container health probe failing".to_string()
        } else {
            format!("Container health probe failing: {}", reasons.join("; "))
        };

        match deployment.status {
            DeploymentStatus::Deploying if all_ready => {
                self.handle_deployment_became_healthy(project, deployment)
                    .await?;
            }
            DeploymentStatus::Healthy if !all_ready => {
                self.store
                    .mark_deployment_unhealthy(deployment.id, unhealthy_reason)
                    .await?;
                self.store
                    .update_project_calculated_status(project.id)
                    .await?;
            }
            DeploymentStatus::Unhealthy if all_ready => {
                self.store.mark_deployment_healthy(deployment.id).await?;
                self.store
                    .update_project_calculated_status(project.id)
                    .await?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Merge `serverStatus` across a container's route services with OR
    /// semantics: the per-route Traefik services share the same backing servers,
    /// so a server is UP if any of them reports it UP.
    async fn fetch_server_status_aggregated(
        &self,
        service_names: &[String],
        cache: &mut HashMap<String, Option<HashMap<String, bool>>>,
    ) -> Option<HashMap<String, bool>> {
        let client = self.traefik_api.as_ref()?;
        let mut merged: HashMap<String, bool> = HashMap::new();
        let mut any = false;
        for name in service_names {
            let status = match cache.get(name) {
                Some(cached) => cached.clone(),
                None => {
                    let fetched = client.server_status(name).await;
                    cache.insert(name.clone(), fetched.clone());
                    fetched
                }
            };
            if let Some(status) = status {
                any = true;
                for (server, up) in status {
                    let entry = merged.entry(server).or_insert(false);
                    *entry = *entry || up;
                }
            }
        }
        any.then_some(merged)
    }

    /// Describe a service's tasks, projecting each onto the backend-agnostic
    /// [`InspectedContainer`] the shared `pod_status` builder consumes.
    async fn describe_service_tasks(&self, service_name: &str) -> Vec<TaskView> {
        let listed = self
            .ecs
            .list_tasks()
            .cluster(&self.config.cluster)
            .service_name(service_name)
            .send()
            .await;
        let arns: Vec<String> = match listed {
            Ok(out) => out.task_arns().to_vec(),
            Err(e) => {
                debug!(service = %service_name, "ListTasks failed: {}", aws_error_detail(&e));
                return Vec::new();
            }
        };
        if arns.is_empty() {
            return Vec::new();
        }

        let mut views = Vec::new();
        // DescribeTasks accepts up to 100 ARNs per call.
        for chunk in arns.chunks(100) {
            let mut req = self.ecs.describe_tasks().cluster(&self.config.cluster);
            for arn in chunk {
                req = req.tasks(arn);
            }
            let out = match req.send().await {
                Ok(out) => out,
                Err(e) => {
                    debug!(service = %service_name, "DescribeTasks failed: {}", aws_error_detail(&e));
                    continue;
                }
            };
            for task in out.tasks() {
                let last_status = task.last_status().unwrap_or_default().to_string();
                let running = last_status == "RUNNING";
                let name = task
                    .task_arn()
                    .and_then(|arn| arn.rsplit('/').next())
                    .unwrap_or("task")
                    .to_string();
                let ip = task
                    .attachments()
                    .iter()
                    .flat_map(|a| a.details())
                    .find(|d| d.name() == Some("privateIPv4Address"))
                    .and_then(|d| d.value())
                    .map(str::to_string);
                let container = task.containers().first();

                views.push(TaskView {
                    name: name.clone(),
                    running,
                    ip,
                    inspected: InspectedContainer {
                        // ECS status strings are uppercase; the shared pod_status
                        // mapper expects Docker's lowercase vocabulary.
                        status: Some(ecs_status_to_container_state(&last_status).to_string()),
                        running,
                        started_at: task.started_at().map(|t| t.to_string()),
                        finished_at: task.stopped_at().map(|t| t.to_string()),
                        exit_code: container.and_then(|c| c.exit_code()).map(|c| c as i64),
                        restart_count: None,
                        health: task.health_status().map(|h| h.as_str().to_lowercase()),
                        error: task
                            .stopped_reason()
                            .filter(|r| !r.is_empty())
                            .map(str::to_string),
                        ip: None,
                        published_host_port: None,
                    },
                });
            }
        }
        views
    }

    /// Mark this deployment active and supersede the outgoing one.
    async fn handle_deployment_became_healthy(
        &self,
        project: &Project,
        deployment: &Deployment,
    ) -> Result<()> {
        let previous_active = self
            .store
            .find_active_deployment_for_project_and_group(project.id, &deployment.deployment_group)
            .await?;

        self.store.mark_deployment_healthy(deployment.id).await?;

        if let Some(previous) = previous_active {
            if previous.id != deployment.id
                && !rise_backend_core::state_machine::is_terminal(&previous.status)
            {
                self.store
                    .mark_deployment_terminating(previous.id, TerminationReason::Superseded)
                    .await?;
                // The retired deployment's secrets go with it, so a later
                // rollback re-creates them from the database rather than reading
                // a stale value.
                let tags = ServiceTags {
                    project: project.name.clone(),
                    deployment_group: previous.deployment_group.clone(),
                    deployment_id: previous.deployment_id.clone(),
                    deployment_uuid: previous.id.to_string(),
                    container: String::new(),
                    environment: None,
                    env_hash: String::new(),
                    image: String::new(),
                    route_hash: String::new(),
                };
                let _ = self.delete_secrets_for(&tags).await;
            }
        }

        // Any other still-active row in this group is a straggler; retire it too.
        let siblings = self
            .store
            .find_non_terminal_deployments_for_project_and_group(
                project.id,
                &deployment.deployment_group,
            )
            .await?;
        for other in siblings {
            if other.id == deployment.id {
                continue;
            }
            if other.is_active && !rise_backend_core::state_machine::is_terminal(&other.status) {
                self.store
                    .mark_deployment_terminating(other.id, TerminationReason::Superseded)
                    .await?;
            }
        }

        self.store
            .mark_deployment_as_active(deployment.id, project.id, &deployment.deployment_group)
            .await?;
        self.store
            .update_project_calculated_status(project.id)
            .await?;
        info!(
            project = %project.name,
            deployment = %deployment.deployment_id,
            "Deployment is healthy and now active"
        );
        Ok(())
    }

    fn traefik_render_config(&self) -> rise_backend_traefik::TraefikRenderConfig<'_> {
        rise_backend_traefik::TraefikRenderConfig {
            label_namespace: &self.config.label_namespace,
            controller_class: &self.config.controller_class,
            traefik_entrypoint: &self.config.traefik_entrypoint,
            traefik_certresolver: self.config.traefik_certresolver.as_deref(),
            // ECS tasks are on awsvpc ENIs; the ECS provider resolves them itself
            // and would mis-resolve if handed a Docker network name.
            network: None,
            auth_backend_url: &self.config.auth_backend_url,
            access_classes: &self.config.access_classes,
        }
    }
}

/// One observed ECS task, reduced to what readiness and `pod_status` need.
struct TaskView {
    name: String,
    running: bool,
    /// The task's ENI private IP — the key Traefik's `serverStatus` uses
    /// (`http://{ip}:{port}`).
    ip: Option<String>,
    inspected: InspectedContainer,
}

/// Map an ECS task `lastStatus` onto the Docker-shaped container state the
/// shared `pod_status` builder understands, so the frontend's Pods tab renders
/// identically on every backend.
pub(crate) fn ecs_status_to_container_state(last_status: &str) -> &'static str {
    match last_status {
        "RUNNING" => "running",
        // Everything before RUNNING is a pending state; the Pods tab shows these
        // as "waiting", which is what a user expects while a task starts.
        "PROVISIONING" | "PENDING" | "ACTIVATING" => "created",
        "DEACTIVATING" | "STOPPING" | "DEPROVISIONING" => "restarting",
        "STOPPED" => "exited",
        _ => "",
    }
}

/// Whether a configured controller class matches an Organization's.
///
/// An empty configured class matches everything (a legacy or unconfigured
/// install); otherwise the Organization must name this class exactly. An
/// Organization with no class set belongs to no controller.
pub(crate) fn controller_class_matches(configured: &str, org_class: Option<&str>) -> bool {
    if configured.is_empty() {
        return true;
    }
    org_class == Some(configured)
}

/// Clamp a requested replica count into the range a service may carry.
pub(crate) fn clamp_replicas(requested: Option<u32>) -> u32 {
    requested.unwrap_or(1).clamp(1, MAX_REPLICAS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_class_matching() {
        // An unconfigured install must keep working (empty matches all), but a
        // configured one must never adopt an Organization that names a different
        // controller — that would mean two controllers fighting over one
        // workload, each deleting the other's services as orphans.
        assert!(controller_class_matches("", Some("anything")));
        assert!(controller_class_matches("", None));
        assert!(controller_class_matches("default", Some("default")));
        assert!(!controller_class_matches("default", Some("other")));
        assert!(
            !controller_class_matches("default", None),
            "an Organization with no class belongs to no controller"
        );
    }

    #[test]
    fn ecs_task_states_map_onto_the_shared_pod_status_vocabulary() {
        // The Pods tab is rendered from one shared JSON shape across all
        // backends. ECS reports uppercase lifecycle states that mean nothing to
        // that mapper — leaving them unmapped would show every task as "Unknown"
        // for its entire life.
        assert_eq!(ecs_status_to_container_state("RUNNING"), "running");
        assert_eq!(ecs_status_to_container_state("PENDING"), "created");
        assert_eq!(ecs_status_to_container_state("PROVISIONING"), "created");
        assert_eq!(ecs_status_to_container_state("STOPPED"), "exited");
        assert_eq!(ecs_status_to_container_state("STOPPING"), "restarting");
        // An unrecognised state must not masquerade as running.
        assert_eq!(ecs_status_to_container_state("SOMETHING_NEW"), "");
    }

    #[test]
    fn clamp_replicas_defaults_and_bounds() {
        // A runaway replicas value would exhaust the account's Fargate vCPU
        // quota (6 by default) and starve every other workload in the cluster.
        assert_eq!(clamp_replicas(None), 1);
        assert_eq!(clamp_replicas(Some(0)), 1);
        assert_eq!(clamp_replicas(Some(3)), 3);
        assert_eq!(clamp_replicas(Some(100)), 100);
        assert_eq!(clamp_replicas(Some(10_000)), MAX_REPLICAS);
    }
}
