//! Pure ECS service diff: desired vs. observed services → create / update /
//! delete actions.
//!
//! **Why this is not `rise_backend_core::diff`.** The Docker backend models one
//! container per replica, so its identity tuple carries a replica index and its
//! only response to drift is remove-then-create. ECS is structurally different
//! in a way that simplifies us: an ECS *service* owns its replicas via
//! `desiredCount`, and pointing it at a new task-definition revision makes ECS
//! perform the rolling replacement itself. So the unit of state here is the
//! service — the core `spec_key` tuple, replica-free — and drift is answered by
//! `UpdateService` rather than by destroying anything. That removes the outage
//! window that would otherwise be far worse on ECS, where a task takes tens of
//! seconds to start rather than one.

use std::collections::{BTreeSet, HashMap, HashSet};

use rise_backend_core::naming::sanitize_ecs_name;
use rise_backend_core::spec_key;

use crate::capacity::Capacity;
use crate::tags::ServiceTags;

/// The service-level shape Rise controls: where the tasks' ENIs live, and what
/// capacity places them.
///
/// Separate from the task definition on purpose. These are properties of the
/// *service*, so folding them into `TaskDefinitionSpec::content_hash` would
/// register a new revision for a change that needs none — and
/// `RegisterTaskDefinition` sustains only 1 request/second.
///
/// Subnets and security groups are sets, not lists: ECS does not preserve the
/// order they were sent in, so comparing sequences would report drift on every
/// tick and re-roll the fleet forever.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredShape {
    pub subnets: BTreeSet<String>,
    pub security_groups: BTreeSet<String>,
    pub assign_public_ip: bool,
    pub capacity: Capacity,
}

/// The same shape as ECS reports it for a running service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedShape {
    pub subnets: BTreeSet<String>,
    pub security_groups: BTreeSet<String>,
    pub assign_public_ip: bool,
    /// `launchType`, when the service was created with one.
    pub launch_type: Option<String>,
    /// Provider names from `capacityProviderStrategy`. ECS reports this **or**
    /// `launch_type`, never both.
    pub capacity_providers: Vec<String>,
}

impl DesiredShape {
    /// Whether the observed network configuration already matches.
    pub fn network_matches(&self, observed: &ObservedShape) -> bool {
        self.subnets == observed.subnets
            && self.security_groups == observed.security_groups
            && self.assign_public_ip == observed.assign_public_ip
    }

    /// Whether the observed capacity already matches.
    pub fn capacity_matches(&self, observed: &ObservedShape) -> bool {
        self.capacity.matches_observed(
            observed.launch_type.as_deref(),
            &observed.capacity_providers,
        )
    }
}

/// A service Rise wants to exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredService {
    /// ECS service name.
    pub name: String,
    /// Identity tuple, replica-free: `project\0group\0deployment_id\0container`.
    pub key: String,
    /// Task-definition family this service runs.
    pub family: String,
    /// Content hash of the desired task definition. Drift here means a new
    /// revision must be registered and the service pointed at it.
    pub task_definition_hash: String,
    pub desired_count: i32,
    pub tags: ServiceTags,
    /// Service-level configuration: network placement and capacity.
    pub shape: DesiredShape,
}

/// One line of an ECS service's event narrative.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceNarrative {
    /// ECS's own id for the event — stable per occurrence, so it doubles as the
    /// dedupe key when the same tick is seen twice.
    pub id: String,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub message: String,
}

/// A service observed in the cluster, reduced to what the diff needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActualService {
    pub name: String,
    /// The service's own ARN, as `DescribeServices` reports it.
    ///
    /// Carried rather than reconstructed because `ecs:TagResource` takes a real
    /// ARN and there is no way to build one from a cluster *name* -- which is
    /// what `deployment_controller.cluster` holds.
    pub arn: String,
    /// Identity recovered from tags. `None` when the tags are incomplete — such
    /// a service can never be matched, only (potentially) GC'd.
    pub key: Option<String>,
    /// ARN of the task definition the service currently runs.
    pub task_definition_arn: String,
    /// The `task-definition-hash` tag stamped when Rise last updated it.
    pub task_definition_hash: String,
    pub desired_count: i32,
    pub running_count: i32,
    /// `deployment-id` tag, used by the protected-deployment GC guard.
    pub deployment_id: Option<String>,
    /// `project` / `deployment-group` tags. Together with `deployment_id` they
    /// form the SSM path prefix, so a `Delete` can retire the service's secret
    /// parameters. Empty for an unattributable service (which is never diffed
    /// into a `Delete`).
    pub project: String,
    pub deployment_group: String,
    /// When ECS started rolling out the service's PRIMARY deployment, as
    /// `DescribeServices` reports it. `None` when ECS reports no primary.
    /// The service's own narrative, newest first, as ECS returns it. Kept
    /// because it explains what the task list cannot: a service with no tasks
    /// looks the same whether the image is missing or the cluster is full, and
    /// only this says which.
    ///
    /// Already in every `DescribeServices` response the reconciler makes, so
    /// keeping it costs no additional API call — which matters against ECS's
    /// read budget.
    pub events: Vec<ServiceNarrative>,
    pub rollout_started_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Network placement and capacity, read straight from `DescribeServices`.
    ///
    /// Read from the API rather than recovered from a Rise tag deliberately. A
    /// tag would be absent on every service that predates it, making the whole
    /// fleet look drifted on the first tick after an upgrade — and since
    /// `networkConfiguration` is one of the parameters that makes ECS start new
    /// tasks, that is a fleet-wide rolling restart. The API cannot go stale the
    /// way a cached tag can.
    ///
    /// `None` when ECS reported no network configuration at all. Unknown is not
    /// drift: an absent shape is left alone rather than "corrected".
    pub shape: Option<ObservedShape>,
}

/// Whether a `Healthy` deployment should keep that status while ECS replaces
/// its tasks in place.
///
/// An in-place `UpdateService` is a rolling replacement: ECS keeps the outgoing
/// revision serving every request until the incoming one is up. Readiness is
/// judged against the incoming revision — that is the whole point of pairing
/// replica slots to it — so for the length of the roll it reads as "not ready",
/// which is true of the revision and false of the deployment. Flipping to
/// `Unhealthy` there would report an outage that is not happening, on a path
/// that runs for every custom domain added to a live project.
///
/// The hold is deliberately not unconditional. It requires the outgoing
/// revision to actually still be serving, so a genuine outage is still reported;
/// and it expires, so a rollout that never completes stops being papered over —
/// otherwise a broken in-place change would leave a project reading `Healthy`
/// forever while serving the content it was supposed to replace.
///
/// `just_registered` covers the tick that starts the roll, where ECS has not yet
/// reported a new PRIMARY deployment and `rollout_started_at` still describes
/// the outgoing one.
pub fn hold_status_during_rollout(
    outgoing_still_serving: bool,
    just_registered: bool,
    rollout_started_at: Option<chrono::DateTime<chrono::Utc>>,
    now: chrono::DateTime<chrono::Utc>,
    window: chrono::Duration,
) -> bool {
    if !outgoing_still_serving {
        return false;
    }
    if just_registered {
        return true;
    }
    rollout_started_at.is_some_and(|started| now - started < window)
}

/// What the reconciler should do about one service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceAction {
    /// Register the task definition and create the service.
    Create { key: String, name: String },
    /// Register a new task-definition revision and point the service at it.
    /// ECS performs the rolling replacement.
    UpdateTaskDefinition { key: String, name: String },
    /// Apply service-level configuration — network placement — to a service
    /// whose task definition is already current. No new revision is registered:
    /// nothing about the task changed, and `RegisterTaskDefinition` sustains
    /// only 1 request/second.
    UpdateServiceShape {
        key: String,
        name: String,
        desired_count: i32,
    },
    /// Scale only — no new revision needed.
    UpdateDesiredCount {
        key: String,
        name: String,
        desired_count: i32,
    },
    /// Retire a service Rise no longer wants.
    Delete { name: String },
}

/// Deterministic ordering so a tick's actions are reproducible (and testable):
/// creates first so capacity is added before anything is retired, then updates,
/// then deletes. Within a class, by name.
fn action_key(a: &ServiceAction) -> (u8, String) {
    match a {
        ServiceAction::Create { name, .. } => (0, name.clone()),
        ServiceAction::UpdateTaskDefinition { name, .. } => (1, name.clone()),
        ServiceAction::UpdateServiceShape { name, .. } => (2, name.clone()),
        ServiceAction::UpdateDesiredCount { name, .. } => (3, name.clone()),
        ServiceAction::Delete { name } => (4, name.clone()),
    }
}

/// ECS service name for one deployment's container spec.
///
/// Unlike the task-definition family, this **does** carry the deployment id: the
/// outgoing and incoming deployments must be able to run simultaneously during a
/// cutover, which means two distinct services.
pub fn service_name(
    resource_prefix: &str,
    project: &str,
    deployment_group: &str,
    deployment_id: &str,
    container: &str,
) -> String {
    sanitize_ecs_name(&format!(
        "{resource_prefix}-{project}-{deployment_group}-{deployment_id}-{container}"
    ))
}

/// Identity key for a desired/actual service — the core `spec_key` tuple.
pub fn service_key(project: &str, group: &str, deployment_id: &str, container: &str) -> String {
    spec_key(project, group, deployment_id, container)
}

/// One service running on a capacity other than the one now configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapacityDrift {
    pub name: String,
    pub desired: String,
    pub observed: String,
}

/// Services whose capacity no longer matches what is configured.
///
/// Deliberately **not** a [`ServiceAction`]. Moving a live service between a
/// launch type and a capacity provider is a retopology, not a routine
/// reconcile: AWS's own two references disagree about which of those
/// transitions `UpdateService` performs, and the ones that are allowed need a
/// forced new deployment. Attempting it on every tick would either roll the
/// fleet or fail the same call forever.
///
/// Rise creates one service per (deployment, container spec), so the next
/// deploy creates fresh services on the new capacity with no special handling.
/// Until then the mismatch is reported, never silently tolerated -- the same
/// posture as `reject_unsupported`.
pub fn capacity_drift(desired: &[DesiredService], actual: &[ActualService]) -> Vec<CapacityDrift> {
    let actual_by_key: HashMap<&str, &ActualService> = actual
        .iter()
        .filter_map(|a| a.key.as_deref().map(|k| (k, a)))
        .collect();

    let mut drift: Vec<CapacityDrift> = desired
        .iter()
        .filter_map(|d| {
            let a = actual_by_key.get(d.key.as_str())?;
            let observed = a.shape.as_ref()?;
            if d.shape.capacity_matches(observed) {
                return None;
            }
            Some(CapacityDrift {
                name: a.name.clone(),
                desired: d.shape.capacity.describe(),
                observed: describe_observed_capacity(observed),
            })
        })
        .collect();
    drift.sort_by(|a, b| a.name.cmp(&b.name));
    drift
}

fn describe_observed_capacity(observed: &ObservedShape) -> String {
    if !observed.capacity_providers.is_empty() {
        return format!(
            "capacity provider {}",
            observed.capacity_providers.join(", ")
        );
    }
    match &observed.launch_type {
        Some(lt) => format!("launch type {lt}"),
        None => "an unreported capacity".to_string(),
    }
}

/// Diff desired services against what the cluster actually runs.
///
/// `protected_deployment_ids` names deployments whose desired state could not be
/// computed this tick (a decryption failure, an unreadable row). Their services
/// look orphaned but must **not** be deleted — the same guard the Docker backend
/// applies, and for the same reason: a transient error must never take a running
/// app offline.
pub fn diff_services(
    desired: &[DesiredService],
    actual: &[ActualService],
    protected_deployment_ids: &HashSet<String>,
) -> Vec<ServiceAction> {
    let mut actions = Vec::new();
    let actual_by_key: HashMap<&str, &ActualService> = actual
        .iter()
        .filter_map(|a| a.key.as_deref().map(|k| (k, a)))
        .collect();
    let mut matched: HashSet<&str> = HashSet::new();

    for d in desired {
        match actual_by_key.get(d.key.as_str()) {
            None => actions.push(ServiceAction::Create {
                key: d.key.clone(),
                name: d.name.clone(),
            }),
            Some(a) => {
                matched.insert(a.name.as_str());
                if a.task_definition_hash != d.task_definition_hash {
                    // Content changed: new revision, ECS rolls it.
                    actions.push(ServiceAction::UpdateTaskDefinition {
                        key: d.key.clone(),
                        name: a.name.clone(),
                    });
                } else if a
                    .shape
                    .as_ref()
                    .is_some_and(|observed| !d.shape.network_matches(observed))
                {
                    // The operator moved the workload's subnets, security groups
                    // or public-IP assignment. Nothing about the task changed,
                    // so this needs no revision -- but it does need applying,
                    // which is why it outranks a scale change: the scale-only
                    // call omits the network configuration entirely.
                    actions.push(ServiceAction::UpdateServiceShape {
                        key: d.key.clone(),
                        name: a.name.clone(),
                        desired_count: d.desired_count,
                    });
                } else if a.desired_count != d.desired_count {
                    // Scale-only: no revision churn, and RegisterTaskDefinition
                    // sustains just 1/s so avoiding a needless one matters.
                    actions.push(ServiceAction::UpdateDesiredCount {
                        key: d.key.clone(),
                        name: a.name.clone(),
                        desired_count: d.desired_count,
                    });
                }
            }
        }
    }

    for a in actual {
        if matched.contains(a.name.as_str()) {
            continue;
        }
        if let Some(deployment_id) = &a.deployment_id {
            if protected_deployment_ids.contains(deployment_id) {
                continue;
            }
        }
        actions.push(ServiceAction::Delete {
            name: a.name.clone(),
        });
    }

    actions.sort_by_key(action_key);
    actions
}

#[cfg(test)]
mod tests {

    /// The messages worth waking someone for: the service wants tasks and
    /// cannot get them. This is the case a task list cannot explain — no tasks
    /// looks identical whether the image is missing or the cluster is full.
    #[test]
    fn placement_failures_are_errors() {
        for m in [
            "service rise-web was unable to place a task because no container instance met all of its requirements.",
            "service rise-web is unable to consistently start tasks successfully.",
        ] {
            assert_eq!(super::narrative_severity(m), super::NarrativeSeverity::Error, "{m}");
        }
    }

    #[test]
    fn health_and_stop_messages_are_warnings() {
        for m in [
            "(service rise-web) (task abc) failed ELB health checks in (target-group tg).",
            "(service rise-web) has stopped 1 running tasks: (task abc).",
        ] {
            assert_eq!(
                super::narrative_severity(m),
                super::NarrativeSeverity::Warning,
                "{m}"
            );
        }
    }

    /// Routine progress, and — importantly — anything AWS rewords. An
    /// unrecognised message must still be forwarded, just without an elevated
    /// severity: silently dropping it would be worse than under-ranking it.
    #[test]
    fn routine_and_unrecognised_messages_are_info() {
        for m in [
            "(service rise-web) has reached a steady state.",
            "(service rise-web) has started 1 tasks: (task abc).",
            "something AWS has not said before",
        ] {
            assert_eq!(
                super::narrative_severity(m),
                super::NarrativeSeverity::Info,
                "{m}"
            );
        }
    }
    use super::*;

    fn tags(deployment_id: &str) -> ServiceTags {
        ServiceTags {
            project: "myapp".to_string(),
            project_uuid: "22222222-2222-2222-2222-222222222222".to_string(),
            deployment_group: "default".to_string(),
            deployment_id: deployment_id.to_string(),
            deployment_uuid: "11111111-1111-1111-1111-111111111111".to_string(),
            container: "app".to_string(),
            environment: None,
            env_hash: "eh".to_string(),
            image: "img".to_string(),
            route_hash: "rh".to_string(),
        }
    }

    fn shape() -> DesiredShape {
        DesiredShape {
            subnets: ["subnet-a".to_string(), "subnet-b".to_string()].into(),
            security_groups: ["sg-1".to_string()].into(),
            assign_public_ip: false,
            capacity: Capacity::Fargate,
        }
    }

    /// The observed shape of a service that already matches [`shape`].
    fn converged_shape() -> ObservedShape {
        ObservedShape {
            subnets: ["subnet-a".to_string(), "subnet-b".to_string()].into(),
            security_groups: ["sg-1".to_string()].into(),
            assign_public_ip: false,
            launch_type: Some("FARGATE".to_string()),
            capacity_providers: Vec::new(),
        }
    }

    fn desired(deployment_id: &str, hash: &str, count: i32) -> DesiredService {
        DesiredService {
            name: service_name("rise", "myapp", "default", deployment_id, "app"),
            key: service_key("myapp", "default", deployment_id, "app"),
            family: "rise-myapp-default-app".to_string(),
            task_definition_hash: hash.to_string(),
            desired_count: count,
            tags: tags(deployment_id),
            shape: shape(),
        }
    }

    fn actual_for(d: &DesiredService, hash: &str, count: i32) -> ActualService {
        ActualService {
            shape: Some(converged_shape()),
            events: Vec::new(),
            name: d.name.clone(),
            arn: format!(
                "arn:aws:ecs:eu-central-1:123456789012:service/rise/{}",
                d.name
            ),
            key: Some(d.key.clone()),
            task_definition_arn: format!("arn:aws:ecs:::task-definition/{}:1", d.family),
            task_definition_hash: hash.to_string(),
            desired_count: count,
            running_count: count,
            deployment_id: Some(d.tags.deployment_id.clone()),
            project: d.tags.project.clone(),
            deployment_group: d.tags.deployment_group.clone(),
            rollout_started_at: None,
        }
    }

    fn rollout_tests_now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-08-26T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    #[test]
    fn an_outage_is_never_held_back() {
        // Nothing of the outgoing revision is serving, so "not ready" is the
        // truth about the deployment and must be reported.
        assert!(!hold_status_during_rollout(
            false,
            true,
            Some(rollout_tests_now()),
            rollout_tests_now(),
            chrono::Duration::minutes(5),
        ));
    }

    #[test]
    fn the_tick_that_starts_the_roll_holds() {
        // ECS still reports the outgoing revision as PRIMARY here, so the
        // timestamp alone would not recognise the roll.
        let long_ago = rollout_tests_now() - chrono::Duration::days(3);
        assert!(hold_status_during_rollout(
            true,
            true,
            Some(long_ago),
            rollout_tests_now(),
            chrono::Duration::minutes(5),
        ));
    }

    #[test]
    fn a_roll_in_progress_holds_until_the_window_expires() {
        let now = rollout_tests_now();
        let window = chrono::Duration::minutes(5);
        assert!(hold_status_during_rollout(
            true,
            false,
            Some(now - chrono::Duration::minutes(4)),
            now,
            window,
        ));
        // Past the window the rollout is not "in progress", it is stuck, and a
        // project serving superseded content should say so.
        assert!(!hold_status_during_rollout(
            true,
            false,
            Some(now - chrono::Duration::minutes(6)),
            now,
            window,
        ));
    }

    #[test]
    fn no_known_rollout_start_does_not_hold() {
        assert!(!hold_status_during_rollout(
            true,
            false,
            None,
            rollout_tests_now(),
            chrono::Duration::minutes(5),
        ));
    }

    #[test]
    fn missing_service_is_created() {
        let d = desired("dep-a", "h1", 1);
        let actions = diff_services(std::slice::from_ref(&d), &[], &HashSet::new());
        assert_eq!(
            actions,
            vec![ServiceAction::Create {
                key: d.key,
                name: d.name
            }]
        );
    }

    #[test]
    fn a_converged_service_produces_no_action() {
        // The most important property of any reconciler: steady state is quiet.
        // A spurious action here would re-register a task definition every tick
        // and exhaust the 1/s RegisterTaskDefinition budget.
        let d = desired("dep-a", "h1", 2);
        let a = actual_for(&d, "h1", 2);
        assert!(diff_services(&[d], &[a], &HashSet::new()).is_empty());
    }

    #[test]
    fn content_drift_updates_the_task_definition_rather_than_recreating() {
        // The structural win over the Docker model: ECS rolls the replacement
        // itself, so a changed image or env never destroys the running service.
        let d = desired("dep-a", "h2", 1);
        let a = actual_for(&d, "h1", 1);
        let actions = diff_services(std::slice::from_ref(&d), &[a], &HashSet::new());
        assert_eq!(
            actions,
            vec![ServiceAction::UpdateTaskDefinition {
                key: d.key,
                name: d.name
            }]
        );
    }

    #[test]
    fn scale_only_change_avoids_a_task_definition_revision() {
        // Scaling must not churn revisions: RegisterTaskDefinition sustains only
        // 1 request/second, so needless revisions throttle real deploys.
        let d = desired("dep-a", "h1", 3);
        let a = actual_for(&d, "h1", 1);
        let actions = diff_services(std::slice::from_ref(&d), &[a], &HashSet::new());
        assert_eq!(
            actions,
            vec![ServiceAction::UpdateDesiredCount {
                key: d.key,
                name: d.name,
                desired_count: 3
            }]
        );
    }

    #[test]
    fn content_drift_takes_precedence_over_scale() {
        // Both changed: the new revision must carry the new count, not race a
        // separate scale call against the rollout.
        let d = desired("dep-a", "h2", 5);
        let a = actual_for(&d, "h1", 1);
        let actions = diff_services(&[d], &[a], &HashSet::new());
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            actions[0],
            ServiceAction::UpdateTaskDefinition { .. }
        ));
    }

    #[test]
    fn a_changed_subnet_set_produces_a_shape_update() {
        // The operator moved the workload subnets. Nothing about the task
        // changed, so this must not register a revision -- but it must be
        // applied, which before this existed it never was.
        let mut d = desired("dep-a", "h1", 1);
        let a = actual_for(&d, "h1", 1);
        d.shape.subnets = ["subnet-c".to_string()].into();

        let actions = diff_services(&[d], &[a], &HashSet::new());
        assert_eq!(actions.len(), 1);
        assert!(
            matches!(actions[0], ServiceAction::UpdateServiceShape { .. }),
            "{:?}",
            actions[0]
        );
    }

    #[test]
    fn subnets_in_a_different_order_are_not_drift() {
        // ECS does not preserve the order subnets were sent in. Comparing them
        // as sequences would report drift on every tick and roll the fleet
        // forever, so the comparison is set-based.
        let d = desired("dep-a", "h1", 1);
        let mut a = actual_for(&d, "h1", 1);
        let observed = a.shape.as_mut().expect("test fixture has a shape");
        assert_eq!(
            observed.subnets, d.shape.subnets,
            "fixture must start converged"
        );
        // A BTreeSet already normalises order; assert the desired-vs-observed
        // comparison itself is order-free rather than the container type.
        observed.subnets = ["subnet-b".to_string(), "subnet-a".to_string()].into();

        assert!(d.shape.network_matches(observed));
        assert!(diff_services(&[d], &[a], &HashSet::new()).is_empty());
    }

    #[test]
    fn a_changed_security_group_or_public_ip_is_also_shape_drift() {
        for mutate in [
            (|d: &mut DesiredService| d.shape.security_groups = ["sg-2".to_string()].into())
                as fn(&mut DesiredService),
            |d: &mut DesiredService| d.shape.assign_public_ip = true,
        ] {
            let mut d = desired("dep-a", "h1", 1);
            let a = actual_for(&d, "h1", 1);
            mutate(&mut d);
            let actions = diff_services(&[d], &[a], &HashSet::new());
            assert_eq!(actions.len(), 1);
            assert!(matches!(
                actions[0],
                ServiceAction::UpdateServiceShape { .. }
            ));
        }
    }

    #[test]
    fn content_drift_still_outranks_shape_drift() {
        // The task-definition update carries the network configuration anyway,
        // so doing both would be two calls for one convergence.
        let mut d = desired("dep-a", "h2", 1);
        let a = actual_for(&d, "h1", 1);
        d.shape.subnets = ["subnet-c".to_string()].into();

        let actions = diff_services(&[d], &[a], &HashSet::new());
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            actions[0],
            ServiceAction::UpdateTaskDefinition { .. }
        ));
    }

    #[test]
    fn shape_drift_outranks_scale_drift_and_carries_the_count() {
        // The scale-only call omits the network configuration entirely, so
        // taking that branch would leave the subnets stale until something else
        // happened to touch the service.
        let mut d = desired("dep-a", "h1", 4);
        let a = actual_for(&d, "h1", 1);
        d.shape.subnets = ["subnet-c".to_string()].into();

        let actions = diff_services(&[d], &[a], &HashSet::new());
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            ServiceAction::UpdateServiceShape { desired_count, .. } => {
                assert_eq!(*desired_count, 4, "the count must ride along");
            }
            other => panic!("expected a shape update, got {other:?}"),
        }
    }

    #[test]
    fn an_unreadable_shape_is_never_treated_as_drift() {
        // A service ECS reports without a network configuration must be left
        // alone. "Unknown" is not "wrong", and correcting it on the strength of
        // a missing field would roll the service for nothing.
        let mut d = desired("dep-a", "h1", 1);
        let mut a = actual_for(&d, "h1", 1);
        a.shape = None;
        d.shape.subnets = ["subnet-c".to_string()].into();

        assert!(diff_services(&[d], &[a], &HashSet::new()).is_empty());
    }

    #[test]
    fn a_converged_service_is_still_silent() {
        // The steady state must stay a no-op now that there is a third thing to
        // compare -- otherwise every install rolls every service every tick.
        let d = desired("dep-a", "h1", 1);
        let a = actual_for(&d, "h1", 1);
        assert!(diff_services(&[d], &[a], &HashSet::new()).is_empty());
    }

    #[test]
    fn a_capacity_change_is_reported_but_produces_no_action() {
        // ECS restricts which capacity transitions UpdateService performs, and
        // the permitted ones need a forced new deployment. Attempting it every
        // tick would either roll the fleet or fail the same call forever; the
        // next deploy creates fresh services on the new capacity.
        let mut d = desired("dep-a", "h1", 1);
        let a = actual_for(&d, "h1", 1);
        d.shape.capacity = Capacity::Ec2;

        assert!(
            diff_services(&[d.clone()], std::slice::from_ref(&a), &HashSet::new()).is_empty(),
            "capacity must not become an in-place action"
        );

        let drift = capacity_drift(&[d], &[a]);
        assert_eq!(drift.len(), 1);
        assert_eq!(drift[0].desired, "launch type EC2");
        assert_eq!(drift[0].observed, "launch type FARGATE");
    }

    #[test]
    fn a_converged_capacity_reports_no_drift() {
        let d = desired("dep-a", "h1", 1);
        let a = actual_for(&d, "h1", 1);
        assert!(capacity_drift(&[d], &[a]).is_empty());
    }

    #[test]
    fn capacity_drift_ignores_a_service_with_no_readable_shape() {
        let d = desired("dep-a", "h1", 1);
        let mut a = actual_for(&d, "h1", 1);
        a.shape = None;
        assert!(capacity_drift(&[d], &[a]).is_empty());
    }

    #[test]
    fn an_orphan_is_deleted_and_ordered_after_creates() {
        // During a cutover the incoming service must exist before the outgoing
        // one is retired, or the group loses all capacity for a moment.
        let old = desired("dep-a", "h1", 1);
        let new = desired("dep-b", "h1", 1);
        let actions = diff_services(
            std::slice::from_ref(&new),
            &[actual_for(&old, "h1", 1)],
            &HashSet::new(),
        );
        assert_eq!(
            actions,
            vec![
                ServiceAction::Create {
                    key: new.key,
                    name: new.name
                },
                ServiceAction::Delete { name: old.name },
            ]
        );
    }

    #[test]
    fn a_protected_deployments_service_is_never_deleted() {
        // THE critical safety property. When desired-state computation fails for
        // a deployment (KMS blip, unreadable row), its services look orphaned.
        // Deleting them would take a healthy production app offline because of a
        // transient control-plane error.
        let live = desired("dep-a", "h1", 1);
        let protected: HashSet<String> = ["dep-a".to_string()].into_iter().collect();
        let actions = diff_services(&[], &[actual_for(&live, "h1", 1)], &protected);
        assert!(
            actions.is_empty(),
            "a protected deployment's service must survive: {actions:?}"
        );
    }

    #[test]
    fn an_unprotected_orphan_is_still_collected_alongside_a_protected_one() {
        // The guard must be surgical: one failing deployment must not suspend GC
        // for the whole cluster, or retired services accumulate against the
        // 5,000-per-cluster quota.
        let protected_dep = desired("dep-a", "h1", 1);
        let orphan = desired("dep-b", "h1", 1);
        let protected: HashSet<String> = ["dep-a".to_string()].into_iter().collect();
        let actions = diff_services(
            &[],
            &[
                actual_for(&protected_dep, "h1", 1),
                actual_for(&orphan, "h1", 1),
            ],
            &protected,
        );
        assert_eq!(actions, vec![ServiceAction::Delete { name: orphan.name }]);
    }

    #[test]
    fn a_service_with_unparseable_tags_is_treated_as_an_orphan_not_a_match() {
        // Without an identity we cannot claim it as ours. It must never satisfy a
        // desired entry (which would leave the real service uncreated).
        let d = desired("dep-a", "h1", 1);
        let mut untagged = actual_for(&d, "h1", 1);
        untagged.key = None;
        untagged.deployment_id = None;
        let actions = diff_services(
            std::slice::from_ref(&d),
            &[untagged.clone()],
            &HashSet::new(),
        );
        assert_eq!(
            actions,
            vec![
                ServiceAction::Create {
                    key: d.key,
                    name: d.name
                },
                ServiceAction::Delete {
                    name: untagged.name
                },
            ]
        );
    }

    #[test]
    fn service_name_carries_the_deployment_id_so_a_cutover_can_overlap() {
        // Two deployments of one group must yield two distinct ECS services, or
        // blue/green collapses into an in-place replacement.
        let a = service_name("rise", "myapp", "default", "20260101-120000", "app");
        let b = service_name("rise", "myapp", "default", "20260101-130000", "app");
        assert_ne!(a, b);
        assert!(a.contains("20260101-120000"));
    }
}

/// How much attention an ECS service message deserves.
///
/// ECS gives no severity of its own — every message is prose — so this reads the
/// wording. Matching on phrases is unavoidable and will drift as AWS rewords
/// things, so the default is `Info`: a message this does not recognise is still
/// forwarded, just without an elevated severity. Losing an alarm is bad;
/// silently dropping the message would be worse.
pub fn narrative_severity(message: &str) -> NarrativeSeverity {
    let m = message.to_ascii_lowercase();

    // Placement failures. The service wants tasks and cannot get them, which is
    // the case the periodic observation cannot explain on its own.
    if m.contains("unable to place a task")
        || m.contains("unable to consistently start tasks successfully")
        || m.contains("was unable to place")
    {
        return NarrativeSeverity::Error;
    }

    // Something was running and stopped being healthy.
    if m.contains("failed elb health checks")
        || m.contains("failed container health checks")
        || m.contains("deregistered") && m.contains("unhealthy")
        || m.contains("has stopped")
    {
        return NarrativeSeverity::Warning;
    }

    NarrativeSeverity::Info
}

/// Severity of one service message, mirroring the event log's own levels
/// without this crate depending on them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NarrativeSeverity {
    Info,
    Warning,
    Error,
}
