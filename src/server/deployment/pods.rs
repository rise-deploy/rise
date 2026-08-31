//! What Kubernetes says about a deployment's pods: their observed state, and
//! the Events that explain it.
//!
//! The reason this exists is `FailedScheduling`. A pod that cannot be placed
//! stays `Pending` indefinitely, and the pod's own status says only that — the
//! *why* ("0/3 nodes are available: insufficient memory") lives solely in an
//! Event. The same is true of image-pull failures and probe failures: the
//! observation records that a replica is not ready, and only these say what
//! would fix it.
//!
//! Forwarded, not trusted as the source of truth. Events expire (the default
//! TTL is an hour) and a sync can miss them, so the periodic pod observation
//! remains the thing that decides a deployment's status.

use k8s_openapi::api::core::v1::{Event as K8sEvent, Pod};
use rise_backend_core::events::{EventSeverity, ForwardedEvent};
use rise_backend_core::observation::{ContainerObservation, ObservedState};

/// Event reasons worth a deployment's timeline, and how loudly.
///
/// An allowlist rather than everything, because a namespace's Event stream
/// includes a great deal that says nothing about whether the app works. Each
/// entry earns its place by answering a question the pod's own status cannot.
fn severity_for(reason: &str) -> Option<EventSeverity> {
    Some(match reason {
        // Nothing will run until this is resolved, and the message is the only
        // place the cause appears.
        "FailedScheduling"
        | "FailedCreatePodSandBox"
        | "FailedMount"
        | "FailedAttachVolume"
        | "Evicted"
        | "Preempted"
        | "NodeNotReady" => EventSeverity::Error,

        // The image will never arrive, or the container will not stay up.
        "Failed" | "BackOff" | "ErrImageNeverPull" | "InspectFailed" => EventSeverity::Error,

        // Running, but the runtime is unhappy with it.
        "Unhealthy" | "ProbeWarning" | "OOMKilling" | "Killing" => EventSeverity::Warning,

        // Ordinary rollout progress. Kept at debug so it is available when
        // someone is looking closely and invisible otherwise — the timeline's
        // default floor is info.
        "Scheduled" | "Pulling" | "Pulled" | "Created" | "Started" => EventSeverity::Debug,

        _ => return None,
    })
}

/// Project the Events of one deployment's pods onto forwardable events.
///
/// The dedupe key is the Event's uid *and* its count: Kubernetes aggregates
/// repeats into one object with a rising `count`, so keying on uid alone would
/// record the first occurrence and silently ignore every later one, while
/// keying on nothing would re-record the same object on every sync.
pub fn forwardable(events: &[K8sEvent]) -> Vec<ForwardedEvent> {
    events
        .iter()
        .filter_map(|e| {
            let reason = e.reason.as_deref()?;
            let severity = severity_for(reason)?;
            let uid = e.metadata.uid.as_deref()?;
            let count = e.count.unwrap_or(1);

            // The most recent occurrence is the one worth dating: an Event
            // aggregated over ten minutes should appear when it last happened,
            // not when it first did.
            let occurred_at = e
                .last_timestamp
                .as_ref()
                .or(e.first_timestamp.as_ref())
                .and_then(|t| to_chrono(t.0))
                .or_else(|| e.event_time.as_ref().and_then(|t| to_chrono(t.0)))?;

            let mut attributes = serde_json::json!({ "reason": reason });
            if count > 1 {
                // Kubernetes has already aggregated the repeats, so the count
                // comes along rather than being re-derived.
                attributes["count"] = serde_json::json!(count);
            }
            if let Some(component) = e.reporting_component.as_deref().filter(|c| !c.is_empty()) {
                attributes["reported_by"] = serde_json::json!(component);
            }

            Some(ForwardedEvent {
                dedupe_key: format!("{uid}:{count}"),
                occurred_at,
                severity,
                message: e.message.clone().unwrap_or_else(|| reason.to_string()),
                // The pod the Event is about, which is the subject Kubernetes
                // observations use.
                subject: e.involved_object.name.clone(),
                attributes,
            })
        })
        .collect()
}

/// `k8s-openapi` reports times as `jiff::Timestamp`; the event log stores
/// `chrono`. Converted through the epoch, which both agree on.
fn to_chrono(t: k8s_openapi::jiff::Timestamp) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::from_timestamp(t.as_second(), t.subsec_nanosecond().unsigned_abs())
}

/// Project a pod onto the shared observation shape.
///
/// The subject is the pod name. A Deployment's pods carry random suffixes, so
/// there is no stable ordinal to key on — and a replaced pod genuinely is a
/// different pod, which is exactly what a new subject says.
///
/// One observation per pod rather than per container: Rise runs one app
/// container per pod, and the pod is what Kubernetes schedules, restarts and
/// reports on.
pub fn observe(pod: &Pod, container: &str) -> Option<ContainerObservation> {
    let name = pod.metadata.name.clone()?;
    let status = pod.status.as_ref();

    // The app container's status, by name where Rise named it, else the first —
    // a pod Rise created has exactly one.
    let cs = status
        .and_then(|s| s.container_statuses.as_ref())
        .and_then(|all| {
            all.iter()
                .find(|c| c.name == container)
                .or_else(|| all.first())
        });

    // A pod being deleted is on its way out whatever its containers say, and
    // reporting it as running would keep it in the timeline as live.
    let terminating = pod.metadata.deletion_timestamp.is_some();
    let state = if terminating {
        ObservedState::Exited
    } else {
        match status.and_then(|s| s.phase.as_deref()) {
            Some("Running") => ObservedState::Running,
            Some("Pending") => ObservedState::Pending,
            Some("Succeeded") | Some("Failed") => ObservedState::Exited,
            _ => ObservedState::Unknown,
        }
    };

    // `state` is the current run; `last_state` is the previous one. A container
    // that is running now but crashed before takes its exit from `last_state`,
    // which is the only record of why it restarted.
    let current = cs.and_then(|c| c.state.as_ref());
    let terminated = current
        .and_then(|s| s.terminated.as_ref())
        .or_else(|| cs.and_then(|c| c.last_state.as_ref())?.terminated.as_ref());

    Some(ContainerObservation {
        subject: name.clone(),
        container: container.to_string(),
        // A pod is its own instance: it is never reused, so a replacement is a
        // new subject rather than a new incarnation of this one.
        instance: Some(name),
        replica: None,
        state,
        started_at: current
            .and_then(|s| s.running.as_ref())
            .and_then(|r| r.started_at.as_ref())
            .and_then(|t| to_chrono(t.0))
            .or_else(|| {
                terminated
                    .and_then(|t| t.started_at.as_ref())
                    .and_then(|t| to_chrono(t.0))
            }),
        finished_at: terminated
            .and_then(|t| t.finished_at.as_ref())
            .and_then(|t| to_chrono(t.0)),
        // The exit of *this* run. A container running now has not exited, and
        // reporting the previous run's code here would read as though it had —
        // `reason` still carries why it restarted.
        exit_code: (state == ObservedState::Exited)
            .then(|| terminated.map(|t| t.exit_code as i64))
            .flatten(),
        // Kubernetes keeps a real counter, so an in-place restart is visible
        // without comparing pod identities.
        restart_count: cs.map(|c| c.restart_count as i64),
        health: cs.map(|c| if c.ready { "ready" } else { "not-ready" }.to_string()),
        reason: terminated
            .and_then(|t| t.reason.clone())
            .or_else(|| current.and_then(|s| s.waiting.as_ref())?.reason.clone()),
        image: cs.map(|c| c.image.clone()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{
        ContainerState, ContainerStateRunning, ContainerStateTerminated, ContainerStatus, PodStatus,
    };

    fn pod(name: &str, phase: &str) -> Pod {
        Pod {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                ..Default::default()
            },
            status: Some(PodStatus {
                phase: Some(phase.to_string()),
                container_statuses: Some(vec![ContainerStatus {
                    name: "web".to_string(),
                    ready: phase == "Running",
                    restart_count: 0,
                    image: "registry.test/web:v1".to_string(),
                    state: Some(ContainerState {
                        running: (phase == "Running")
                            .then_some(ContainerStateRunning { started_at: None }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// The pod name is the subject, and it is its own instance: Kubernetes never
    /// reuses a pod, so a replacement is a new subject rather than a new
    /// incarnation of the old one.
    #[test]
    fn a_pod_is_its_own_subject_and_instance() {
        let o = observe(&pod("web-abc123", "Running"), "web").unwrap();
        assert_eq!(o.subject, "web-abc123");
        assert_eq!(o.instance.as_deref(), Some("web-abc123"));
        assert_eq!(
            o.replica, None,
            "a Deployment's pods have no stable ordinal"
        );
        assert_eq!(o.state, ObservedState::Running);
        assert_eq!(o.restart_count, Some(0));
    }

    #[test]
    fn pod_phases_map_onto_the_shared_vocabulary() {
        for (phase, expected) in [
            ("Running", ObservedState::Running),
            ("Pending", ObservedState::Pending),
            ("Succeeded", ObservedState::Exited),
            ("Failed", ObservedState::Exited),
            ("Weird", ObservedState::Unknown),
        ] {
            let o = observe(&pod("p", phase), "web").unwrap();
            assert_eq!(o.state, expected, "{phase}");
        }
    }

    /// A pod being deleted is on its way out whatever its containers still say.
    /// Reporting it as running would keep a terminating replica in the timeline
    /// as live.
    #[test]
    fn a_terminating_pod_is_exited_whatever_its_phase_says() {
        let mut p = pod("web-abc", "Running");
        p.metadata.deletion_timestamp = Some(Time(
            k8s_openapi::jiff::Timestamp::from_second(1_800_000_000).unwrap(),
        ));
        assert_eq!(observe(&p, "web").unwrap().state, ObservedState::Exited);
    }

    /// A container running now may have crashed before, and `last_state` is the
    /// only record of why it restarted — an OOM kill is invisible otherwise.
    ///
    /// The *reason* carries over; the exit code does not. This run has not
    /// ended, and reporting the previous run's code as `exit_code` would read as
    /// though it had.
    #[test]
    fn a_previous_termination_supplies_the_reason_but_not_this_runs_exit() {
        let mut p = pod("web-abc", "Running");
        if let Some(cs) = p
            .status
            .as_mut()
            .and_then(|s| s.container_statuses.as_mut())
            .and_then(|all| all.first_mut())
        {
            cs.restart_count = 3;
            cs.last_state = Some(ContainerState {
                terminated: Some(ContainerStateTerminated {
                    exit_code: 137,
                    reason: Some("OOMKilled".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            });
        }

        let o = observe(&p, "web").unwrap();
        assert_eq!(o.state, ObservedState::Running);
        assert_eq!(o.exit_code, None, "this run has not ended");
        assert_eq!(o.reason.as_deref(), Some("OOMKilled"));
        assert_eq!(o.restart_count, Some(3));
    }

    /// A pod with no name cannot be identified, and a subject that is not stable
    /// would match the wrong thing on the next tick.
    #[test]
    fn a_nameless_pod_yields_no_observation() {
        let mut p = pod("x", "Running");
        p.metadata.name = None;
        assert!(observe(&p, "web").is_none());
    }

    use k8s_openapi::api::core::v1::ObjectReference;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};

    fn event(reason: &str, uid: &str, count: i32, pod: &str) -> K8sEvent {
        K8sEvent {
            reason: Some(reason.to_string()),
            message: Some(format!("{reason} happened")),
            count: Some(count),
            metadata: ObjectMeta {
                uid: Some(uid.to_string()),
                ..Default::default()
            },
            involved_object: ObjectReference {
                kind: Some("Pod".to_string()),
                name: Some(pod.to_string()),
                ..Default::default()
            },
            last_timestamp: Some(Time(
                k8s_openapi::jiff::Timestamp::from_second(1_800_000_000).unwrap(),
            )),
            ..Default::default()
        }
    }

    /// The reason this module exists: a pod that will not schedule says only
    /// `Pending` in its own status, and the cause lives solely in an Event.
    #[test]
    fn a_scheduling_failure_is_forwarded_as_an_error() {
        let out = forwardable(&[event("FailedScheduling", "u1", 1, "web-abc")]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].severity, EventSeverity::Error);
        assert_eq!(out[0].subject.as_deref(), Some("web-abc"));
        assert_eq!(out[0].attributes["reason"], "FailedScheduling");
    }

    /// A namespace's Event stream carries a great deal that says nothing about
    /// whether the app works. Anything not on the list is dropped rather than
    /// filling the timeline.
    #[test]
    fn unlisted_reasons_are_not_forwarded() {
        assert!(forwardable(&[event("SandboxChanged", "u1", 1, "web-abc")]).is_empty());
        assert!(forwardable(&[event("Something", "u2", 1, "web-abc")]).is_empty());
    }

    /// Kubernetes aggregates repeats into one object with a rising count.
    /// Keying on the uid alone would record the first occurrence and silently
    /// ignore every later one; including the count makes each occurrence new.
    #[test]
    fn the_dedupe_key_advances_with_the_aggregated_count() {
        let first = forwardable(&[event("Unhealthy", "u1", 1, "web-abc")]);
        let later = forwardable(&[event("Unhealthy", "u1", 7, "web-abc")]);
        assert_ne!(first[0].dedupe_key, later[0].dedupe_key);
        assert_eq!(later[0].attributes["count"], 7);
        // A single occurrence carries no count — there is nothing to aggregate.
        assert!(first[0].attributes.get("count").is_none());
    }

    /// Routine rollout progress is kept below the timeline's default floor, so
    /// it is there when someone looks closely and invisible otherwise.
    #[test]
    fn routine_progress_is_debug() {
        for reason in ["Scheduled", "Pulling", "Pulled", "Created", "Started"] {
            let out = forwardable(&[event(reason, "u1", 1, "web-abc")]);
            assert_eq!(out[0].severity, EventSeverity::Debug, "{reason}");
        }
    }

    /// An Event with no timestamp cannot be placed on a timeline, and guessing
    /// one would date it wrongly.
    #[test]
    fn an_undateable_event_is_dropped() {
        let mut e = event("FailedScheduling", "u1", 1, "web-abc");
        e.last_timestamp = None;
        e.first_timestamp = None;
        e.event_time = None;
        assert!(forwardable(&[e]).is_empty());
    }
}
