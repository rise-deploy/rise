//! Kubernetes `Event` objects, forwarded into a deployment's log.
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

use k8s_openapi::api::core::v1::Event as K8sEvent;
use rise_backend_core::events::{EventSeverity, ForwardedEvent};

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

#[cfg(test)]
mod tests {
    use super::*;
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
