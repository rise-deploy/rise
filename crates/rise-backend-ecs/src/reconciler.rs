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
//!    between drift detection and readiness, and a task definition
//!    is registered only when its content hash actually moves.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, Result};
use rise_backend_core::events::{EventSeverity, EventSource, ForwardedEvent};
use rise_backend_core::models::{Deployment, DeploymentStatus, Project};
use rise_backend_core::observation::{derive_events, ContainerObservation, ObservedState};
use rise_backend_core::{
    effective_health_path, hash_env, merge_container_env, pin_system_env, redact_secrets_for_hash,
    resolve_deployment_env_vars, resolve_runtime_containers, rise_system_env_vars,
    should_have_infrastructure, spec_key, DeploymentStore, DeploymentUrlBuilder, DesiredContainer,
    DesiredRoute, EncryptionProvider, ResolvedDeploymentEnvVars, DEPLOYING_TIMEOUT_MINUTES,
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
use crate::capacity::Capacity;
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

/// `ListServices` returns 10 ARNs per page by default and accepts up to 100.
/// The default would cost ten times the calls for the same result.
const LIST_SERVICES_PAGE: i32 = 100;

/// A Rise-managed ECS service as the cluster currently reports it, paired with
/// the tags that identify what it is meant to be running.
struct ManagedService {
    service: ActualService,
    tags: ServiceTags,
}

/// Where a project stands relative to this controller, for one tick.
///
/// Three states rather than a boolean because the two non-owning ones call for
/// opposite handling of the services we already built for that project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ownership {
    /// The project's organization names our controller class. Converge it.
    Ours,
    /// The organization names a different class: another controller owns this
    /// project now. The services still running under our tags are ours alone to
    /// retire -- the new controller sees only its own class and will never
    /// collect them, and until they are gone the project is served by two
    /// independent deployments at once.
    Migrated,
    /// No class could be resolved: the project has no organization linkage, or
    /// its organization names no `deploymentControllerClass`. Neither is
    /// evidence that anyone else is serving the project, so its live services
    /// stay untouched; a terminal deployment still lets the ordinary orphan
    /// sweep reach them.
    Unlinked,
}

/// Every Rise-managed service in the cluster, read once per tick.
///
/// Holding the whole cluster rather than one project's slice is what lets the
/// orphan sweep see services whose project row is gone — the case per-project
/// scoping structurally cannot reach.
struct ClusterServices {
    services: Vec<ManagedService>,
    /// Services carrying our `managed-by` and `controller-class` markers but
    /// missing one of the identity tags `ServiceTags::parse` requires.
    ///
    /// They are ours -- nothing else stamps both markers -- but they name no
    /// deployment, so no desired entry can ever match them and no tick can
    /// converge them. Kept apart from `services` because every consumer there
    /// reads `tags`, and retired wholesale by the orphan sweep.
    unattributable: Vec<ActualService>,
}

impl ClusterServices {
    /// The services tagged as belonging to `project`.
    ///
    /// Matches on the project's UUID tag when a service carries one, falling
    /// back to the project name only for services tagged before that field
    /// existed. The name is mutable -- a rename leaves already-running
    /// services tagged with the old one -- so matching on it alone would make
    /// every renamed project's live services invisible to this projection: the
    /// reconciler would see no actual state, create a fresh set under the new
    /// name, and orphan the old one until it happened to go terminal some other
    /// way. The UUID tag is immutable for the project's lifetime, so it
    /// survives the rename.
    fn for_project(&self, project: &Project) -> Vec<ActualService> {
        let project_uuid = project.id.to_string();
        self.services
            .iter()
            .filter(|m| {
                if m.tags.project_uuid.is_empty() {
                    m.tags.project == project.name
                } else {
                    m.tags.project_uuid == project_uuid
                }
            })
            .map(|m| m.service.clone())
            .collect()
    }
}

/// Tag suffix carrying the content hash of the task definition a service runs.
/// Stamped alongside the bookkeeping tags so drift detection is a tag comparison
/// rather than a `DescribeTaskDefinition` per service per tick. A best-effort
/// cache: the durable convergence marker is [`persisted_td_hash`], which a tag
/// write cannot corrupt.
const TASK_DEFINITION_HASH_SUFFIX: &str = "task-definition-hash";

/// Keys under a deployment's `controller_metadata` where the ECS reconciler
/// records the content hash of the task definition its service has converged to.
///
/// This is the authoritative drift marker, not the service's `task-definition-hash`
/// tag: the tag is stamped by a separate `TagResource` call *after* `UpdateService`,
/// so a persistently failing tag write (e.g. a role missing `ecs:TagResource`)
/// would leave the service looking drifted every tick and re-roll it forever. The
/// hash is instead persisted here, in the same DB write that records health, so it
/// cannot fail independently of the update it records.
const ECS_META_KEY: &str = "ecs";
const TD_HASH_META_KEY: &str = "task_definition_hash";

impl EcsReconciler {
    /// Load the previous observations, derive what changed, and store both.
    async fn record_observations(
        &self,
        deployment: &Deployment,
        observed: &[ContainerObservation],
    ) -> anyhow::Result<()> {
        let previous = self
            .store
            .list_container_observations(deployment.id)
            .await?;
        let events = derive_events(&previous, observed, chrono::Utc::now());
        self.store
            .record_container_observations(deployment.id, EventSource::Ecs, observed, &events)
            .await
    }
}

/// Project one observed ECS task onto the shared observation shape.
///
/// The subject is the task id, not a replica ordinal. ECS has no per-replica
/// identity — a service owns its replicas through `desiredCount`, and the task
/// list comes back unordered — so an ordinal here would be positional fiction
/// that moves whenever any task is replaced.
///
/// `restart_count` is left unset because Fargate has none: it replaces a task
/// rather than restarting a container in place. That replacement is still
/// observable, as this task's subject disappearing and another's appearing.
fn observe_task(task: &TaskView, container: &str) -> ContainerObservation {
    // ECS reports uppercase; everything before RUNNING is on its way up.
    let state = match task.last_status.as_str() {
        "RUNNING" => ObservedState::Running,
        "PROVISIONING" | "PENDING" | "ACTIVATING" => ObservedState::Pending,
        "DEACTIVATING" | "STOPPING" | "DEPROVISIONING" | "STOPPED" => ObservedState::Exited,
        _ if task.running => ObservedState::Running,
        _ => ObservedState::Unknown,
    };

    ContainerObservation {
        subject: task.name.clone(),
        container: container.to_string(),
        // The subject already is the instance: a replaced task is a new task id.
        instance: Some(task.name.clone()),
        replica: None,
        state,
        started_at: task.started_at,
        finished_at: task.stopped_at,
        exit_code: task.exit_code,
        restart_count: None,
        health: task.health_status.clone(),
        reason: task.stopped_reason.clone(),
        image: task.image.clone(),
    }
}

/// Turn one service message into a forwardable event.
///
/// ECS's id is the dedupe key, so re-reading the same window on the next tick
/// records nothing new. The raw message is kept verbatim: it is the event, and
/// paraphrasing would lose the placement reason that makes it worth having.
fn forwarded_from_ecs(n: &crate::service::ServiceNarrative) -> Option<ForwardedEvent> {
    let severity = match crate::service::narrative_severity(&n.message) {
        crate::service::NarrativeSeverity::Error => EventSeverity::Error,
        crate::service::NarrativeSeverity::Warning => EventSeverity::Warning,
        crate::service::NarrativeSeverity::Info => EventSeverity::Info,
    };
    Some(ForwardedEvent {
        dedupe_key: n.id.clone(),
        occurred_at: n.created_at?,
        severity,
        message: n.message.clone(),
        // ECS names a task in the prose but gives no structured handle, and a
        // subject parsed out of a sentence would be wrong the first time AWS
        // rewords it. Service-wide is the honest scope.
        subject: None,
        attributes: serde_json::json!({}),
    })
}

/// Describe a resource representation that differs from the user's request.
///
/// The event is emitted from desired-state computation, after the task
/// definition has a valid Fargate size. Its key includes both sides of the
/// representation so a later, different resolution remains visible while
/// repeated reconcile ticks remain idempotent.
fn resource_adjustment_event(
    container: &str,
    requested_cpu: &str,
    requested_memory: &str,
    size: crate::sizing::TaskSize,
) -> ForwardedEvent {
    ForwardedEvent {
        dedupe_key: format!(
            "resource-adjusted:{container}:{requested_cpu}:{requested_memory}:{}:{}",
            size.cpu_units, size.memory_mib
        ),
        occurred_at: chrono::Utc::now(),
        severity: EventSeverity::Warning,
        message: "Resources were rounded up to satisfy Fargate task-size limits".to_string(),
        subject: Some(container.to_string()),
        attributes: serde_json::json!({
            rise_backend_core::events::attributes::EVENT_TYPE:
                rise_backend_core::events::attributes::RESOURCE_ADJUSTED,
            rise_backend_core::events::attributes::CONTAINER: container,
            rise_backend_core::events::attributes::REQUESTED_CPU: requested_cpu,
            rise_backend_core::events::attributes::REQUESTED_MEMORY: requested_memory,
            rise_backend_core::events::attributes::RESOLVED_CPU_UNITS: size.cpu_units,
            rise_backend_core::events::attributes::RESOLVED_MEMORY_MIB: size.memory_mib,
        }),
    }
}

/// Project an ECS service's event list onto the shape the reconciler keeps.
///
/// ECS returns roughly the last hour, newest first, and caps the list — so this
/// is a window, not a history. Anything relying on having seen every message
/// would be wrong; forwarding is deduplicated by id precisely because the same
/// window is re-read on every tick.
fn service_narrative(svc: &aws_sdk_ecs::types::Service) -> Vec<crate::service::ServiceNarrative> {
    svc.events()
        .iter()
        .filter_map(|e| {
            Some(crate::service::ServiceNarrative {
                id: e.id()?.to_string(),
                created_at: e
                    .created_at()
                    .and_then(|t| chrono::DateTime::from_timestamp(t.secs(), 0)),
                message: e.message()?.to_string(),
            })
        })
        .collect()
}

/// Read the converged task-definition hash a prior tick persisted for a
/// deployment, if any.
fn persisted_td_hash(controller_metadata: &serde_json::Value) -> Option<String> {
    controller_metadata
        .get(ECS_META_KEY)?
        .get(TD_HASH_META_KEY)?
        .as_str()
        .map(str::to_string)
}

/// The service-level shape ECS reports for a running service.
///
/// `None` when there is no `awsvpc` configuration to read. Unknown is not drift:
/// a shape we cannot see is left alone rather than "corrected" into a rolling
/// replacement on the strength of a missing field.
fn observed_shape(svc: &aws_sdk_ecs::types::Service) -> Option<crate::service::ObservedShape> {
    let vpc = svc.network_configuration()?.awsvpc_configuration()?;
    Some(crate::service::ObservedShape {
        subnets: vpc.subnets().iter().cloned().collect(),
        security_groups: vpc.security_groups().iter().cloned().collect(),
        assign_public_ip: matches!(
            vpc.assign_public_ip(),
            Some(aws_sdk_ecs::types::AssignPublicIp::Enabled)
        ),
        launch_type: svc.launch_type().map(|lt| lt.as_str().to_string()),
        capacity_providers: svc
            .capacity_provider_strategy()
            .iter()
            .map(|item| item.capacity_provider().to_string())
            .collect(),
    })
}

/// Place a service's tasks on the configured capacity.
///
/// `launchType` and `capacityProviderStrategy` are mutually exclusive on
/// `CreateService`, so this matches exhaustively rather than setting both --
/// which also means a new [`Capacity`] variant fails the build here instead of
/// silently placing tasks on the default.
fn apply_capacity(
    req: aws_sdk_ecs::operation::create_service::builders::CreateServiceFluentBuilder,
    capacity: &Capacity,
) -> Result<aws_sdk_ecs::operation::create_service::builders::CreateServiceFluentBuilder> {
    use aws_sdk_ecs::types as ecs_types;
    Ok(match capacity {
        Capacity::Fargate => req.launch_type(ecs_types::LaunchType::Fargate),
        Capacity::Ec2 => req.launch_type(ecs_types::LaunchType::Ec2),
        Capacity::Provider(provider) => {
            let mut item = ecs_types::CapacityProviderStrategyItem::builder()
                .capacity_provider(&provider.name);
            if let Some(base) = provider.base {
                item = item.base(base);
            }
            if let Some(weight) = provider.weight {
                item = item.weight(weight);
            }
            req.capacity_provider_strategy(
                item.build()
                    .map_err(|e| anyhow::anyhow!("invalid capacity provider strategy: {e}"))?,
            )
        }
    })
}

/// The `requiresCompatibilities` token for a capacity.
///
/// Matched rather than converted from a string: `Compatibility::from` maps an
/// unrecognised value to an `Unknown` variant that serialises verbatim, so a
/// drift between the two enums would reach `RegisterTaskDefinition` as an
/// AWS-shaped error per deployment rather than a compile error here.
fn compatibility_type(
    compatibility: crate::capacity::Compatibility,
) -> aws_sdk_ecs::types::Compatibility {
    match compatibility {
        crate::capacity::Compatibility::Fargate => aws_sdk_ecs::types::Compatibility::Fargate,
        crate::capacity::Compatibility::Ec2 => aws_sdk_ecs::types::Compatibility::Ec2,
    }
}

/// Fold the converged task-definition hash into a controller-metadata object,
/// leaving any sibling keys untouched. A no-op if `hash` is `None` or the
/// metadata is not a JSON object.
fn with_persisted_td_hash(
    mut metadata: serde_json::Value,
    hash: Option<&str>,
) -> serde_json::Value {
    if let (Some(hash), Some(obj)) = (hash, metadata.as_object_mut()) {
        obj.insert(
            ECS_META_KEY.to_string(),
            serde_json::json!({ TD_HASH_META_KEY: hash }),
        );
    }
    metadata
}

/// Owned controller configuration.
#[derive(Clone)]
pub struct ReconcilerConfig {
    pub cluster: String,
    pub region: String,
    pub subnets: Vec<String>,
    pub security_groups: Vec<String>,
    pub assign_public_ip: bool,
    /// Where workload tasks run. `awsvpc` either way -- see
    /// [`crate::capacity`].
    pub capacity: Capacity,
    pub execution_role_arn: Option<String>,
    pub task_role_arn: Option<String>,
    /// Secrets Manager secret ECS reads to authenticate pulls from a private
    /// non-ECR registry. Unset for ECR, whose pulls the execution role
    /// authenticates directly.
    pub repository_credentials_secret_arn: Option<String>,
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

/// Whether a service whose deployment resolved to `status` should be collected.
///
/// `None` means the database has no such deployment at all -- deleted project,
/// restored backup -- and the service is unambiguously abandoned. A terminal
/// status means the deployment is finished and nothing will recreate its
/// services. Anything else belongs to some project's reconcile pass, even one
/// this tick did not visit, and must be left alone.
fn is_orphaned(status: Option<&DeploymentStatus>) -> bool {
    match status {
        Some(s) => rise_backend_core::state_machine::is_terminal(s),
        None => true,
    }
}

/// Why a managed service should be retired, or `None` to leave it running.
///
/// `project_migrated` outranks the deployment status deliberately. A project
/// whose organization now names a different controller class has a live
/// deployment on *both* controllers, and a proxy that is not class-constrained
/// routes to both -- so a `Healthy` status is the case that most needs
/// retiring, not the case that exempts it.
fn retirement_reason(
    project_migrated: bool,
    deployment: Option<&DeploymentStatus>,
) -> Option<String> {
    if project_migrated {
        return Some("its project moved to another deployment controller class".to_string());
    }
    if !is_orphaned(deployment) {
        return None;
    }
    Some(match deployment {
        Some(status) => format!("its deployment is {status:?}"),
        None => "its deployment is absent from the database".to_string(),
    })
}

/// Whether a permanent rejection should fail `status` outright, or only
/// protect its services and retry.
///
/// Fail-fast exists for a deployment still on its way up: the message names
/// what the user must change, and without it the deploying timeout would
/// overwrite it with "timed out after 300 seconds".
///
/// A deployment that already reached `Healthy`/`Unhealthy` is *serving traffic*
/// and must never be failed on this path. Nothing about it changed -- its env
/// vars are an immutable snapshot -- so a permanent rejection appearing against
/// it means the *rules* changed underneath it: an Organization moved onto this
/// controller class with a live deployment whose sizing, container count or
/// identity block this backend does not support. Failing it would mark it
/// terminal, drop it from the desired set, and delete a running app's services
/// within the same tick, with no user action and no way back. Protect and retry
/// instead: the operator sees the warning, and the app keeps serving until a
/// deploy replaces it.
fn rejection_fails_deployment(status: &DeploymentStatus) -> bool {
    matches!(
        status,
        DeploymentStatus::Pushed | DeploymentStatus::Deploying
    )
}

/// The user-facing text of a permanent rejection.
///
/// [`PermanentDeployError`] is a marker with an empty `Display`, and anyhow's
/// alternate form joins the chain with `": "` -- so formatting the error
/// directly yields a message with a leading `": "`. This renders the chain
/// without the marker's empty link.
fn rejection_message(err: &anyhow::Error) -> String {
    let parts: Vec<String> = err
        .chain()
        .map(|c| c.to_string())
        .filter(|c| !c.is_empty())
        .collect();
    if parts.is_empty() {
        "the deployment was rejected by the ECS backend".to_string()
    } else {
        parts.join(": ")
    }
}

/// Attached as context to an error a deployment can never recover from.
///
/// `reconcile_project` looks for it to decide whether to fail the deployment
/// with the message the operator needs, or protect it and retry.
#[derive(Debug, Clone, Copy)]
pub struct PermanentDeployError;

impl std::fmt::Display for PermanentDeployError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately empty of its own text: the message the user sees is the
        // one the rejection wrote, not this marker.
        f.write_str("")
    }
}

impl std::error::Error for PermanentDeployError {}

/// Background reconciler converging the ECS cluster with the database.
pub struct EcsReconciler {
    ecs: aws_sdk_ecs::Client,
    ssm: aws_sdk_ssm::Client,
    store: Arc<dyn DeploymentStore>,
    /// Retained solely for leader election; all deployment state goes through
    /// [`Self::store`].
    db_pool: PgPool,
    url_builder: Arc<DeploymentUrlBuilder>,
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

        // One read of the cluster for the whole tick. Everything below works
        // off this snapshot: the per-project diff, readiness, the termination
        // check and the orphan sweep. A failure here aborts the tick rather
        // than degrading to a partial view, because a partial view of what is
        // running is a view in which live services look orphaned.
        let cluster = self.list_managed_services().await?;

        // Projects the orphan sweep must not touch this tick. Two reasons land
        // a project here, and they are different reasons:
        //
        //   - the per-project pass already handled it, and `diff_services`
        //     knows things the sweep does not (in-flight deployments, the
        //     protected set). Re-deleting behind it is at best duplicate calls;
        //   - its ownership could not be resolved at all, so nothing about its
        //     state is trustworthy this tick.
        //
        // A project we resolved as *not ours* is deliberately absent: its
        // services still carry our own controller-class tag, so we created
        // them and no other controller will ever clean them up.
        let mut sweep_exempt: HashSet<String> = HashSet::new();
        // Projects that moved to another controller class. The sweep retires
        // their services whatever state their deployment is in.
        let mut migrated: HashSet<String> = HashSet::new();

        for project in projects {
            match self.ownership_of(&project, &mut org_class_cache).await {
                Ok(Ownership::Ours) => {}
                Ok(Ownership::Migrated) => {
                    migrated.insert(project.name.clone());
                    continue;
                }
                // Not ours to converge, and not evidence that anyone else's is
                // either — the sweep sees it, but only retires what is orphaned
                // on the ordinary deployment-status test.
                Ok(Ownership::Unlinked) => continue,
                // Ownership unknown. Shield it: a transient Organization read
                // must never escalate into deleting a running workload.
                Err(e) => {
                    warn!(project = %project.name, "Failed to resolve ownership: {:?}", e);
                    sweep_exempt.insert(project.name.clone());
                    continue;
                }
            }
            sweep_exempt.insert(project.name.clone());
            // The lease is global, so losing it aborts the whole tick rather
            // than just this project.
            if !self.confirm_leadership(election).await {
                return Ok(());
            }
            if let Err(e) = self.reconcile_project(&project, &cluster, election).await {
                error!(project = %project.name, "Failed to reconcile project: {:?}", e);
            }
        }

        if let Err(e) = self
            .collect_orphans(&cluster, &sweep_exempt, &migrated, election)
            .await
        {
            error!("Cluster-wide orphan sweep failed: {:?}", e);
        }
        Ok(())
    }

    /// Delete services whose deployment the database no longer knows.
    ///
    /// Per-project reconciliation cannot do this. It only ever visits services
    /// whose project tag matches a project row it is currently reconciling, so
    /// the moment a project row disappears — or stops being ours — its services
    /// become unreachable by every future tick. They keep running, keep costing,
    /// and keep being routed to. Three ways in:
    ///
    ///   - a `DeleteService` that failed once, after the deployment was already
    ///     marked terminal and the project row was deleted;
    ///   - an Organization's `deploymentControllerClass` changing, so
    ///     `ownership_of` stops returning `Ours` for projects we built services
    ///     for — the new controller only ever sees its own tags, so the
    ///     services we left behind are ours alone to collect. These retire
    ///     whatever state their deployment is in: the live ones are exactly the
    ///     ones that would otherwise be served twice over;
    ///   - a database restored to a point before the project existed;
    ///   - a service of ours whose identity tags were lost, which names no
    ///     deployment at all and so can never be converged.
    ///
    /// Keyed on the deployment UUID rather than the project name: it is the tag
    /// that maps to a row we can definitively resolve, and a project can be
    /// renamed. Only services carrying our own `managed-by` *and*
    /// `controller-class` markers are considered, so a second Rise controller
    /// on the same cluster is never touched.
    async fn collect_orphans(
        &self,
        cluster: &ClusterServices,
        sweep_exempt: &HashSet<String>,
        migrated: &HashSet<String>,
        election: &LeaderElection,
    ) -> Result<()> {
        // Services of ours that name no deployment. Nothing can converge them
        // and nothing else will collect them, so they go first and
        // unconditionally -- there is no status to consult.
        for service in &cluster.unattributable {
            if !self.confirm_leadership(election).await {
                return Ok(());
            }
            warn!(
                service = %service.name,
                "Deleting an ECS service tagged as ours but missing the identity tags \
                 that say which deployment it serves"
            );
            if let Err(e) = self.delete_service(&service.name).await {
                warn!(service = %service.name, "Failed to delete it: {:?}", e);
            }
        }

        for managed in &cluster.services {
            let (name, tags) = (&managed.service.name, &managed.tags);
            if sweep_exempt.contains(&tags.project) {
                continue;
            }

            let project_migrated = migrated.contains(&tags.project);
            // A migrated project needs no status: it retires either way, and
            // skipping the lookup keeps the sweep off the database for it.
            let deployment = if project_migrated {
                None
            } else {
                // Resolve the deployment this service claims to serve. Anything
                // we cannot parse or cannot look up is left alone: deleting on a
                // failed lookup would turn a transient database error into data
                // loss.
                let Ok(uuid) = Uuid::parse_str(&tags.deployment_uuid) else {
                    continue;
                };
                match self.store.find_deployment(uuid).await {
                    Ok(d) => d,
                    Err(e) => {
                        warn!(
                            service = %name,
                            "Could not resolve deployment {uuid} while sweeping orphans; \
                             leaving the service alone: {:?}", e
                        );
                        continue;
                    }
                }
            };

            let Some(reason) =
                retirement_reason(project_migrated, deployment.as_ref().map(|d| &d.status))
            else {
                continue;
            };

            if !self.confirm_leadership(election).await {
                return Ok(());
            }

            warn!(
                service = %name,
                deployment = %tags.deployment_uuid,
                project = %tags.project,
                "Deleting orphaned ECS service: {reason}"
            );
            if let Err(e) = self.delete_service(name).await {
                warn!(service = %name, "Failed to delete orphaned service: {:?}", e);
                continue;
            }
            // The service is gone; its secrets would otherwise outlive it in
            // Parameter Store forever.
            if let Err(e) = self
                .delete_secrets_for(&tags.project, &tags.deployment_group, &tags.deployment_id)
                .await
            {
                warn!(service = %name, "Failed to delete the orphan's secrets: {:?}", e);
            }
        }
        Ok(())
    }

    /// How this controller stands to `project` — i.e. whether the project's
    /// Organization's `deploymentControllerClass` matches ours.
    async fn ownership_of(
        &self,
        project: &Project,
        org_class_cache: &mut HashMap<Uuid, Option<String>>,
    ) -> Result<Ownership> {
        let Some(org_uid) = self.store.organization_uid_for_project(project.id).await? else {
            warn!(
                project = %project.name,
                "Project has no organization linkage; skipping in ECS reconciler"
            );
            return Ok(Ownership::Unlinked);
        };
        let org_class = self
            .resolve_org_controller_class(org_uid, org_class_cache)
            .await?;
        Ok(ownership_from_class(
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

    async fn reconcile_project(
        &self,
        project: &Project,
        cluster: &ClusterServices,
        election: &LeaderElection,
    ) -> Result<()> {
        let mut actual = cluster.for_project(project);

        // 1. Status transitions, isolated per deployment. A `Terminating`
        // deployment completes immediately here; its services are retired by
        // the diff on the next tick (`should_have_infrastructure` keeps them
        // in the desired set while still Terminating, dropped once terminal).
        // That ordering means the project-deletion controller (5s poll,
        // against this loop's 30s) can delete the project row before the
        // services are gone, putting them out of reach of every future
        // per-project pass -- waiting here cannot fix it, since the very diff
        // that removes them will not run until this transition happens. The
        // cluster-wide orphan sweep is what closes it.
        let non_terminal = self
            .store
            .list_non_terminal_deployments_for_project(project.id)
            .await?;
        for deployment in &non_terminal {
            if let Err(e) = rise_backend_core::lifecycle::perform_status_transition(
                self.store.as_ref(),
                project,
                deployment,
            )
            .await
            {
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
                Ok((mut entries, events)) => {
                    desired.append(&mut entries);
                    if !events.is_empty() {
                        match self
                            .store
                            .forward_backend_events(deployment.id, EventSource::Ecs, &events)
                            .await
                        {
                            Ok(n) if n > 0 => {
                                debug!(
                                    deployment = %deployment.deployment_id,
                                    "Forwarded {n} new ECS deployment event(s)"
                                );
                            }
                            Ok(_) => {}
                            Err(e) => warn!(
                                deployment = %deployment.deployment_id,
                                "Failed to forward ECS deployment events: {:?}",
                                e
                            ),
                        }
                    }
                }
                Err(e) if e.downcast_ref::<PermanentDeployError>().is_some() => {
                    let message = rejection_message(&e);
                    if rejection_fails_deployment(&deployment.status) {
                        // Retrying would fail identically forever, and the
                        // deploying timeout would eventually replace this
                        // message with "timed out after 300 seconds" -- burying
                        // the one piece of text that tells the user what to
                        // change.
                        warn!(
                            deployment = %deployment.deployment_id,
                            "Rejecting deployment: {}", message
                        );
                        if self
                            .store
                            .mark_deployment_failed(deployment.id, &message)
                            .await?
                            .is_some()
                        {
                            self.store
                                .update_project_calculated_status(project.id)
                                .await?;
                        }
                    } else {
                        // Already serving, or on its way down. See
                        // `rejection_fails_deployment`.
                        warn!(
                            deployment = %deployment.deployment_id,
                            status = ?deployment.status,
                            "This backend cannot express the running deployment, but it is \
                             not failed on that account; protecting its services: {}", message
                        );
                        protected_deployment_ids.insert(deployment.deployment_id.clone());
                    }
                }
                Err(e) => {
                    warn!(
                        deployment = %deployment.deployment_id,
                        "Failed to compute desired state; protecting its services from GC: {:?}", e
                    );
                    protected_deployment_ids.insert(deployment.deployment_id.clone());
                }
            }
        }

        // The task-definition hash each deployment desires, by deployment id.
        let desired_hash_by_deployment: HashMap<String, String> = desired
            .iter()
            .map(|(d, _, _)| (d.tags.deployment_id.clone(), d.task_definition_hash.clone()))
            .collect();

        // Overlay the durable convergence marker onto the cluster snapshot: when
        // a prior tick recorded that a service reached the hash it still desires,
        // treat it as converged even if its `task-definition-hash` tag is stale
        // (a failed `TagResource` write). Without this a persistent tag failure
        // would re-roll the service every tick. The tag remains a fast path; the
        // persisted hash is the backstop.
        let persisted_hash_by_deployment: HashMap<&str, String> = non_terminal
            .iter()
            .filter_map(|d| {
                Some((
                    d.deployment_id.as_str(),
                    persisted_td_hash(&d.controller_metadata)?,
                ))
            })
            .collect();
        for a in &mut actual {
            if let Some(deployment_id) = a.deployment_id.as_deref() {
                if let (Some(desired_hash), Some(persisted)) = (
                    desired_hash_by_deployment.get(deployment_id),
                    persisted_hash_by_deployment.get(deployment_id),
                ) {
                    if desired_hash == persisted {
                        a.task_definition_hash = desired_hash.clone();
                    }
                }
            }
        }

        // 3. Diff and apply against the snapshot taken at the top of the tick.
        let desired_services: Vec<DesiredService> =
            desired.iter().map(|(d, _, _)| d.clone()).collect();
        let actions = service::diff_services(&desired_services, &actual, &protected_deployment_ids);

        // Capacity is reported, never applied in place: ECS restricts which
        // launch-type/capacity-provider transitions `UpdateService` performs,
        // and the permitted ones need a forced new deployment. A retopology is
        // the operator's call, and the next deploy carries it for free -- Rise
        // creates one service per deployment, so those are new services.
        let capacity_drift = service::capacity_drift(&desired_services, &actual);
        if let Some(first) = capacity_drift.first() {
            // One line for the whole project, not one per service: the condition
            // persists until the project is redeployed, and this loop runs every
            // `reconcile_interval_secs`.
            warn!(
                project = %project.name,
                services = capacity_drift.len(),
                "{} of this project's services run on {} but the install is configured \
                 for {}. ECS cannot move a running service between capacities; the next \
                 deployment of this project will use the configured one.",
                capacity_drift.len(),
                first.observed,
                first.desired,
            );
        }

        if !actions.is_empty() && !self.confirm_leadership(election).await {
            return Ok(());
        }
        let registered = self
            .apply_actions(project, &desired, &actual, &actions, election)
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
                .reconcile_health(
                    project,
                    deployment,
                    &actual,
                    &registered,
                    desired_hash_by_deployment
                        .get(&deployment.deployment_id)
                        .map(String::as_str),
                    &mut server_status_cache,
                )
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

    // ── desired state ─────────────────────────────────────────────────────

    /// Compute the services (and their task definitions) one deployment wants.
    ///
    /// Mirrors the Docker backend's desired computation with two deliberate
    /// differences: secrets stay *out* of the plain environment (they go to SSM),
    /// and there is no per-replica fan-out — the replica count becomes the
    /// service's `desiredCount`.
    /// Wrap an error the deployment can never recover from by retrying.
    ///
    /// The distinction matters because the two need opposite handling: a
    /// transient failure (a store read, a KMS decrypt) should protect the
    /// deployment's services from GC and be retried next tick, while a
    /// permanent one — an unsupported feature, an unsatisfiable size, a name
    /// ECS will not accept — will fail identically forever. Retrying it just
    /// buries the actionable message in a log until the deploying timeout
    /// replaces it with "timed out after 300 seconds".
    fn permanent<T>(result: Result<T>) -> Result<T> {
        result.map_err(|e| e.context(PermanentDeployError))
    }

    async fn compute_desired_for_deployment(
        &self,
        project: &Project,
        deployment: &Deployment,
    ) -> Result<(
        Vec<(DesiredService, TaskDefinitionSpec, DesiredContainer)>,
        Vec<ForwardedEvent>,
    )> {
        let (container_specs, route_specs) = resolve_runtime_containers(deployment)?;

        // Fail closed on the features v1 does not implement, rather than
        // deploying something that looks fine and is quietly broken.
        Self::permanent(self.reject_unsupported(deployment, &container_specs))?;

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
        // still ride in the merged env, since a container sees one flat
        // environment, but they are stripped from the task definition later and
        // never reach the drift hash in the clear (see `desired_for_spec`).
        let raw_env_vars = self.store.list_deployment_env_vars(deployment.id).await?;
        let resolved =
            resolve_deployment_env_vars(raw_env_vars, self.encryption_provider.as_deref()).await?;

        let mut base_env = resolved.plain_env_vars.clone();
        for (key, value) in &resolved.secret_env_vars {
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
        let mut events = Vec::new();
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
                    &resolved,
                    env_name.as_deref(),
                    &base_image,
                )
                .await?;
            let (service, task_definition, desired_container, event) = entry;
            out.push((service, task_definition, desired_container));
            if let Some(event) = event {
                events.push(event);
            }
        }
        Ok((out, events))
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
        secret_env: &ResolvedDeploymentEnvVars,
        env_name: Option<&str>,
        base_image: &str,
    ) -> Result<(
        DesiredService,
        TaskDefinitionSpec,
        DesiredContainer,
        Option<ForwardedEvent>,
    )> {
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
        // Bookkeeping for operators, not a gate: `task_definition_hash` is what
        // the diff compares. It still covers the FULL env, so two services can
        // be told apart by it — but secret values enter as fingerprints rather
        // than plaintext, because the tag is stamped on the service and
        // propagated to every task, and a plaintext digest would hand every
        // reader of the cluster an oracle for confirming a guessed secret.
        let env_hash = hash_env(&redact_secrets_for_hash(
            &env,
            &secret_env.secret_env_vars,
            &secret_env.secret_fingerprints,
        ));

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
            project_uuid: project.id.to_string(),
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
        let secrets: Vec<SecretRef> = secret_env
            .secret_env_vars
            .keys()
            .map(|key| SecretRef {
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
        for (key, value) in &secret_env.secret_env_vars {
            // A name ECS will not accept is a property of the spec, not of the
            // moment.
            Self::permanent(ssm::validate(key, value))?;
        }

        // Everything build() rejects -- an unsatisfiable Fargate size, an
        // environment past the task-definition limit -- is a property of the
        // spec and will be rejected identically forever.
        let task_def = Self::permanent(task_definition::build(
            &desired_container,
            &secrets,
            &TaskDefinitionConfig {
                resource_prefix: &self.config.resource_prefix,
                cpu_architecture: &self.config.cpu_architecture,
                compatibility: self.config.capacity.compatibility(),
                execution_role_arn: self.config.execution_role_arn.as_deref(),
                repository_credentials_secret_arn: self
                    .config
                    .repository_credentials_secret_arn
                    .as_deref(),
                task_role_arn: self.config.task_role_arn.as_deref(),
                log_group: self.config.log_group.as_deref(),
                region: &self.config.region,
                traefik: self.traefik_render_config(),
                label_namespace: &self.config.label_namespace,
                controller_class: &self.config.controller_class,
            },
        ))?;

        let resource_adjustment = if task_def.size.rounded_up {
            info!(
                project = %project.name,
                container = %spec.name,
                requested_cpu = %desired_container.cpu,
                requested_memory = %desired_container.memory,
                resolved_cpu = %task_def.cpu,
                resolved_memory_mib = %task_def.memory,
                "Rounded the requested resources up to the nearest valid Fargate task size"
            );
            Some(resource_adjustment_event(
                &spec.name,
                &desired_container.cpu,
                &desired_container.memory,
                task_def.size,
            ))
        } else {
            None
        };

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
            shape: self.desired_shape(),
            tags: ServiceTags {
                project: project.name.clone(),
                project_uuid: project.id.to_string(),
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
        Ok((
            desired_service,
            task_def,
            desired_container,
            resource_adjustment,
        ))
    }

    // ── observing the cluster ─────────────────────────────────────────────

    /// Read every Rise-managed service in the cluster, once.
    ///
    /// Discovery is by **tag**, never by parsing names: a name is lossy (long
    /// project names are truncated with a hash suffix) while tags are exact, and
    /// mistaking another controller's service for ours would mean deleting it.
    ///
    /// Called exactly once per tick, and the resulting [`ClusterServices`] is
    /// threaded through the diff, readiness, the termination check and the
    /// orphan sweep. Service reads sustain only 20 requests/second and this is
    /// the only unbounded-by-project read the reconciler makes, so re-listing
    /// per project — let alone per deployment — turns installs with many
    /// projects into a throttling problem: the cost would grow as
    /// `(projects + deployments) x services/page`, against a fixed budget.
    /// One pass costs `1 + services/100` list calls plus `services/10`
    /// describes, no matter how many projects there are.
    async fn list_managed_services(&self) -> Result<ClusterServices> {
        let mut arns: Vec<String> = Vec::new();
        let mut next_token: Option<String> = None;
        loop {
            let mut req = self
                .ecs
                .list_services()
                .cluster(&self.config.cluster)
                .max_results(LIST_SERVICES_PAGE);
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
        let mut unattributable = Vec::new();
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
                // A service being deleted still appears in DescribeServices for a
                // while; treating it as live would make the diff think the
                // desired state is already satisfied, and would make the orphan
                // sweep re-issue a delete that is already in flight.
                if svc
                    .status()
                    .is_some_and(|s| s == "INACTIVE" || s == "DRAINING")
                {
                    continue;
                }
                let Some(parsed) = ServiceTags::parse(&tags, &self.config.label_namespace) else {
                    unattributable.push(ActualService {
                        name: svc.service_name().unwrap_or_default().to_string(),
                        arn: svc.service_arn().unwrap_or_default().to_string(),
                        key: None,
                        task_definition_arn: svc.task_definition().unwrap_or_default().to_string(),
                        task_definition_hash: String::new(),
                        desired_count: svc.desired_count(),
                        running_count: svc.running_count(),
                        deployment_id: None,
                        project: String::new(),
                        deployment_group: String::new(),
                        events: service_narrative(svc),
                        rollout_started_at: primary_rollout_start(svc),
                        shape: observed_shape(svc),
                    });
                    continue;
                };
                services.push(ManagedService {
                    service: ActualService {
                        name: svc.service_name().unwrap_or_default().to_string(),
                        arn: svc.service_arn().unwrap_or_default().to_string(),
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
                        deployment_id: Some(parsed.deployment_id.clone()),
                        project: parsed.project.clone(),
                        deployment_group: parsed.deployment_group.clone(),
                        events: service_narrative(svc),
                        rollout_started_at: primary_rollout_start(svc),
                        shape: observed_shape(svc),
                    },
                    tags: parsed,
                });
            }
        }
        Ok(ClusterServices {
            services,
            unattributable,
        })
    }

    // ── applying ──────────────────────────────────────────────────────────

    /// Apply the diff's actions, isolating failures per action so one bad
    /// deployment cannot stall the others, and re-verifying leadership before
    /// each one (a create can take seconds, exceeding the validity window).
    ///
    /// Returns `service name -> task-definition ARN` for the services pointed at
    /// a new revision here. The health pass runs against the snapshot taken at
    /// the top of the tick, which predates these writes, so this is the only
    /// record of what the affected services are converging on right now.
    async fn apply_actions(
        &self,
        project: &Project,
        desired: &[(DesiredService, TaskDefinitionSpec, DesiredContainer)],
        actual: &[ActualService],
        actions: &[ServiceAction],
        election: &LeaderElection,
    ) -> HashMap<String, String> {
        let by_key: HashMap<&str, &(DesiredService, TaskDefinitionSpec, DesiredContainer)> =
            desired.iter().map(|e| (e.0.key.as_str(), e)).collect();
        // Tagging needs the service's real ARN, and only DescribeServices has it.
        let arn_by_name: HashMap<&str, &str> = actual
            .iter()
            .map(|a| (a.name.as_str(), a.arn.as_str()))
            .collect();
        // A `Delete` retires a service; its secrets in SSM must go with it, and
        // the path prefix is recovered from the service's own tags.
        let actual_by_name: HashMap<&str, &ActualService> =
            actual.iter().map(|a| (a.name.as_str(), a)).collect();

        let mut registered: HashMap<String, String> = HashMap::new();
        for action in actions {
            if !self.confirm_leadership(election).await {
                return registered;
            }
            let result = match action {
                ServiceAction::Create { key, name } => match by_key.get(key.as_str()) {
                    Some(entry) => self
                        .create_service(project, entry, name)
                        .await
                        .map(|arn| registered.insert(name.clone(), arn))
                        .map(|_| ()),
                    None => continue,
                },
                ServiceAction::UpdateTaskDefinition { key, name } => {
                    match by_key.get(key.as_str()) {
                        Some(entry) => self
                            .update_service(
                                project,
                                entry,
                                name,
                                arn_by_name.get(name.as_str()).copied(),
                            )
                            .await
                            .map(|arn| registered.insert(name.clone(), arn))
                            .map(|_| ()),
                        None => continue,
                    }
                }
                ServiceAction::UpdateServiceShape {
                    name,
                    desired_count,
                    ..
                } => self.update_service_shape(name, *desired_count).await,
                ServiceAction::UpdateDesiredCount {
                    name,
                    desired_count,
                    ..
                } => self.scale_service(name, *desired_count).await,
                ServiceAction::Delete { name } => {
                    let deleted = self.delete_service(name).await;
                    // Only once the service is actually gone: while it exists a
                    // task can still restart and needs its parameters. Secret
                    // cleanup is best-effort — a leftover costs nothing, whereas
                    // failing the whole action would strand the service.
                    if deleted.is_ok() {
                        if let Some(a) = actual_by_name.get(name.as_str()) {
                            if let Some(deployment_id) = a.deployment_id.as_deref() {
                                if let Err(e) = self
                                    .delete_secrets_for(
                                        &a.project,
                                        &a.deployment_group,
                                        deployment_id,
                                    )
                                    .await
                                {
                                    warn!(service = %name, "Failed to delete a retired service's secrets: {:?}", e);
                                }
                            }
                        }
                    }
                    deleted
                }
            };
            if let Err(e) = result {
                error!(action = ?action, "ECS action failed; will retry next tick: {:?}", e);
            }
        }
        registered
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
            // The task definition already references this parameter by ARN, so
            // skipping the write would deploy a task that cannot start -- ECS
            // reports only an opaque "unable to pull secrets or registry auth".
            // The two lists are built from the same env split, so a mismatch is
            // a bug in that construction, not a user error; surface it here
            // rather than at task start.
            let Some(value) = by_name.get(secret.name.as_str()) else {
                anyhow::bail!(
                    "Secret {:?} is referenced by the task definition for project {} but has \
                     no value in the resolved environment; refusing to register a task \
                     definition whose secret cannot be read",
                    secret.name,
                    project.name
                );
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
    async fn delete_secrets_for(
        &self,
        project: &str,
        deployment_group: &str,
        deployment_id: &str,
    ) -> Result<()> {
        let prefix = ssm::deployment_path_prefix(
            &self.config.ssm_parameter_prefix,
            project,
            deployment_group,
            deployment_id,
        );
        // Paginated: GetParametersByPath returns 10 per page by default, so a
        // deployment with more secrets than that would leave the rest behind.
        let mut names: Vec<String> = Vec::new();
        let mut next: Option<String> = None;
        loop {
            let mut req = self
                .ssm
                .get_parameters_by_path()
                .path(&prefix)
                .recursive(true)
                .max_results(10);
            if let Some(token) = next.as_deref() {
                req = req.next_token(token);
            }
            match req.send().await {
                Ok(out) => {
                    names.extend(
                        out.parameters()
                            .iter()
                            .filter_map(|p| p.name().map(str::to_string)),
                    );
                    next = out.next_token().map(str::to_string);
                    if next.is_none() {
                        break;
                    }
                }
                Err(e) => {
                    debug!(path = %prefix, "Could not list SSM parameters: {}", aws_error_detail(&e));
                    return Ok(());
                }
            }
        }
        if names.is_empty() {
            return Ok(());
        }

        // DeleteParameters takes at most 10 names; passing more fails the whole
        // request, so a deployment with 11 secrets would clean up none of them.
        for chunk in names.chunks(10) {
            let mut req = self.ssm.delete_parameters();
            for name in chunk {
                req = req.names(name);
            }
            if let Err(e) = req.send().await {
                warn!(path = %prefix, "Failed to delete SSM parameters: {}", aws_error_detail(&e));
            }
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
        if let Some(arn) = &container.repository_credentials_secret_arn {
            cd = cd.repository_credentials(
                ecs_types::RepositoryCredentials::builder()
                    .credentials_parameter(arn)
                    .build()
                    .map_err(|e| anyhow::anyhow!("invalid repository credentials: {e}"))?,
            );
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
            // Not configurable: readiness keys Traefik's `serverStatus` on the
            // task's own ENI address, which only `awsvpc` provides.
            .network_mode(ecs_types::NetworkMode::Awsvpc)
            .requires_compatibilities(compatibility_type(self.config.capacity.compatibility()))
            .cpu(&spec.cpu)
            .memory(&spec.memory)
            .runtime_platform(
                ecs_types::RuntimePlatform::builder()
                    .cpu_architecture(match spec.cpu_architecture.as_str() {
                        "X86_64" => ecs_types::CpuArchitecture::X8664,
                        "ARM64" => ecs_types::CpuArchitecture::Arm64,
                        // `CpuArchitecture::from` would hand back an `Unknown`
                        // variant here and ship the bad string to AWS. The
                        // builder canonicalises, so reaching this arm means the
                        // two have drifted — say so instead of guessing.
                        other => anyhow::bail!(
                            "task definition for family {:?} carries CPU architecture {other:?}, \
                             which is not one of {}",
                            spec.family,
                            task_definition::FARGATE_CPU_ARCHITECTURES.join(" or ")
                        ),
                    })
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

    /// The service-level shape this install wants: where the tasks' ENIs live
    /// and what capacity places them.
    fn desired_shape(&self) -> crate::service::DesiredShape {
        crate::service::DesiredShape {
            subnets: self.config.subnets.iter().cloned().collect(),
            security_groups: self.config.security_groups.iter().cloned().collect(),
            assign_public_ip: self.config.assign_public_ip,
            capacity: self.config.capacity.clone(),
        }
    }

    fn network_configuration(&self) -> Result<aws_sdk_ecs::types::NetworkConfiguration> {
        use aws_sdk_ecs::types as ecs_types;
        let mut vpc = ecs_types::AwsVpcConfiguration::builder();
        // Omitted entirely where the capacity does not support it: ECS rejects
        // the field on EC2 rather than ignoring it. Settings validation already
        // refuses `assign_public_ip: true` there, so nothing is silently
        // dropped -- this only keeps the field off the wire.
        if self
            .config
            .capacity
            .compatibility()
            .supports_assign_public_ip()
        {
            vpc = vpc.assign_public_ip(if self.config.assign_public_ip {
                ecs_types::AssignPublicIp::Enabled
            } else {
                ecs_types::AssignPublicIp::Disabled
            });
        }
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
    ) -> Result<String> {
        let task_definition_arn = self.prepare_task_definition(project, entry).await?;
        let (desired, _, _) = entry;

        let mut req = self
            .ecs
            .create_service()
            .cluster(&self.config.cluster)
            .service_name(name)
            .task_definition(&task_definition_arn)
            .desired_count(desired.desired_count)
            .network_configuration(self.network_configuration()?)
            // Tags must propagate to the tasks: Traefik's ECS provider and any
            // operator debugging both look at task tags, not just service tags.
            .propagate_tags(aws_sdk_ecs::types::PropagateTags::Service)
            .enable_ecs_managed_tags(true);
        req = apply_capacity(req, &self.config.capacity)?;
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
        Ok(task_definition_arn)
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
        service_arn: Option<&str>,
    ) -> Result<String> {
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

        // Re-tag so the next tick can see the new content hash without reading
        // the DB. Best-effort: the durable convergence marker is the hash
        // `reconcile_health` persists to `controller_metadata`, so a tag failure
        // here only loses a fast path, it no longer drives an unbounded re-roll.
        if let Some(service_arn) = service_arn {
            let mut req = self.ecs.tag_resource().resource_arn(service_arn);
            for tag in self.service_tag_list(desired) {
                req = req.tags(tag);
            }
            if let Err(e) = req.send().await {
                warn!(
                    project = %project.name,
                    service = %name,
                    "Failed to re-tag after update; convergence is tracked in the DB: {}",
                    aws_error_detail(&e)
                );
            }
        } else {
            warn!(
                service = %name,
                "No ARN observed to re-tag after update; convergence is tracked in the DB"
            );
        }

        info!(project = %project.name, service = %name, "Updated ECS service task definition");
        Ok(task_definition_arn)
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

    /// Apply service-level configuration to a service whose task definition is
    /// already current.
    ///
    /// Deliberately sends no task definition: nothing about the task changed, and
    /// registering a revision to carry a network change would burn the 1 req/s
    /// `RegisterTaskDefinition` budget for nothing. `desiredCount` rides along
    /// because it is free here and the scale-only path omits the network
    /// configuration, so a service needing both would otherwise take two ticks.
    ///
    /// Capacity is **not** applied here -- see [`crate::service::capacity_drift`].
    async fn update_service_shape(&self, name: &str, desired_count: i32) -> Result<()> {
        self.ecs
            .update_service()
            .cluster(&self.config.cluster)
            .service(name)
            .desired_count(desired_count)
            .network_configuration(self.network_configuration()?)
            .send()
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "UpdateService (configuration) {:?} failed: {}",
                    name,
                    aws_error_detail(&e)
                )
            })?;
        info!(
            service = %name,
            "Applied the configured network placement to ECS service"
        );
        Ok(())
    }

    /// Retire a service: ask ECS to scale it to zero, then delete it.
    ///
    /// `DeleteService` refuses a service that still has running tasks unless
    /// forced, and this does not wait for the scale-to-zero to take effect, so
    /// the force is what actually removes the service. Tasks still get ECS's
    /// normal stop sequence (SIGTERM, then `stopTimeout`), but this call does
    /// **not** drain connections: Traefik discovers ECS tasks by polling, so a
    /// task can stay in its routing table for up to one poll interval after it
    /// begins stopping.
    ///
    /// What protects live requests at a cutover is the overlap (D12) -- the
    /// outgoing deployment keeps serving alongside the incoming one until it is
    /// retired -- not this function. Making the retirement itself drain-clean
    /// would mean scaling to zero, waiting out Traefik's discovery interval,
    /// and deleting on a later tick; see the tracked follow-up.
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
        services: &[ActualService],
        registered: &HashMap<String, String>,
        desired_td_hash: Option<&str>,
        server_status_cache: &mut HashMap<String, Option<HashMap<String, bool>>>,
    ) -> Result<()> {
        if !matches!(
            deployment.status,
            DeploymentStatus::Deploying | DeploymentStatus::Healthy | DeploymentStatus::Unhealthy
        ) {
            return Ok(());
        }

        let (container_specs, route_specs) = resolve_runtime_containers(deployment)?;
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
        // Only meaningful when `all_ready` is false: every container that is not
        // ready is mid-roll with its predecessor still serving.
        let mut rolling_but_serving = true;
        let now = chrono::Utc::now();
        let mut reasons: Vec<String> = Vec::new();

        // One entry per task actually observed, across every spec.
        let mut observed: Vec<ContainerObservation> = Vec::new();
        // Whether every spec's task list came back cleanly. A spec whose
        // `DescribeTasks` failed contributes no tasks, and an empty set is
        // indistinguishable from "every task stopped" — which the derivation
        // would record as a termination per replica. So a tick that could not
        // see everything records nothing at all.
        let mut observation_complete = true;

        for spec in &container_specs {
            let key = spec_key(
                &project.name,
                &deployment.deployment_group,
                &deployment.deployment_id,
                &spec.name,
            );
            let Some(actual) = by_key.get(key.as_str()) else {
                all_ready = false;
                rolling_but_serving = false;
                reasons.push(format!("service for '{}' not found", spec.name));
                // No service means nothing to observe *and* no evidence its
                // tasks stopped, so the tick cannot claim a complete picture.
                observation_complete = false;
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
            let server_status = match self.traefik_api.as_ref() {
                Some(client) => {
                    client
                        .aggregated_server_status(&service_names, server_status_cache)
                        .await
                }
                None => None,
            };

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

            // The revision this deployment is *supposed* to be running. The
            // service snapshot was taken before this tick applied its actions,
            // so a roll started this tick is only visible in `registered`;
            // reading the snapshot alone would judge the tick that starts a
            // rollout by the revision it is replacing.
            let target_arn = registered
                .get(actual.name.as_str())
                .map(String::as_str)
                .unwrap_or(actual.task_definition_arn.as_str());

            // A service mid-roll owns both revisions' tasks. Readiness is about
            // the incoming one; the outgoing one only answers whether anything
            // is still serving.
            let (mut current, outgoing): (Vec<TaskView>, Vec<TaskView>) = self
                .describe_service_tasks(&actual.name)
                .await?
                .into_iter()
                .partition(|t| t.task_definition_arn == target_arn);
            // ListTasks returns no defined order, so indexing it straight would
            // let two ticks give the same replica slot to different tasks and
            // report a replica flapping that never moved. The task id is stable
            // for a task's whole life, which is all a slot needs.
            current.sort_by(|a, b| a.name.cmp(&b.name));

            // Both revisions' tasks are this deployment's: during a roll the
            // outgoing ones are still running and still serving, so leaving
            // them out would show a deployment as half its actual size and
            // then report them terminating once the roll completed anyway.
            observed.extend(
                current
                    .iter()
                    .chain(outgoing.iter())
                    .map(|t| observe_task(t, &spec.name)),
            );
            let api_available = self.traefik_api.is_some() && server_status.is_some();

            let verdict_for = |task: &TaskView, label: &str| -> ReadyVerdict {
                match spec.port {
                    Some(port) => {
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
                            task.running,
                            api_available,
                            server_up.flatten(),
                        )
                    }
                    // A port-less worker has no router and no probe: running
                    // is the only signal available.
                    None if task.running => ReadyVerdict::Ready,
                    None => ReadyVerdict::NotReady(format!("worker '{label}' not running")),
                }
            };

            // The count Rise *wants*, not the service's observed `desiredCount`.
            // An out-of-band console scale-up raises the observed count, and
            // judging readiness against tasks that legitimately do not exist yet
            // would flap the deployment Unhealthy for a tick before the next
            // reconcile corrects the drift. `clamp_replicas` already floors at 1.
            let expected = clamp_replicas(spec.replicas) as usize;
            let mut spec_ready = true;
            for idx in 0..expected {
                let label = if expected == 1 {
                    spec.name.clone()
                } else {
                    format!("{}[{}]", spec.name, idx)
                };
                let task = current.get(idx);
                let verdict = match task {
                    None => ReadyVerdict::NotReady(format!("'{label}' task not running")),
                    Some(task) => verdict_for(task, &label),
                };

                if let ReadyVerdict::NotReady(reason) = verdict {
                    all_ready = false;
                    spec_ready = false;
                    reasons.push(format!("'{label}': {reason}"));
                }
                // Always recorded, even when the verdict short-circuits, so the
                // Pods tab shows the full picture rather than a truncated one.
            }

            // Whether the revision being replaced is still answering for this
            // container. Only consulted when the incoming one is not ready yet.
            if !spec_ready {
                let still_serving = outgoing
                    .iter()
                    .any(|t| matches!(verdict_for(t, &spec.name), ReadyVerdict::Ready));
                rolling_but_serving &= service::hold_status_during_rollout(
                    still_serving,
                    registered.contains_key(actual.name.as_str()),
                    actual.rollout_started_at,
                    now,
                    chrono::Duration::minutes(DEPLOYING_TIMEOUT_MINUTES),
                );
            }
        }

        // Record the task-definition hash this service has converged to, so a
        // failed `TagResource` cannot make the next tick re-roll it. When this
        // tick registered the service (Create/UpdateTaskDefinition succeeded),
        // that is the hash it now desires; otherwise carry forward what a prior
        // tick recorded, rather than dropping it.
        let just_rolled = container_specs.iter().any(|spec| {
            let name = service::service_name(
                &self.config.resource_prefix,
                &project.name,
                &deployment.deployment_group,
                &deployment.deployment_id,
                &spec.name,
            );
            registered.contains_key(&name)
        });
        let converged_hash = if just_rolled {
            desired_td_hash.map(str::to_string)
        } else {
            persisted_td_hash(&deployment.controller_metadata)
        };

        // Record what was seen, but only from a tick that saw everything. An
        // incomplete pass would look like tasks having stopped, and the log
        // would gain terminations that never happened.
        if observation_complete {
            if let Err(e) = self.record_observations(deployment, &observed).await {
                warn!(
                    deployment = %deployment.deployment_id,
                    "Failed to record container observations: {:?}", e
                );
            }
        } else {
            debug!(
                deployment = %deployment.deployment_id,
                "Skipping container observations: this tick could not see every service"
            );
        }

        // Forward what the services said about themselves this tick. Every
        // message ECS has in its window is offered on every tick; the store
        // deduplicates on ECS's own event id, so only the new ones land.
        // Failing here must not fail the health pass — the deployment's status
        // is the job, its narrative is a record of the job.
        let narrated: Vec<ForwardedEvent> = container_specs
            .iter()
            .filter_map(|spec| {
                let key = spec_key(
                    &project.name,
                    &deployment.deployment_group,
                    &deployment.deployment_id,
                    &spec.name,
                );
                by_key.get(key.as_str())
            })
            .flat_map(|svc| svc.events.iter())
            .filter_map(forwarded_from_ecs)
            .collect();
        if !narrated.is_empty() {
            match self
                .store
                .forward_backend_events(deployment.id, EventSource::Ecs, &narrated)
                .await
            {
                Ok(n) if n > 0 => {
                    debug!(
                        deployment = %deployment.deployment_id,
                        "Forwarded {n} new ECS service event(s)"
                    );
                }
                Ok(_) => {}
                Err(e) => warn!(
                    deployment = %deployment.deployment_id,
                    "Failed to forward ECS service events: {:?}", e
                ),
            }
        }

        // `controller_metadata` carries exactly one thing: the task-definition
        // hash the service has converged to. It is the authoritative drift
        // marker, so it is written even when there is nothing else to record.
        let metadata = with_persisted_td_hash(serde_json::json!({}), converged_hash.as_deref());
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
                rise_backend_core::lifecycle::handle_deployment_became_healthy(
                    self.store.as_ref(),
                    Some(self),
                    project,
                    deployment,
                )
                .await?;
            }
            DeploymentStatus::Healthy if !all_ready && rolling_but_serving => {
                debug!(
                    deployment = %deployment.deployment_id,
                    "New task-definition revision still rolling out; the outgoing one is \
                     still serving, so the deployment stays Healthy"
                );
            }
            DeploymentStatus::Healthy
                if !all_ready
                    && self
                        .store
                        .mark_deployment_unhealthy(deployment.id, unhealthy_reason)
                        .await?
                        .is_some() =>
            {
                self.store
                    .update_project_calculated_status(project.id)
                    .await?;
            }
            DeploymentStatus::Healthy if !all_ready => {
                // The guarded write above was a no-op: some other request
                // already moved this deployment on. Benign race, nothing
                // left to do this tick.
            }
            DeploymentStatus::Unhealthy
                if all_ready
                    && self
                        .store
                        .mark_deployment_healthy(deployment.id)
                        .await?
                        .is_some() =>
            {
                self.store
                    .update_project_calculated_status(project.id)
                    .await?;
            }
            DeploymentStatus::Unhealthy if all_ready => {}
            _ => {}
        }
        Ok(())
    }

    /// Describe a service's tasks, projecting each onto the backend-agnostic
    /// [`InspectedContainer`] the shared readiness verdict consumes and
    /// tagging it with the revision it runs.
    ///
    /// Every task is returned, both revisions' during a roll: the caller needs
    /// the incoming revision to judge readiness and the outgoing one to know
    /// whether anything is still serving.
    ///
    /// Both desired states are listed. `ListTasks` defaults to
    /// `desiredStatus=RUNNING`, but the moment ECS starts draining the outgoing
    /// revision it flips those tasks to `desiredStatus=STOPPED` — while they are
    /// still `lastStatus=RUNNING` and still serving every request until the
    /// incoming revision is up. Listing only RUNNING-desired tasks would drop
    /// them, so the caller would conclude "nothing is serving" during the very
    /// window the drain protects, and flap a healthy in-place roll to Unhealthy.
    ///
    /// A transient `ListTasks`/`DescribeTasks` failure makes the whole pass
    /// indeterminate (`Err`) rather than silently returning whatever partial
    /// data was collected: a partial or empty task list is exactly as
    /// capable of flapping a healthy deployment to Unhealthy for a tick as a
    /// wrongly-empty one, so the caller must skip this tick's readiness
    /// verdict entirely rather than act on it. See `tasks_or_indeterminate`.
    async fn describe_service_tasks(&self, service_name: &str) -> Result<Vec<TaskView>> {
        use aws_sdk_ecs::types::DesiredStatus;

        let mut arns: Vec<String> = Vec::new();
        let mut had_error = false;
        for desired_status in [DesiredStatus::Running, DesiredStatus::Stopped] {
            match self
                .ecs
                .list_tasks()
                .cluster(&self.config.cluster)
                .service_name(service_name)
                .desired_status(desired_status.clone())
                .send()
                .await
            {
                Ok(out) => arns.extend(out.task_arns().iter().cloned()),
                Err(e) => {
                    had_error = true;
                    warn!(service = %service_name, ?desired_status, "ListTasks failed: {}", aws_error_detail(&e));
                }
            }
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
                    had_error = true;
                    warn!(service = %service_name, "DescribeTasks failed: {}", aws_error_detail(&e));
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
                // The first container is the app's: Rise runs one container per
                // task definition, so its exit code is the task's.
                let container = task.containers().first();
                views.push(TaskView {
                    name: name.clone(),
                    task_definition_arn: task.task_definition_arn().unwrap_or_default().to_string(),
                    running,
                    ip,
                    last_status,
                    started_at: task.started_at().and_then(aws_time),
                    stopped_at: task.stopped_at().and_then(aws_time),
                    stopped_reason: task
                        .stopped_reason()
                        .filter(|r| !r.is_empty())
                        .map(str::to_string),
                    health_status: task.health_status().map(|h| h.as_str().to_lowercase()),
                    exit_code: container.and_then(|c| c.exit_code()).map(|c| c as i64),
                    image: container.and_then(|c| c.image()).map(str::to_string),
                });
            }
        }
        tasks_or_indeterminate(views, had_error)
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

#[async_trait::async_trait]
impl rise_backend_core::lifecycle::SupersededHook for EcsReconciler {
    /// The retired deployment's SSM secrets go with it, so a later rollback
    /// re-creates them from the database rather than reading a stale value.
    async fn on_superseded(&self, project: &Project, superseded: &Deployment) -> Result<()> {
        self.delete_secrets_for(
            &project.name,
            &superseded.deployment_group,
            &superseded.deployment_id,
        )
        .await
    }
}

/// One observed ECS task, reduced to what the readiness verdict needs.
struct TaskView {
    name: String,
    /// ARN of the task definition this task runs. During a roll a service owns
    /// tasks of two revisions at once, and they must be told apart.
    task_definition_arn: String,
    running: bool,
    /// The task's ENI private IP — the key Traefik's `serverStatus` uses
    /// (`http://{ip}:{port}`).
    ip: Option<String>,

    // Everything below is observation rather than readiness: it says what
    // happened to the task, which the deployment's history needs and the
    // ready/not-ready verdict does not.
    /// ECS's own lifecycle string, uppercase (`RUNNING`, `PENDING`, `STOPPED`).
    last_status: String,
    started_at: Option<chrono::DateTime<chrono::Utc>>,
    stopped_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Why ECS stopped it, in ECS's words. The only explanation that exists for
    /// a task that went away.
    stopped_reason: Option<String>,
    health_status: Option<String>,
    exit_code: Option<i64>,
    image: Option<String>,
}

/// AWS timestamps are seconds-and-nanos; the observation stores `chrono`.
fn aws_time(t: &aws_sdk_ecs::primitives::DateTime) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::from_timestamp(t.secs(), t.subsec_nanos())
}

/// Collapse a `describe_service_tasks` pass into a single readiness-relevant
/// result: any `ListTasks`/`DescribeTasks` error along the way makes the
/// whole pass indeterminate, not just the tasks it would have contributed.
/// Deliberately does not try to salvage a partial `views` list — a partial
/// task set is just as capable of causing a false-Unhealthy flap as an
/// empty one, so "some data, but incomplete" and "no data" are treated the
/// same: skip this tick's readiness verdict rather than act on it.
fn tasks_or_indeterminate(views: Vec<TaskView>, had_error: bool) -> Result<Vec<TaskView>> {
    if had_error {
        anyhow::bail!(
            "ECS API error(s) while describing tasks; treating readiness as indeterminate for this tick"
        );
    }
    Ok(views)
}

/// When ECS started rolling out the service's current (PRIMARY) deployment.
///
/// ECS reports one entry per in-flight revision; PRIMARY is the one it is
/// converging on. Its `createdAt` is the only signal available for how long a
/// roll has been running, which is what bounds the status hold in
/// [`service::hold_status_during_rollout`].
fn primary_rollout_start(
    svc: &aws_sdk_ecs::types::Service,
) -> Option<chrono::DateTime<chrono::Utc>> {
    svc.deployments()
        .iter()
        .find(|d| d.status() == Some("PRIMARY"))
        .and_then(|d| d.created_at())
        .and_then(|t| chrono::DateTime::from_timestamp(t.secs(), 0))
}

/// Whether a configured controller class matches an Organization's.
///
/// An empty configured class matches everything (a legacy or unconfigured
/// install); otherwise the Organization must name this class exactly. An
/// Organization with no class set belongs to no controller.
/// Where a project stands, from the two class values alone.
///
/// The distinction that matters is between an Organization naming a *different*
/// class and one naming *none*. `Migrated` retires live services on the
/// strength of a replacement controller already serving the project; an
/// Organization with no class has no such replacement, so retiring there would
/// be pure downtime — and a class can be absent for reasons as ordinary as a
/// two-step spec edit that clears it before setting it.
fn ownership_from_class(configured: &str, org_class: Option<&str>) -> Ownership {
    if controller_class_matches(configured, org_class) {
        Ownership::Ours
    } else if org_class.is_none() {
        Ownership::Unlinked
    } else {
        Ownership::Migrated
    }
}

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
    use super::{
        persisted_td_hash, resource_adjustment_event, tasks_or_indeterminate,
        with_persisted_td_hash, TaskView,
    };

    #[test]
    fn resource_adjustment_event_preserves_request_and_effective_size() {
        let event = resource_adjustment_event(
            "app",
            "128m-500m",
            "192Mi-512Mi",
            crate::sizing::TaskSize {
                cpu_units: 512,
                memory_mib: 1024,
                rounded_up: true,
            },
        );

        assert_eq!(
            event.severity,
            rise_backend_core::events::EventSeverity::Warning
        );
        assert_eq!(event.subject.as_deref(), Some("app"));
        assert_eq!(event.attributes["type"], "resource_adjusted");
        assert_eq!(event.attributes["requested_cpu"], "128m-500m");
        assert_eq!(event.attributes["requested_memory"], "192Mi-512Mi");
        assert_eq!(event.attributes["resolved_cpu_units"], 512);
        assert_eq!(event.attributes["resolved_memory_mib"], 1024);
    }

    fn task_view(name: &str) -> TaskView {
        TaskView {
            name: name.to_string(),
            task_definition_arn: "arn:aws:ecs:task-def/foo:1".to_string(),
            running: true,
            ip: None,
            last_status: "RUNNING".to_string(),
            started_at: None,
            stopped_at: None,
            stopped_reason: None,
            health_status: None,
            exit_code: None,
            image: None,
        }
    }

    #[test]
    fn tasks_or_indeterminate_passes_through_a_clean_pass() {
        assert!(tasks_or_indeterminate(Vec::new(), false)
            .unwrap()
            .is_empty());
        let views = vec![task_view("a"), task_view("b")];
        let result = tasks_or_indeterminate(views, false).unwrap();
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn tasks_or_indeterminate_errors_even_with_partial_data() {
        // A partial task list is exactly as capable of causing a false flap
        // as an empty one, so any API error makes the whole pass
        // indeterminate rather than trying to salvage what was collected.
        let views = vec![task_view("a")];
        assert!(tasks_or_indeterminate(views, true).is_err());
        assert!(tasks_or_indeterminate(Vec::new(), true).is_err());
    }

    #[test]
    fn converged_hash_round_trips_through_controller_metadata() {
        // The reconciler writes only the hash today, but the stamp must stay
        // additive: whatever else comes to share this column survives it.
        let base = serde_json::json!({ "other": { "kept": true } });
        assert_eq!(persisted_td_hash(&base), None);

        let stamped = with_persisted_td_hash(base.clone(), Some("abc123"));
        assert_eq!(persisted_td_hash(&stamped).as_deref(), Some("abc123"));
        assert_eq!(stamped["other"]["kept"], true);
    }

    #[test]
    fn stamping_none_leaves_the_metadata_unchanged() {
        let base = serde_json::json!({ "other": { "kept": false } });
        assert_eq!(with_persisted_td_hash(base.clone(), None), base);
    }

    #[test]
    fn a_later_stamp_overwrites_an_earlier_one() {
        let once = with_persisted_td_hash(serde_json::json!({}), Some("old"));
        let twice = with_persisted_td_hash(once, Some("new"));
        assert_eq!(persisted_td_hash(&twice).as_deref(), Some("new"));
    }

    /// A cluster entry as `list_managed_services` would build it: the tags are
    /// the identity, the `ActualService` is what the diff sees.
    fn managed(project: &str, project_uuid: &str, deployment_id: &str) -> super::ManagedService {
        super::ManagedService {
            service: crate::service::ActualService {
                events: Vec::new(),
                name: format!("rise-{project}-{deployment_id}-web"),
                arn: format!("arn:aws:ecs:eu-west-1:1:service/c/{project}-{deployment_id}"),
                key: None,
                task_definition_arn: String::new(),
                task_definition_hash: String::new(),
                desired_count: 1,
                running_count: 1,
                deployment_id: Some(deployment_id.to_string()),
                project: project.to_string(),
                deployment_group: "main".to_string(),
                rollout_started_at: None,
                shape: None,
            },
            tags: crate::tags::ServiceTags {
                project: project.to_string(),
                project_uuid: project_uuid.to_string(),
                deployment_group: "main".to_string(),
                deployment_id: deployment_id.to_string(),
                deployment_uuid: uuid::Uuid::nil().to_string(),
                container: "web".to_string(),
                environment: None,
                env_hash: String::new(),
                image: "img".to_string(),
                route_hash: String::new(),
            },
        }
    }

    /// Minimal `Project` fixture for tests exercising project-scoped lookups.
    fn project_fixture(name: &str, uuid: &str) -> Project {
        let now = chrono::Utc::now();
        Project {
            id: Uuid::parse_str(uuid).unwrap(),
            name: name.to_string(),
            status: rise_backend_core::models::ProjectStatus::Running,
            access_class: "public".to_string(),
            owner_user_id: None,
            owner_team_id: None,
            finalizers: Vec::new(),
            source_url: None,
            template_id: None,
            template_image: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// The tick reads the cluster once and projects per-project slices out of
    /// it. If the projection matched loosely -- on the service name, say --
    /// `rise-app` and `rise-app-staging` would leak into each other's diffs and
    /// each would try to delete the other's services.
    const APP_UUID: &str = "11111111-1111-1111-1111-111111111111";
    const STAGING_UUID: &str = "22222222-2222-2222-2222-222222222222";

    #[test]
    fn a_project_slice_holds_only_that_project_s_services() {
        let cluster = super::ClusterServices {
            services: vec![
                managed("app", APP_UUID, "d-1"),
                managed("app-staging", STAGING_UUID, "d-2"),
                managed("app", APP_UUID, "d-3"),
            ],
            unattributable: Vec::new(),
        };

        let app = project_fixture("app", APP_UUID);
        let slice = cluster.for_project(&app);
        assert_eq!(slice.len(), 2);
        assert!(
            slice
                .iter()
                .all(|s| s.deployment_id.as_deref() != Some("d-2")),
            "a different project's service must not appear in this project's slice"
        );
        assert!(cluster
            .for_project(&project_fixture(
                "nope",
                "33333333-3333-3333-3333-333333333333"
            ))
            .is_empty());
    }

    /// After a rename, the project's ROW keeps its UUID -- only `name`
    /// changes -- and the tag stamped on already-running services is the
    /// UUID, not the name. The slice must still find them under the new name;
    /// this is the specific bug in the projection matching services on
    /// `project` (the mutable name) instead.
    #[test]
    fn a_project_slice_survives_a_rename() {
        let cluster = super::ClusterServices {
            services: vec![managed("old-name", APP_UUID, "d-1")],
            unattributable: Vec::new(),
        };

        let renamed = project_fixture("new-name", APP_UUID);
        let slice = cluster.for_project(&renamed);
        assert_eq!(
            slice.len(),
            1,
            "a service tagged under the pre-rename name must still be found by UUID"
        );
    }

    /// A service created before the `project-uuid` tag existed has no such
    /// tag; the projection must still find it by the name it does carry, or
    /// every pre-upgrade service would look orphaned on the first tick after
    /// upgrade.
    #[test]
    fn a_project_slice_falls_back_to_name_for_untagged_legacy_services() {
        let mut legacy = managed("app", APP_UUID, "d-1");
        legacy.tags.project_uuid = String::new();
        let cluster = super::ClusterServices {
            services: vec![legacy],
            unattributable: Vec::new(),
        };

        let app = project_fixture("app", APP_UUID);
        assert_eq!(cluster.for_project(&app).len(), 1);
    }

    /// The three-way split exists because the two non-owning answers want
    /// opposite handling, and getting `None` wrong is the expensive direction:
    /// it deletes live services with nothing standing by to replace them.
    #[test]
    fn an_organization_with_no_class_is_unlinked_not_migrated() {
        use super::Ownership;

        assert_eq!(
            super::ownership_from_class("prod", Some("prod")),
            Ownership::Ours
        );
        assert_eq!(
            super::ownership_from_class("prod", Some("staging")),
            Ownership::Migrated,
            "an affirmatively different class means someone else serves it now"
        );
        assert_eq!(
            super::ownership_from_class("prod", None),
            Ownership::Unlinked,
            "no class names no controller -- retiring here is downtime, not a handover"
        );
        // An unconfigured install owns everything, including the classless.
        assert_eq!(super::ownership_from_class("", None), Ownership::Ours);
        assert_eq!(
            super::ownership_from_class("", Some("staging")),
            Ownership::Ours
        );
    }

    /// A project that moved to another controller class keeps running under our
    /// tags: the new controller sees only its own class, so nothing there will
    /// ever collect ours. Until they go, an unconstrained proxy routes the
    /// project to two independent deployments at once -- so the live ones are
    /// precisely what must retire, and the deployment status must not veto it.
    #[test]
    fn a_migrated_project_retires_even_while_its_deployment_is_healthy() {
        use rise_backend_core::models::DeploymentStatus;

        for status in [
            Some(&DeploymentStatus::Healthy),
            Some(&DeploymentStatus::Unhealthy),
            Some(&DeploymentStatus::Deploying),
            None,
        ] {
            assert!(
                super::retirement_reason(true, status).is_some(),
                "a migrated project must retire whatever its deployment status"
            );
        }

        // And the reason says which rule fired, because the two are diagnosed
        // very differently by whoever reads the log.
        let reason = super::retirement_reason(true, Some(&DeploymentStatus::Healthy))
            .expect("migrated projects retire");
        assert!(reason.contains("controller class"), "reason was {reason:?}");
    }

    /// The same call on a project that is still ours must defer entirely to the
    /// orphan rule -- otherwise every tick would delete every live service.
    #[test]
    fn an_owned_project_retires_only_on_the_orphan_rule() {
        use rise_backend_core::models::DeploymentStatus;

        assert!(super::retirement_reason(false, Some(&DeploymentStatus::Healthy)).is_none());
        assert!(super::retirement_reason(false, Some(&DeploymentStatus::Deploying)).is_none());
        assert!(super::retirement_reason(false, Some(&DeploymentStatus::Stopped)).is_some());
        assert!(
            super::retirement_reason(false, None).is_some(),
            "no row at all is the deleted-project case"
        );
    }

    /// The orphan rule decides whether to delete something that is running.
    /// A wrong "yes" destroys a live workload, so both directions are pinned.
    #[test]
    fn only_finished_or_unknown_deployments_are_collected() {
        use rise_backend_core::models::DeploymentStatus;

        // No row at all: a deleted project, or a database restored past it.
        assert!(super::is_orphaned(None));

        for finished in [
            DeploymentStatus::Stopped,
            DeploymentStatus::Failed,
            DeploymentStatus::Superseded,
            DeploymentStatus::Cancelled,
            DeploymentStatus::Expired,
        ] {
            assert!(
                super::is_orphaned(Some(&finished)),
                "{finished:?} is finished; nothing will recreate its services"
            );
        }

        // Live, or on its way there. Deleting these would take down a running
        // deployment the reconciler is mid-way through.
        for live in [
            DeploymentStatus::Pending,
            DeploymentStatus::Pushed,
            DeploymentStatus::Deploying,
            DeploymentStatus::Healthy,
            DeploymentStatus::Terminating,
        ] {
            assert!(
                !super::is_orphaned(Some(&live)),
                "{live:?} still belongs to a reconcile pass"
            );
        }
    }

    /// Fail-fast must reach a deployment on its way up and stop at one that is
    /// already serving. Failing a Healthy deployment marks it terminal, which
    /// drops it from the desired set and deletes a running app's services in
    /// the same tick.
    #[test]
    fn only_a_deployment_still_coming_up_is_failed_by_a_permanent_rejection() {
        use rise_backend_core::models::DeploymentStatus;

        for coming_up in [DeploymentStatus::Pushed, DeploymentStatus::Deploying] {
            assert!(
                super::rejection_fails_deployment(&coming_up),
                "{coming_up:?} has never served traffic; the user needs the message"
            );
        }

        for live in [
            DeploymentStatus::Healthy,
            DeploymentStatus::Unhealthy,
            DeploymentStatus::Terminating,
        ] {
            assert!(
                !super::rejection_fails_deployment(&live),
                "{live:?} must not be failed -- its services would be deleted under it"
            );
        }
    }

    /// The rejection text is what `rise deploy` shows the user. The marker in
    /// the chain has an empty `Display`, so formatting the error directly
    /// produces a message that starts with a stray ": ".
    #[test]
    fn the_rejection_message_has_no_empty_leading_chain_link() {
        fn rejects() -> Result<()> {
            anyhow::bail!("multi-container deployments are not supported on ECS")
        }
        let err = EcsReconciler::permanent(rejects()).expect_err("must fail");

        assert_eq!(
            super::rejection_message(&err),
            "multi-container deployments are not supported on ECS"
        );
        assert!(
            format!("{err:#}").starts_with(": "),
            "if anyhow stops emitting the empty link, this helper can go away"
        );
    }

    /// The classification is the whole mechanism: a permanent error must be
    /// recognisable as one after travelling through `?`, and must keep the text
    /// that tells the user what to change. If either half breaks, the deployment
    /// silently reverts to failing with "timed out after 300 seconds".
    #[test]
    fn a_permanent_error_survives_the_error_chain_with_its_message() {
        fn rejects() -> Result<()> {
            anyhow::bail!("multi-container deployments are not supported on ECS")
        }
        fn wraps() -> Result<()> {
            EcsReconciler::permanent(rejects())?;
            Ok(())
        }

        let err = wraps().expect_err("must fail");
        assert!(
            err.downcast_ref::<PermanentDeployError>().is_some(),
            "the marker did not survive `?`"
        );
        assert!(
            format!("{:#}", err).contains("multi-container deployments are not supported"),
            "the actionable message was lost: {err:#}"
        );
    }

    /// A store read that failed once should be retried, not turned into a
    /// permanent failure the user has to redeploy past.
    #[test]
    fn an_untagged_error_is_not_treated_as_permanent() {
        let err: anyhow::Error = anyhow::anyhow!("connection reset by peer");
        assert!(err.downcast_ref::<PermanentDeployError>().is_none());
    }

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
