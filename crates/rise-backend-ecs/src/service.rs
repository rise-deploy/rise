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

use std::collections::{HashMap, HashSet};

use rise_backend_core::naming::sanitize_ecs_name;
use rise_backend_core::spec_key;

use crate::tags::ServiceTags;

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
}

/// What the reconciler should do about one service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceAction {
    /// Register the task definition and create the service.
    Create { key: String, name: String },
    /// Register a new task-definition revision and point the service at it.
    /// ECS performs the rolling replacement.
    UpdateTaskDefinition { key: String, name: String },
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
        ServiceAction::UpdateDesiredCount { name, .. } => (2, name.clone()),
        ServiceAction::Delete { name } => (3, name.clone()),
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
    use super::*;

    fn tags(deployment_id: &str) -> ServiceTags {
        ServiceTags {
            project: "myapp".to_string(),
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

    fn desired(deployment_id: &str, hash: &str, count: i32) -> DesiredService {
        DesiredService {
            name: service_name("rise", "myapp", "default", deployment_id, "app"),
            key: service_key("myapp", "default", deployment_id, "app"),
            family: "rise-myapp-default-app".to_string(),
            task_definition_hash: hash.to_string(),
            desired_count: count,
            tags: tags(deployment_id),
        }
    }

    fn actual_for(d: &DesiredService, hash: &str, count: i32) -> ActualService {
        ActualService {
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
        }
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
