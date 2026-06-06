//! Pure builders for the deployment's `controller_metadata` JSON: a
//! K8s-pod-status-shaped `pod_status` block plus the legacy sibling `health`
//! block. Pure so they can be unit-tested without a daemon.

use chrono::Utc;

use super::diff::InspectedContainer;
use crate::db::models::DeploymentStatus;

/// Assemble the `controller_metadata` JSON for the deployment: a
/// K8s-pod-status-shaped `pod_status` block (so the Pods tab renders unchanged —
/// see `frontend/src/features/deployments.tsx`) plus the legacy sibling `health`
/// block. `pods` is one (name, inspection) entry PER REPLICA container across all
/// specs (so `desired_replicas` = sum of N over specs, `current_replicas` =
/// running count, `ready_replicas` = healthy count). Pure so it can be
/// unit-tested without a daemon. The container readiness verdict (`is_ready`) is
/// the same one driving the status transitions.
pub(crate) fn build_controller_metadata(
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
pub(crate) fn build_pod_status(
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

#[cfg(test)]
mod tests {
    use super::*;

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
