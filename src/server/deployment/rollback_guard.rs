//! The Rollback Guard (ADR-0007, Phase 2): a leader-elected controller that
//! replaces an environment's active deployment once it has stayed `Unhealthy`
//! past the environment's timeout. The replacement is created from the last
//! deployment that served in the same environment and group (the last known
//! good) through the same primitive as a manual rollback
//! ([`promotion::create_from_source`]).
//!
//! The guard never tears anything down: the unhealthy deployment keeps running
//! until the replacement turns Healthy and the (fenced) supersession retires
//! it, and if it recovers first the guard cancels the replacement. A partial
//! unique index allows one replacement per unhealthy deployment, and the
//! creating transaction re-checks the subject under a row lock, so a recovery
//! and a trigger cannot both win. Every decision is recorded as a
//! `rollback_guard` event on the unhealthy deployment.

use std::time::Duration;

use chrono::{DateTime, Utc};
use rise_backend_core::events::{attributes, EventKind, EventSeverity, EventSource};
use rise_runtime_sync::{leader_controller, LeaderElection, LEASE_DURATION};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use super::promotion::{self, BarrierOutcome, CreateFromSource, EffectiveConstraints, EnvMode};
use super::state_machine;
use crate::db::deployment_events::{self, NewEvent};
use crate::db::deployments::{self as db_deployments, GuardSubject};
use crate::db::models::{CreatedVia, Deployment, DeploymentStatus, Environment, Project};
use crate::db::{environments as db_environments, projects as db_projects};
use crate::server::error::ServerError;
use crate::server::settings::RollbackGuardSettings;
use crate::server::state::AppState;

/// How far back failed replacements are reported. Each is reported once
/// (dedupe key), so this only bounds the query.
const FAILED_CANDIDATE_LOOKBACK_HOURS: i64 = 24;

/// Why the guard did not replace an unhealthy deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeclineReason {
    Chained,
    PlatformWideUnhealthy,
    InFlightLimit,
    NewerDeploymentInProgress,
    NoLastKnownGood,
    LastKnownGoodNotSuperseded,
    LastKnownGoodUnavailable,
    MigrationBarrier,
    MigrationVersionUnknown,
}

impl DeclineReason {
    /// The event's `reason` code and its human-readable description.
    fn parts(self) -> (&'static str, &'static str) {
        match self {
            Self::Chained => ("chained", "it is itself a Rollback Guard replacement"),
            Self::PlatformWideUnhealthy => (
                "platform_wide_unhealthy",
                "a large share of the platform's deployments are unhealthy at once",
            ),
            Self::InFlightLimit => (
                "in_flight_limit",
                "too many Rollback Guard replacements are already in progress",
            ),
            Self::NewerDeploymentInProgress => (
                "newer_deployment_in_progress",
                "a newer deployment is in progress",
            ),
            Self::NoLastKnownGood => (
                "no_last_known_good",
                "no earlier deployment in the group ever served",
            ),
            Self::LastKnownGoodNotSuperseded => (
                "last_known_good_not_superseded",
                "the last deployment that served was not replaced by a newer one",
            ),
            Self::LastKnownGoodUnavailable => (
                "last_known_good_unavailable",
                "the last deployment that served cannot be reused",
            ),
            Self::MigrationBarrier => (
                "migration_barrier",
                "going back crosses a recorded migration barrier",
            ),
            Self::MigrationVersionUnknown => (
                "migration_version_unknown",
                "the last deployment that served declares no migration version",
            ),
        }
    }
}

/// Everything [`decide`] looks at besides the subject itself.
#[derive(Debug, Clone)]
struct GuardFacts<'a> {
    fleet_tripped: bool,
    /// Replacements on their way up right now.
    in_flight: i64,
    max_in_flight: u32,
    newer_in_progress: bool,
    /// See [`db_deployments::find_last_served_before`].
    last_known_good: Option<&'a Deployment>,
    barriers: &'a BarrierOutcome,
}

/// The declines that need nothing beyond the tick's shared counts, checked
/// before anything else is loaded. The guard makes one hop and never chains.
fn early_decline(
    subject: &Deployment,
    fleet_tripped: bool,
    in_flight: i64,
    max_in_flight: u32,
) -> Option<DeclineReason> {
    if subject.created_via == CreatedVia::RollbackGuard {
        Some(DeclineReason::Chained)
    } else if fleet_tripped {
        Some(DeclineReason::PlatformWideUnhealthy)
    } else if in_flight >= i64::from(max_in_flight) {
        Some(DeclineReason::InFlightLimit)
    } else {
        None
    }
}

/// Decide whether to replace `subject`; `Ok` carries the last known good.
fn decide<'a>(subject: &Deployment, f: &GuardFacts<'a>) -> Result<&'a Deployment, DeclineReason> {
    use DeclineReason::*;
    if let Some(reason) = early_decline(subject, f.fleet_tripped, f.in_flight, f.max_in_flight) {
        return Err(reason);
    }
    if f.newer_in_progress {
        return Err(NewerDeploymentInProgress);
    }
    let lkg = f.last_known_good.ok_or(NoLastKnownGood)?;
    if lkg.status != DeploymentStatus::Superseded {
        return Err(LastKnownGoodNotSuperseded);
    }
    if !state_machine::can_create_from(lkg) {
        return Err(LastKnownGoodUnavailable);
    }
    match f.barriers {
        BarrierOutcome::Ok
        | BarrierOutcome::Unknown {
            has_barriers: false,
        } => Ok(lkg),
        BarrierOutcome::Unknown { has_barriers: true } => Err(MigrationVersionUnknown),
        BarrierOutcome::Crossed { .. } => Err(MigrationBarrier),
    }
}

/// The fleet circuit breaker: enough active deployments are unhealthy, and
/// they are a large enough share of the active ones, that the cause is more
/// likely the platform (a Traefik API outage marks every health-checked
/// Docker/ECS deployment Unhealthy) than any one deployment.
fn fleet_breaker_tripped(healthy: i64, unhealthy: i64, settings: &RollbackGuardSettings) -> bool {
    let total = healthy + unhealthy;
    unhealthy >= i64::from(settings.fleet_unhealthy_min_count)
        && total > 0
        && unhealthy as f64 / total as f64 >= settings.fleet_unhealthy_ratio
}

fn deployment_ref(d: &Deployment) -> Value {
    json!({ "kind": "deployment", "name": d.deployment_id })
}

/// Append a `rollback_guard` event to the unhealthy deployment `subject_id`,
/// at most once per `dedupe_key`. Returns whether it was written.
async fn record_guard_event(
    pool: &sqlx::PgPool,
    subject_id: Uuid,
    severity: EventSeverity,
    message: &str,
    attributes: &Value,
    dedupe_key: &str,
) -> anyhow::Result<bool> {
    deployment_events::record(
        pool,
        NewEvent {
            deployment_id: subject_id,
            kind: EventKind::RollbackGuard.as_str(),
            severity: severity.as_str(),
            source: EventSource::ControlPlane.as_str(),
            message: Some(message),
            attributes,
            dedupe_key: Some(dedupe_key),
        },
    )
    .await
}

/// Record that the guard declined to replace `subject`, once per reason.
/// `extra` (an object, or null) is merged into the attributes.
async fn record_decline(
    pool: &sqlx::PgPool,
    subject: &GuardSubject,
    reason: DeclineReason,
    extra: Value,
) -> anyhow::Result<bool> {
    let s = &subject.deployment;
    let (code, description) = reason.parts();
    let mut attrs = json!({
        (attributes::OUTCOME): "declined",
        (attributes::REASON): code,
        "unhealthy_since": s.unhealthy_since.map(|t| t.to_rfc3339()),
        "timeout_seconds": subject.timeout_seconds,
    });
    if let (Value::Object(attrs), Value::Object(extra)) = (&mut attrs, extra) {
        attrs.extend(extra);
    }
    let message = format!("Rollback Guard did not replace this deployment: {description}");
    let recorded = record_guard_event(
        pool,
        s.id,
        EventSeverity::Warning,
        &message,
        &attrs,
        &format!("declined:{code}"),
    )
    .await?;
    if recorded {
        warn!(
            project = %subject.project_name,
            environment = %subject.environment_name,
            deployment = %s.deployment_id,
            reason = code,
            "Rollback Guard declined to replace an unhealthy deployment"
        );
    } else {
        debug!(deployment = %s.deployment_id, reason = code, "Rollback Guard still declining");
    }
    Ok(recorded)
}

/// Cancel every open replacement whose subject no longer needs replacing:
/// it recovered (`Healthy`), or it is being or has been retired by someone
/// else (`Terminating` or terminal).
async fn reconcile_open_candidates(
    pool: &sqlx::PgPool,
    kube_client: Option<&kube::Client>,
    election: &LeaderElection,
) -> anyhow::Result<()> {
    for (candidate, subject) in db_deployments::list_open_rollback_guard_candidates(pool).await? {
        let (reason, message) = match &subject.status {
            DeploymentStatus::Healthy => (
                "recovered",
                "Rollback Guard cancelled its replacement: this deployment recovered",
            ),
            st if *st == DeploymentStatus::Terminating || state_machine::is_terminal(st) => (
                "replaced",
                "Rollback Guard cancelled its replacement: this deployment was replaced or stopped",
            ),
            _ => continue,
        };

        election.assert_leader().await?;
        let Some(cancelled) = db_deployments::mark_cancelling(pool, candidate.id).await? else {
            // It moved on (e.g. turned Healthy) since it was listed.
            continue;
        };
        info!(
            candidate = %cancelled.deployment_id,
            subject = %subject.deployment_id,
            reason,
            "Rollback Guard cancelled its replacement"
        );

        if let Err(e) = db_projects::update_calculated_status(pool, candidate.project_id).await {
            warn!(
                project_id = %candidate.project_id,
                "Failed to update project status after cancelling a Rollback Guard replacement: {:?}",
                e
            );
        }
        if let Some(kube_client) = kube_client {
            let resync = async {
                if let Some(project) = db_projects::find_by_id(pool, candidate.project_id).await? {
                    super::crd::trigger_resync(kube_client, &project.name).await?;
                }
                anyhow::Ok(())
            };
            if let Err(e) = resync.await {
                warn!(
                    project_id = %candidate.project_id,
                    "Failed to trigger CRD resync after cancelling a Rollback Guard replacement: {:?}",
                    e
                );
            }
        }

        record_guard_event(
            pool,
            subject.id,
            EventSeverity::Info,
            message,
            &json!({
                (attributes::OUTCOME): "aborted",
                (attributes::REASON): reason,
                (attributes::CANDIDATE): deployment_ref(&candidate),
            }),
            &format!("aborted:{}", candidate.id),
        )
        .await?;
    }
    Ok(())
}

/// Report replacements that failed since `since`, once each, on the
/// deployment they were meant to replace. That deployment keeps running: the
/// guard does not try again.
async fn report_failed_candidates(pool: &sqlx::PgPool, since: DateTime<Utc>) -> anyhow::Result<()> {
    for (candidate, subject) in
        db_deployments::list_failed_rollback_guard_candidates(pool, since).await?
    {
        let mut attrs = json!({
            (attributes::OUTCOME): "failed",
            (attributes::CANDIDATE): deployment_ref(&candidate),
        });
        if let Some(message) = &candidate.error_message {
            attrs[attributes::REASON] = json!(message);
        }
        let recorded = record_guard_event(
            pool,
            subject.id,
            EventSeverity::Error,
            "Rollback Guard replacement failed; this deployment keeps running",
            &attrs,
            &format!("failed:{}", candidate.id),
        )
        .await?;
        if recorded {
            error!(
                candidate = %candidate.deployment_id,
                subject = %subject.deployment_id,
                error = ?candidate.error_message,
                "Rollback Guard replacement failed"
            );
        }
    }
    Ok(())
}

/// The create request for replacing `subject` with `lkg`: the last known
/// good's image, containers, routes, identity and port, its sizing clamped
/// into the environment's current range, and its variable snapshot (minus
/// system and extension variables, which are derived fresh).
fn guard_request<'a>(
    project: &'a Project,
    environment: &'a Environment,
    subject: &'a Deployment,
    lkg: &'a Deployment,
    timeout_seconds: i32,
    deployment_id: &'a str,
    constraints: &EffectiveConstraints,
) -> Result<CreateFromSource<'a>, ServerError> {
    let clamped = promotion::clamp_inherited_resources(
        lkg,
        promotion::decode_source_specs(lkg)?,
        &promotion::ExplicitResources::default(),
        constraints,
    )?;
    let containers = match clamped.containers.as_ref() {
        Some(specs) => Some(super::models::encode_side_data(specs).map_err(|e| {
            ServerError::internal(format!("Failed to serialise containers: {e:?}"))
        })?),
        None => lkg.containers.clone(),
    };
    let mut extra = json!({ (attributes::LAST_KNOWN_GOOD): deployment_ref(lkg) });
    if !clamped.adjustments.is_empty() {
        extra[attributes::RESOURCE_ADJUSTMENTS] = json!(clamped.adjustments);
    }

    Ok(CreateFromSource {
        project,
        source: lkg,
        target_env: environment,
        group: &subject.deployment_group,
        deployment_id,
        created_by_id: lkg.created_by_id,
        created_via: CreatedVia::RollbackGuard,
        env_mode: EnvMode::SourceSnapshot,
        overrides: &[],
        http_port: u16::try_from(lkg.http_port).map_err(|_| {
            ServerError::internal(format!(
                "Deployment {} has an invalid http_port {}",
                lkg.deployment_id, lkg.http_port
            ))
        })?,
        replicas: clamped.replicas,
        cpu: clamped.cpu,
        memory: clamped.memory,
        containers,
        routes: lkg.routes.clone(),
        identity_audiences: lkg.identity_audiences.clone(),
        image: lkg.image.clone(),
        image_digest: lkg.image_digest.clone(),
        expires_at: None,
        expiration_cap: None,
        job_url: None,
        pull_request_url: None,
        git_repository_url: lkg.git_repository_url.clone(),
        rollback_guard_for: Some(subject.id),
        guard_lock: Some(timeout_seconds),
        extra_event_attributes: extra,
    })
}

/// The Rollback Guard controller. See the module documentation.
pub struct RollbackGuardController {
    state: AppState,
    settings: RollbackGuardSettings,
}

impl RollbackGuardController {
    pub fn new(state: AppState, settings: RollbackGuardSettings) -> Self {
        Self { state, settings }
    }

    /// Run under a leader election until `shutdown` is cancelled.
    pub async fn run(self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let pool = self.state.db_pool.clone();
        let interval = Duration::from_secs(self.settings.poll_interval_seconds.max(1));
        let me = self;
        leader_controller! {
            pool: pool,
            lease: "rise-rollback-guard",
            holder: Uuid::new_v4(),
            ttl: LEASE_DURATION,
            shutdown: shutdown,
            election: election,
            schedules: {
                "rise-rollback-guard" every interval => me.tick(&election).await,
            },
        }
        .await
    }

    /// One pass: settle open replacements, report failed ones, then decide
    /// about every deployment that has been unhealthy past its timeout.
    async fn tick(&self, election: &LeaderElection) -> anyhow::Result<()> {
        let pool = &self.state.db_pool;
        reconcile_open_candidates(pool, self.state.kube_client.as_ref(), election).await?;
        let since = Utc::now() - chrono::Duration::hours(FAILED_CANDIDATE_LOOKBACK_HOURS);
        report_failed_candidates(pool, since).await?;

        let subjects = db_deployments::list_rollback_guard_subjects(pool).await?;
        if subjects.is_empty() {
            return Ok(());
        }
        let (healthy, unhealthy) = db_deployments::active_health_counts(pool).await?;
        let fleet_tripped = fleet_breaker_tripped(healthy, unhealthy, &self.settings);
        if fleet_tripped {
            // Each subject also records a deduplicated decline; this repeats every tick.
            debug!(
                healthy,
                unhealthy,
                "Rollback Guard circuit breaker tripped: treating unhealthiness as platform-wide"
            );
        }
        let mut in_flight = db_deployments::count_open_rollback_guard_candidates(pool).await?;

        for subject in &subjects {
            if let Err(e) = self
                .consider(subject, fleet_tripped, &mut in_flight, election)
                .await
            {
                error!(
                    project = %subject.project_name,
                    environment = %subject.environment_name,
                    deployment = %subject.deployment.deployment_id,
                    "Rollback Guard failed to handle an unhealthy deployment: {:?}",
                    e
                );
            }
        }
        Ok(())
    }

    async fn consider(
        &self,
        subject: &GuardSubject,
        fleet_tripped: bool,
        in_flight: &mut i64,
        election: &LeaderElection,
    ) -> anyhow::Result<()> {
        let pool = &self.state.db_pool;
        let s = &subject.deployment;

        let max_in_flight = self.settings.max_in_flight;
        if let Some(reason) = early_decline(s, fleet_tripped, *in_flight, max_in_flight) {
            if reason == DeclineReason::Chained {
                error!(
                    project = %subject.project_name,
                    environment = %subject.environment_name,
                    deployment = %s.deployment_id,
                    "A Rollback Guard replacement is itself unhealthy; not replacing it again"
                );
            }
            record_decline(pool, subject, reason, Value::Null).await?;
            return Ok(());
        }

        let newer_in_progress = db_deployments::exists_newer_non_terminal_in_group(pool, s).await?;
        let last_known_good = if newer_in_progress {
            None
        } else {
            db_deployments::find_last_served_before(pool, s).await?
        };
        let barriers = match &last_known_good {
            Some(lkg) => promotion::check_barriers(
                pool,
                s.project_id,
                s.environment_id,
                lkg.migration_version,
            )
            .await
            .map_err(|e| anyhow::anyhow!("Failed to check migration barriers: {:?}", e))?,
            None => BarrierOutcome::Ok,
        };
        let facts = GuardFacts {
            fleet_tripped,
            in_flight: *in_flight,
            max_in_flight,
            newer_in_progress,
            last_known_good: last_known_good.as_ref(),
            barriers: &barriers,
        };

        let lkg = match decide(s, &facts) {
            Ok(lkg) => lkg,
            Err(reason) => {
                let mut extra = json!({});
                if let Some(lkg) = &last_known_good {
                    extra[attributes::LAST_KNOWN_GOOD] = deployment_ref(lkg);
                }
                if let BarrierOutcome::Crossed {
                    candidate,
                    environment,
                    barriers,
                } = &barriers
                {
                    extra["barriers"] = barriers
                        .iter()
                        .map(|b| json!({ "version": b.version, "reason": b.reason }))
                        .collect();
                    extra["candidate_version"] = json!(candidate);
                    extra["environment_version"] = json!(environment);
                }
                record_decline(pool, subject, reason, extra).await?;
                return Ok(());
            }
        };

        let project = db_projects::find_by_id(pool, s.project_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Project {} not found", s.project_id))?;
        let environment = db_environments::find_by_id(pool, s.environment_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Environment {} not found", s.environment_id))?;
        let constraints =
            EffectiveConstraints::resolve(self.state.deployment_constraints.as_ref(), &environment);
        let deployment_id = super::utils::generate_deployment_id();
        let request = match guard_request(
            &project,
            &environment,
            s,
            lkg,
            subject.timeout_seconds,
            &deployment_id,
            &constraints,
        ) {
            Ok(request) => request,
            Err(e) => {
                let extra = json!({
                    (attributes::LAST_KNOWN_GOOD): deployment_ref(lkg),
                    "detail": e.message,
                });
                let reason = DeclineReason::LastKnownGoodUnavailable;
                record_decline(pool, subject, reason, extra).await?;
                return Ok(());
            }
        };

        election.assert_leader().await?;
        let created = promotion::create_from_source(&self.state, request)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to create the replacement: {:?}", e))?;
        let Some(candidate) = created else {
            debug!(
                deployment = %s.deployment_id,
                "Rollback Guard trigger abandoned: the deployment recovered or was already replaced"
            );
            return Ok(());
        };
        *in_flight += 1;

        warn!(
            project = %subject.project_name,
            environment = %subject.environment_name,
            deployment = %s.deployment_id,
            candidate = %candidate.deployment_id,
            last_known_good = %lkg.deployment_id,
            "Rollback Guard is replacing an unhealthy deployment with its last known good"
        );
        record_guard_event(
            pool,
            s.id,
            EventSeverity::Warning,
            &format!(
                "Rollback Guard is replacing this deployment with {} (a redeploy of {})",
                candidate.deployment_id, lkg.deployment_id
            ),
            &json!({
                (attributes::OUTCOME): "triggered",
                (attributes::CANDIDATE): deployment_ref(&candidate),
                (attributes::LAST_KNOWN_GOOD): deployment_ref(lkg),
                "unhealthy_since": s.unhealthy_since.map(|t| t.to_rfc3339()),
                "timeout_seconds": subject.timeout_seconds,
            }),
            "triggered",
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::ProjectStatus;
    use crate::db::{deployments, environments, projects, users};
    use sqlx::PgPool;
    use DeclineReason as R;

    fn deployment(status: &str, created_via: &str) -> Deployment {
        serde_json::from_value(json!({
            "id": Uuid::new_v4(), "deployment_id": "20261025-000000",
            "project_id": Uuid::nil(), "created_by_id": Uuid::nil(), "environment_id": Uuid::nil(),
            "status": status, "created_via": created_via, "deployment_group": "default",
            "image_digest": "registry/app@sha256:abc", "controller_metadata": null,
            "http_port": 8080, "needs_reconcile": false, "is_active": false,
            "replicas": 1, "cpu": "500m", "memory": "256Mi", "identity_audiences": {},
            "created_at": Utc::now(), "updated_at": Utc::now(),
        }))
        .unwrap()
    }

    #[test]
    fn decide_table() {
        let sick = deployment("Unhealthy", "deploy");
        let chained = deployment("Unhealthy", "rollback_guard");
        let good = deployment("Superseded", "deploy");
        let stopped = deployment("Stopped", "deploy");
        let ok = BarrierOutcome::Ok;
        let crossed = BarrierOutcome::Crossed {
            candidate: 1,
            environment: 3,
            barriers: vec![],
        };
        let unknown = |has_barriers| BarrierOutcome::Unknown { has_barriers };
        let (unknown_with, unknown_without) = (unknown(true), unknown(false));
        let g = Some(&good);

        // (subject, fleet_tripped, in_flight, newer_in_progress, last known good, barriers, expected)
        #[rustfmt::skip]
        let cases = [
            (&sick, false, 0, false, g, &ok, Ok(good.id)),
            (&sick, false, 0, false, g, &unknown_without, Ok(good.id)),
            // Chained wins over everything; then the fleet breaker; then the limit.
            (&chained, true, 9, false, g, &ok, Err(R::Chained)),
            (&sick, true, 9, false, g, &ok, Err(R::PlatformWideUnhealthy)),
            (&sick, false, 3, true, g, &ok, Err(R::InFlightLimit)),
            (&sick, false, 0, true, None, &ok, Err(R::NewerDeploymentInProgress)),
            (&sick, false, 0, false, None, &ok, Err(R::NoLastKnownGood)),
            (&sick, false, 0, false, Some(&stopped), &ok, Err(R::LastKnownGoodNotSuperseded)),
            (&sick, false, 0, false, g, &crossed, Err(R::MigrationBarrier)),
            (&sick, false, 0, false, g, &unknown_with, Err(R::MigrationVersionUnknown)),
        ];
        for (
            i,
            (
                subject,
                fleet_tripped,
                in_flight,
                newer_in_progress,
                last_known_good,
                barriers,
                expected,
            ),
        ) in cases.into_iter().enumerate()
        {
            let facts = GuardFacts {
                fleet_tripped,
                in_flight,
                max_in_flight: 3,
                newer_in_progress,
                last_known_good,
                barriers,
            };
            assert_eq!(decide(subject, &facts).map(|d| d.id), expected, "case {i}");
        }
    }

    #[test]
    fn fleet_breaker_needs_both_count_and_ratio() {
        let settings = RollbackGuardSettings::default();
        for (healthy, unhealthy, tripped) in [
            (0, 0, false),
            (0, 2, false), // ratio 1.0 but below the minimum count
            (10, 3, false),
            (3, 3, true),
            (2, 3, true),
            (100, 60, false),
            (60, 60, true),
        ] {
            assert_eq!(
                fleet_breaker_tripped(healthy, unhealthy, &settings),
                tripped,
                "healthy={healthy} unhealthy={unhealthy}"
            );
        }
    }

    // Database tests

    struct Seeded {
        project: Project,
        environment: Environment,
        lkg: Deployment,
        subject: Deployment,
    }

    async fn reload(pool: &PgPool, id: Uuid) -> Deployment {
        deployments::find_by_id(pool, id).await.unwrap().unwrap()
    }

    const RECOVER: &str =
        "UPDATE deployments SET status = 'Healthy', unhealthy_since = NULL WHERE id = $1";

    async fn execute(pool: &PgPool, sql: &'static str, id: Uuid) {
        sqlx::query(sql).bind(id).execute(pool).await.unwrap();
    }

    /// A guarded production environment whose active deployment (`subject`)
    /// has been Unhealthy for an hour, with a Superseded predecessor that
    /// served (`lkg`).
    async fn seed(pool: &PgPool) -> Seeded {
        let user = users::create(pool, "guard@example.com").await.unwrap();
        let project = projects::create(
            pool,
            "guard-project",
            ProjectStatus::Running,
            "public".to_string(),
            Some(user.id),
            None,
            None,
        )
        .await
        .unwrap();
        let env_id = environments::test_environment_id(pool, project.id).await;
        let environment =
            environments::update_promotion_policy(pool, env_id, None, Some(true), Some(60))
                .await
                .unwrap();

        let ids: Vec<Uuid> = sqlx::query_scalar(
            "INSERT INTO deployments (deployment_id, project_id, created_by_id, environment_id, \
               deployment_group, image, image_digest, migration_version, status, \
               termination_reason, is_active, created_at, deploying_started_at, \
               first_healthy_at, completed_at, unhealthy_since) \
             SELECT v.name, $1, $2, $3, $4, 'nginx', 'docker.io/library/nginx@sha256:abc', 3, \
               v.status, v.reason::termination_reason, v.active, \
               NOW() - v.age, NOW() - v.age, NOW() - v.age, v.completed, v.unhealthy \
             FROM (VALUES \
               (1, 'lkg', 'Superseded', 'Superseded', false, interval '4 hours', \
                NOW() - interval '2 hours', NULL::timestamptz), \
               (2, 'sick', 'Unhealthy', NULL, true, interval '2 hours', \
                NULL, NOW() - interval '1 hour') \
             ) AS v(n, name, status, reason, active, age, completed, unhealthy) \
             ORDER BY v.n RETURNING id",
        )
        .bind(project.id)
        .bind(user.id)
        .bind(environment.id)
        .bind(environment.primary_deployment_group.as_deref().unwrap())
        .fetch_all(pool)
        .await
        .unwrap();

        Seeded {
            project,
            environment,
            lkg: reload(pool, ids[0]).await,
            subject: reload(pool, ids[1]).await,
        }
    }

    async fn create_candidate(
        pool: &PgPool,
        seeded: &Seeded,
        deployment_id: &str,
    ) -> Option<Deployment> {
        let constraints = EffectiveConstraints::resolve(None, &seeded.environment);
        let request = guard_request(
            &seeded.project,
            &seeded.environment,
            &seeded.subject,
            &seeded.lkg,
            60,
            deployment_id,
            &constraints,
        )
        .unwrap();
        promotion::create_from_source_in(pool, request, vec![], vec![], |_| async { Ok(vec![]) })
            .await
            .unwrap()
    }

    async fn guard_events(pool: &PgPool, deployment_id: Uuid) -> Vec<(String, Value)> {
        sqlx::query_as::<_, (Option<String>, Value)>(
            "SELECT dedupe_key, attributes FROM deployment_events \
             WHERE deployment_id = $1 AND kind = 'rollback_guard' ORDER BY id",
        )
        .bind(deployment_id)
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|(k, a)| (k.unwrap_or_default(), a))
        .collect()
    }

    async fn leader(pool: &PgPool) -> LeaderElection {
        rise_runtime_sync::run_migrations(pool)
            .await
            .expect("runtime sync migrations");
        let election = LeaderElection::spawn(
            pool.clone(),
            "test/rollback-guard",
            Uuid::new_v4(),
            LEASE_DURATION,
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !election.is_leader() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "no leadership within 5s"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        election
    }

    #[sqlx::test]
    async fn trigger_creates_exactly_one_candidate(pool: PgPool) {
        let seeded = seed(&pool).await;

        // The seeded subject is found, and its last known good qualifies.
        let subjects = deployments::list_rollback_guard_subjects(&pool)
            .await
            .unwrap();
        assert_eq!(subjects.len(), 1);
        assert_eq!(subjects[0].deployment.id, seeded.subject.id);
        let lkg = deployments::find_last_served_before(&pool, &seeded.subject)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(lkg.id, seeded.lkg.id);
        let barriers = promotion::check_barriers(
            &pool,
            seeded.project.id,
            seeded.environment.id,
            lkg.migration_version,
        )
        .await
        .unwrap();
        let facts = GuardFacts {
            fleet_tripped: false,
            in_flight: 0,
            max_in_flight: 3,
            newer_in_progress: false,
            last_known_good: Some(&lkg),
            barriers: &barriers,
        };
        assert!(decide(&seeded.subject, &facts).is_ok());

        let (a, b) = tokio::join!(
            create_candidate(&pool, &seeded, "guard-a"),
            create_candidate(&pool, &seeded, "guard-b"),
        );
        let created: Vec<Deployment> = [a, b].into_iter().flatten().collect();
        assert_eq!(created.len(), 1, "exactly one trigger must win");
        let candidate = &created[0];
        assert_eq!(candidate.created_via, CreatedVia::RollbackGuard);
        assert_eq!(
            candidate.rollback_guard_for_deployment_id,
            Some(seeded.subject.id)
        );
        assert_eq!(
            candidate.rolled_back_from_deployment_id,
            Some(seeded.lkg.id)
        );
        assert_eq!(candidate.created_by_id, seeded.lkg.created_by_id);
        assert_eq!(candidate.migration_version, Some(3));
        assert_eq!(candidate.status, DeploymentStatus::Pushed);
        assert_eq!(candidate.deployment_group, seeded.subject.deployment_group);

        // Once replaced, it is no longer a subject, and a third try loses too.
        assert!(deployments::list_rollback_guard_subjects(&pool)
            .await
            .unwrap()
            .is_empty());
        assert!(create_candidate(&pool, &seeded, "guard-c").await.is_none());
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM deployments WHERE rollback_guard_for_deployment_id = $1",
        )
        .bind(seeded.subject.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 1);
    }

    #[sqlx::test]
    async fn subject_recovered_before_commit_yields_none(pool: PgPool) {
        let seeded = seed(&pool).await;
        let subject_id = seeded.subject.id;

        // A recovery holding the subject's row lock when the trigger arrives:
        // the trigger waits on the lock, then re-checks and finds it Healthy.
        let recovery = async {
            let mut tx = pool.begin().await.unwrap();
            sqlx::query("SELECT id FROM deployments WHERE id = $1 FOR UPDATE")
                .bind(subject_id)
                .execute(&mut *tx)
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
            sqlx::query(RECOVER)
                .bind(subject_id)
                .execute(&mut *tx)
                .await
                .unwrap();
            tx.commit().await.unwrap();
        };
        let trigger = async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            create_candidate(&pool, &seeded, "guard-late").await
        };
        let ((), created) = tokio::join!(recovery, trigger);
        assert!(created.is_none());

        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM deployments WHERE deployment_id = 'guard-late'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 0, "nothing of the abandoned create may remain");
    }

    #[sqlx::test]
    async fn abort_on_recovery_cancels_open_candidate(pool: PgPool) {
        let seeded = seed(&pool).await;
        let election = leader(&pool).await;
        let candidate = create_candidate(&pool, &seeded, "guard-a").await.unwrap();
        let reconcile = || reconcile_open_candidates(&pool, None, &election);

        // Still unhealthy: the replacement is left alone.
        reconcile().await.unwrap();
        assert_eq!(
            reload(&pool, candidate.id).await.status,
            DeploymentStatus::Pushed
        );
        assert!(guard_events(&pool, seeded.subject.id).await.is_empty());

        execute(&pool, RECOVER, seeded.subject.id).await;
        reconcile().await.unwrap();
        reconcile().await.unwrap();

        assert_eq!(
            reload(&pool, candidate.id).await.status,
            DeploymentStatus::Cancelling
        );
        let events = guard_events(&pool, seeded.subject.id).await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, format!("aborted:{}", candidate.id));
        assert_eq!(events[0].1["outcome"], "aborted");
        assert_eq!(events[0].1["reason"], "recovered");
        assert_eq!(events[0].1["candidate"]["name"], "guard-a");
        // The recovered deployment itself is untouched.
        assert_eq!(
            reload(&pool, seeded.subject.id).await.status,
            DeploymentStatus::Healthy
        );
        election.release().await.unwrap();
    }

    #[sqlx::test]
    async fn decline_events_are_deduplicated(pool: PgPool) {
        let seeded = seed(&pool).await;
        let subject = deployments::list_rollback_guard_subjects(&pool)
            .await
            .unwrap()
            .pop()
            .unwrap();
        let decline = |reason, extra| record_decline(&pool, &subject, reason, extra);

        assert!(decline(R::InFlightLimit, Value::Null).await.unwrap());
        assert!(!decline(R::InFlightLimit, Value::Null).await.unwrap());
        let extra = json!({ "last_known_good": deployment_ref(&seeded.lkg) });
        assert!(decline(R::MigrationBarrier, extra).await.unwrap());

        let events = guard_events(&pool, seeded.subject.id).await;
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].0, "declined:in_flight_limit");
        assert_eq!(events[0].1["outcome"], "declined");
        assert_eq!(events[0].1["reason"], "in_flight_limit");
        assert_eq!(events[0].1["timeout_seconds"], 60);
        assert!(events[0].1["unhealthy_since"].is_string());
        assert_eq!(events[1].0, "declined:migration_barrier");
        assert_eq!(events[1].1["last_known_good"]["name"], "lkg");
    }

    #[sqlx::test]
    async fn failed_candidates_are_reported_once(pool: PgPool) {
        let seeded = seed(&pool).await;
        let candidate = create_candidate(&pool, &seeded, "guard-a").await.unwrap();
        let fail = "UPDATE deployments SET status = 'Failed', error_message = 'crash loop', \
                    completed_at = NOW() WHERE id = $1";
        execute(&pool, fail, candidate.id).await;

        let since = Utc::now() - chrono::Duration::hours(1);
        report_failed_candidates(&pool, since).await.unwrap();
        report_failed_candidates(&pool, since).await.unwrap();

        let events = guard_events(&pool, seeded.subject.id).await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].1["outcome"], "failed");
        assert_eq!(events[0].1["reason"], "crash loop");
        // The unhealthy deployment keeps running.
        assert_eq!(
            reload(&pool, seeded.subject.id).await.status,
            DeploymentStatus::Unhealthy
        );
    }

    #[sqlx::test]
    async fn guard_request_redeploys_the_last_known_good_clamped_into_range(pool: PgPool) {
        let seeded = seed(&pool).await;
        let constraints = EffectiveConstraints {
            max_cpu: "250m".to_string(),
            ..EffectiveConstraints::resolve(None, &seeded.environment)
        };
        let request = guard_request(
            &seeded.project,
            &seeded.environment,
            &seeded.subject,
            &seeded.lkg,
            120,
            "guard-a",
            &constraints,
        )
        .unwrap();

        assert_eq!(request.source.id, seeded.lkg.id);
        assert_eq!(request.target_env.id, seeded.environment.id);
        assert_eq!(request.group, seeded.subject.deployment_group);
        assert_eq!(request.created_by_id, seeded.lkg.created_by_id);
        assert_eq!(request.created_via, CreatedVia::RollbackGuard);
        assert_eq!(request.env_mode, EnvMode::SourceSnapshot);
        assert!(request.overrides.is_empty());
        assert_eq!(request.http_port, 8080);
        assert_eq!(request.image_digest, seeded.lkg.image_digest);
        assert_eq!(request.rollback_guard_for, Some(seeded.subject.id));
        assert_eq!(request.guard_lock, Some(120));
        assert_eq!(request.expires_at, None);
        assert_eq!(
            (request.cpu.as_str(), request.memory.as_str()),
            ("250m", "256Mi")
        );
        let attrs = &request.extra_event_attributes;
        assert_eq!(attrs["last_known_good"]["name"], "lkg");
        let adjustment = &attrs["resource_adjustments"][0];
        assert_eq!(attrs["resource_adjustments"].as_array().unwrap().len(), 1);
        assert_eq!(
            ["field", "from", "to"].map(|k| adjustment[k].as_str().unwrap()),
            ["cpu", "500m", "250m"]
        );
    }
}
