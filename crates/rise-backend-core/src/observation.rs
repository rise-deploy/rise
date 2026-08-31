//! What a backend saw of one container replica, and what changed since last time.
//!
//! Every runtime describes a running container differently — Docker inspects a
//! container, Kubernetes reads a pod's container statuses, ECS describes a task.
//! This module is the one shape they all reduce to, so that deriving "this
//! replica restarted" is a single pure function rather than three.
//!
//! The shape is deliberately *not* modelled on any one runtime. A previous
//! design reused a Kubernetes-flavoured snapshot that existed to render a UI
//! tab, which made a presentation format load-bearing for correctness. What
//! belongs here is only what a reader of a deployment's history needs, and only
//! what more than one runtime can actually answer.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::events::{EventKind, EventSeverity};

/// The lifecycle of a replica, reduced to the states every runtime can express.
///
/// Runtime-specific detail (Docker's `restarting`, Kubernetes' `CrashLoopBackOff`,
/// ECS' `DEPROVISIONING`) belongs in [`ContainerObservation::reason`], not here:
/// a state this enum does not have is a state the derivation cannot reason
/// about, so adding one is a decision about behaviour, not vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedState {
    /// Accepted by the runtime, not yet running — pulling, scheduling, starting.
    Pending,
    /// The process is up. Says nothing about whether it is *healthy*; that is
    /// [`ContainerObservation::health`].
    Running,
    /// The process ran and stopped, for any reason.
    Exited,
    /// Observed, but the runtime reported something this does not model. Kept
    /// rather than guessed at: a wrong state derives wrong events.
    Unknown,
}

impl ObservedState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Exited => "exited",
            Self::Unknown => "unknown",
        }
    }
}

/// One replica, as one backend saw it at one moment.
///
/// `subject` is the backend's own stable handle, and the backends genuinely
/// differ — see [`ContainerObservation::subject`]. Nothing here assumes a
/// replica ordinal exists, because on two of the three backends it does not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerObservation {
    /// The stable handle this backend has for this replica, and the key the
    /// derivation matches on across ticks.
    ///
    /// - **Docker** — `web[0]`, from the replica index it stores in a label.
    ///   Stable across recreates of the same slot.
    /// - **Kubernetes** — the pod name. Stable for the pod's life; a replaced
    ///   pod is a new subject, because it is a new pod.
    /// - **ECS** — the task id. Stable for the task's life. Fargate replaces
    ///   tasks rather than restarting containers, so a replacement is a new
    ///   subject there too.
    ///
    /// The consequence is deliberate: on Kubernetes and ECS a replacement reads
    /// as one subject ending and another beginning, which is what actually
    /// happened. Forcing a shared ordinal would report a replica "restarting"
    /// when two unrelated tasks merely changed places in a list.
    pub subject: String,

    /// The declared container this is an instance of.
    pub container: String,

    /// The runtime's identity for the *current incarnation* of this subject,
    /// where the two differ.
    ///
    /// On Docker they do: `subject` is a slot that survives recreates, while
    /// the container filling it is replaced wholesale — and a replacement
    /// starts its restart counter at zero, so a recreate is otherwise
    /// indistinguishable from nothing happening. The container name carries the
    /// generation, so a change here is a replacement.
    ///
    /// On Kubernetes and ECS the subject already *is* the instance (a pod, a
    /// task), so this equals `subject` and the comparison never fires.
    pub instance: Option<String>,

    /// Replica ordinal, only where the backend has a stable one. `None` on
    /// Kubernetes and ECS, where any ordinal would be positional fiction.
    pub replica: Option<u32>,

    pub state: ObservedState,

    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub exit_code: Option<i64>,

    /// The runtime's own restart counter, where it keeps one (Docker,
    /// Kubernetes). `None` on ECS: Fargate has no counter because it has no
    /// in-place restart — a wedged container becomes a replaced task, which
    /// this model sees as one subject ending and another starting.
    pub restart_count: Option<i64>,

    /// Health-check verdict where the runtime performs one.
    pub health: Option<String>,

    /// Why the replica is in this state, in the runtime's own words —
    /// `OOMKilled`, `ImagePullBackOff`, an ECS `stoppedReason`.
    pub reason: Option<String>,

    /// The image actually running, where the runtime reports it.
    pub image: Option<String>,
}

impl ContainerObservation {
    /// A minimal observation, for backends filling in what they can.
    pub fn new(
        subject: impl Into<String>,
        container: impl Into<String>,
        state: ObservedState,
    ) -> Self {
        Self {
            subject: subject.into(),
            container: container.into(),
            instance: None,
            replica: None,
            state,
            started_at: None,
            finished_at: None,
            exit_code: None,
            restart_count: None,
            health: None,
            reason: None,
            image: None,
        }
    }
}

/// An event the derivation concluded, ready to be written to the log.
#[derive(Debug, Clone, PartialEq)]
pub struct DerivedEvent {
    pub kind: EventKind,
    pub severity: EventSeverity,
    /// The replica it is about — always set, since every derived event
    /// describes one subject.
    pub subject: String,
    pub occurred_at: Option<DateTime<Utc>>,
    pub attributes: BTreeMap<String, serde_json::Value>,
}

impl DerivedEvent {
    fn new(kind: EventKind, severity: EventSeverity, subject: &str) -> Self {
        Self {
            kind,
            severity,
            subject: subject.to_string(),
            occurred_at: None,
            attributes: BTreeMap::new(),
        }
    }

    fn at(mut self, when: Option<DateTime<Utc>>) -> Self {
        self.occurred_at = when;
        self
    }

    fn with(mut self, key: &str, value: serde_json::Value) -> Self {
        if !value.is_null() {
            self.attributes.insert(key.to_string(), value);
        }
        self
    }
}

/// What changed between two observations of the same deployment.
///
/// Matched on [`ContainerObservation::subject`], so this is level-triggered:
/// it compares two states rather than trusting a stream of edges. A runtime
/// event stream can drop on disconnect; two snapshots cannot disagree about
/// what is true now, and a missed tick shows up as a larger delta rather than a
/// lost event.
///
/// `previous` empty means nothing was observed before. That yields a
/// `replica_started` per running replica, which is correct on a deployment's
/// first tick and equally correct after a control-plane restart — the previous
/// observation is reloaded from storage, so "nothing before" really does mean
/// nothing was ever recorded.
pub fn derive_events(
    previous: &[ContainerObservation],
    current: &[ContainerObservation],
    now: DateTime<Utc>,
) -> Vec<DerivedEvent> {
    let before: BTreeMap<&str, &ContainerObservation> =
        previous.iter().map(|o| (o.subject.as_str(), o)).collect();
    let after: BTreeMap<&str, &ContainerObservation> =
        current.iter().map(|o| (o.subject.as_str(), o)).collect();

    let mut events = Vec::new();

    for (subject, now_seen) in &after {
        match before.get(subject) {
            // A subject nobody saw last time. On Docker that is a slot filled
            // for the first time; on Kubernetes and ECS it is also how a
            // replacement announces itself.
            None => {
                if now_seen.state == ObservedState::Running {
                    events.push(started(now_seen).at(now_seen.started_at.or(Some(now))));
                }
            }
            Some(was) => {
                events.extend(transition(was, now_seen, now));
            }
        }
    }

    // A subject that stopped being reported. Its last known state is all there
    // is to say about it, which is exactly why the previous observation is
    // stored rather than recomputed.
    for (subject, was) in &before {
        if after.contains_key(subject) {
            continue;
        }
        events.push(
            terminated(was)
                .at(was.finished_at.or(Some(now)))
                .with("disappeared", serde_json::json!(true)),
        );
    }

    events
}

/// The events implied by one subject changing state between two observations.
fn transition(
    was: &ContainerObservation,
    now_seen: &ContainerObservation,
    now: DateTime<Utc>,
) -> Vec<DerivedEvent> {
    let mut events = Vec::new();

    // The slot is the same but the thing filling it is not: the runtime
    // replaced the container. Checked before the counter, because a replacement
    // resets that counter to zero and would otherwise look like nothing at all.
    if let (Some(before), Some(after)) = (was.instance.as_deref(), now_seen.instance.as_deref()) {
        if before != after {
            return vec![DerivedEvent::new(
                EventKind::ReplicaRestarted,
                EventSeverity::Warning,
                &now_seen.subject,
            )
            .at(now_seen.started_at.or(Some(now)))
            .with("container", serde_json::json!(now_seen.container))
            .with("replaced", serde_json::json!(true))
            .with("reason", json_str(&now_seen.reason))];
        }
    }

    // A counter that advanced is a restart the runtime performed in place, and
    // it is reported even when the state did not change — a container that
    // crash-loops between two ticks is `running` at both ends.
    if let (Some(before), Some(after)) = (was.restart_count, now_seen.restart_count) {
        if after > before {
            let mut event = DerivedEvent::new(
                EventKind::ReplicaRestarted,
                EventSeverity::Warning,
                &now_seen.subject,
            )
            .at(Some(now))
            .with("to", serde_json::json!(after))
            .with("container", serde_json::json!(now_seen.container))
            .with("reason", json_str(&now_seen.reason));

            // Only when more than one restart happened between observations,
            // where it is the sole record that the ones in between occurred.
            if after - before > 1 {
                event = event.with("from", serde_json::json!(before));
            }
            events.push(event);
        }
    }

    match (was.state, now_seen.state) {
        (a, b) if a == b => {}
        (_, ObservedState::Running) => {
            events.push(started(now_seen).at(now_seen.started_at.or(Some(now))));
        }
        (ObservedState::Running, ObservedState::Exited) => {
            events.push(terminated(now_seen).at(now_seen.finished_at.or(Some(now))));
        }
        _ => {}
    }

    events
}

fn started(o: &ContainerObservation) -> DerivedEvent {
    DerivedEvent::new(EventKind::ReplicaStarted, EventSeverity::Info, &o.subject)
        .with("container", serde_json::json!(o.container))
        .with("image", json_str(&o.image))
}

/// A clean exit is information; a non-zero one is a fault. The severity is a
/// property of *this* ending, not of the kind.
fn terminated(o: &ContainerObservation) -> DerivedEvent {
    let severity = match o.exit_code {
        Some(0) | None => EventSeverity::Info,
        Some(_) => EventSeverity::Warning,
    };
    DerivedEvent::new(EventKind::ReplicaTerminated, severity, &o.subject)
        .with("container", serde_json::json!(o.container))
        .with(
            "exit_code",
            o.exit_code.map(serde_json::Value::from).unwrap_or_default(),
        )
        .with("reason", json_str(&o.reason))
}

fn json_str(value: &Option<String>) -> serde_json::Value {
    value
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(serde_json::Value::from)
        .unwrap_or(serde_json::Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
    }

    fn obs(subject: &str, state: ObservedState) -> ContainerObservation {
        ContainerObservation::new(subject, "web", state)
    }

    fn kinds(events: &[DerivedEvent]) -> Vec<(EventKind, &str)> {
        events
            .iter()
            .map(|e| (e.kind, e.subject.as_str()))
            .collect()
    }

    #[test]
    fn first_observation_reports_what_is_already_running() {
        let events = derive_events(&[], &[obs("web[0]", ObservedState::Running)], at(0));
        assert_eq!(kinds(&events), vec![(EventKind::ReplicaStarted, "web[0]")]);
    }

    /// A replica the runtime accepted but has not started yet is not a start.
    #[test]
    fn a_pending_replica_is_not_a_start() {
        let events = derive_events(&[], &[obs("web[0]", ObservedState::Pending)], at(0));
        assert!(events.is_empty(), "{events:?}");
    }

    #[test]
    fn nothing_is_reported_when_nothing_moved() {
        let before = vec![obs("web[0]", ObservedState::Running)];
        let after = vec![obs("web[0]", ObservedState::Running)];
        assert!(derive_events(&before, &after, at(10)).is_empty());
    }

    #[test]
    fn a_replica_that_stopped_is_terminated_with_its_exit() {
        let before = vec![obs("web[0]", ObservedState::Running)];
        let mut stopped = obs("web[0]", ObservedState::Exited);
        stopped.exit_code = Some(137);
        stopped.reason = Some("OOMKilled".to_string());

        let events = derive_events(&before, &[stopped], at(10));
        assert_eq!(
            kinds(&events),
            vec![(EventKind::ReplicaTerminated, "web[0]")]
        );
        assert_eq!(events[0].severity, EventSeverity::Warning);
        assert_eq!(events[0].attributes["exit_code"], 137);
        assert_eq!(events[0].attributes["reason"], "OOMKilled");
    }

    /// A container that exits cleanly is not a fault. A job finishing should not
    /// read the same as a crash.
    #[test]
    fn a_clean_exit_is_not_a_warning() {
        let before = vec![obs("web[0]", ObservedState::Running)];
        let mut stopped = obs("web[0]", ObservedState::Exited);
        stopped.exit_code = Some(0);

        let events = derive_events(&before, &[stopped], at(10));
        assert_eq!(events[0].severity, EventSeverity::Info);
    }

    /// The counter is the only evidence of an in-place restart: a container that
    /// crash-looped between two ticks is `running` at both ends.
    #[test]
    fn a_restart_between_two_ticks_is_caught_by_the_counter_alone() {
        let mut before = obs("web[0]", ObservedState::Running);
        before.restart_count = Some(0);
        let mut after = obs("web[0]", ObservedState::Running);
        after.restart_count = Some(1);

        let events = derive_events(&[before], &[after], at(10));
        assert_eq!(
            kinds(&events),
            vec![(EventKind::ReplicaRestarted, "web[0]")]
        );
        assert_eq!(events[0].attributes["to"], 1);
        assert!(
            !events[0].attributes.contains_key("from"),
            "a single restart makes `from` equal to `to - 1`, which says nothing",
        );
    }

    /// Several restarts inside one tick have no events of their own — nobody
    /// observed them — so the span is the only record they happened.
    #[test]
    fn several_restarts_in_one_tick_report_the_span() {
        let mut before = obs("web[0]", ObservedState::Running);
        before.restart_count = Some(3);
        let mut after = obs("web[0]", ObservedState::Running);
        after.restart_count = Some(7);

        let events = derive_events(&[before], &[after], at(10));
        assert_eq!(events[0].attributes["from"], 3);
        assert_eq!(events[0].attributes["to"], 7);
    }

    /// ECS has no restart counter and replaces tasks instead. A replacement is
    /// therefore one subject ending and a different one beginning — which is
    /// what actually happened, and is inferable without any counter.
    #[test]
    fn ecs_task_replacement_reads_as_a_termination_and_a_start() {
        let mut old = ContainerObservation::new("a1b2c3", "web", ObservedState::Running);
        old.exit_code = Some(1);
        old.reason = Some("Essential container in task exited".to_string());
        let new = ContainerObservation::new("d4e5f6", "web", ObservedState::Running);

        let events = derive_events(&[old], &[new], at(30));
        let mut seen = kinds(&events);
        seen.sort_by_key(|(_, s)| *s);
        assert_eq!(
            seen,
            vec![
                (EventKind::ReplicaTerminated, "a1b2c3"),
                (EventKind::ReplicaStarted, "d4e5f6"),
            ],
        );

        let gone = events.iter().find(|e| e.subject == "a1b2c3").unwrap();
        assert_eq!(gone.attributes["exit_code"], 1);
        assert_eq!(gone.attributes["disappeared"], true);
    }

    /// Scaling down is a termination, not a silence.
    #[test]
    fn a_removed_replica_is_reported() {
        let before = vec![
            obs("web[0]", ObservedState::Running),
            obs("web[1]", ObservedState::Running),
        ];
        let events = derive_events(&before, &[obs("web[0]", ObservedState::Running)], at(10));
        assert_eq!(
            kinds(&events),
            vec![(EventKind::ReplicaTerminated, "web[1]")]
        );
    }

    /// The gap live testing found: Rise recreates a container in the same slot,
    /// so the subject and the state are unchanged and the fresh container's
    /// counter starts at zero. Without the instance, a recreate looks exactly
    /// like nothing having happened.
    #[test]
    fn a_recreated_container_in_the_same_slot_is_a_restart() {
        let mut before = obs("web[0]", ObservedState::Running);
        before.instance = Some("rise_app_web_r0_g1".to_string());
        before.restart_count = Some(0);
        let mut after = obs("web[0]", ObservedState::Running);
        after.instance = Some("rise_app_web_r0_g2".to_string());
        after.restart_count = Some(0);

        let events = derive_events(&[before], &[after], at(10));
        assert_eq!(
            kinds(&events),
            vec![(EventKind::ReplicaRestarted, "web[0]")]
        );
        assert_eq!(events[0].attributes["replaced"], true);
    }

    /// The identity must not change when the replica count does. If a
    /// single-replica container's subject were the bare name, scaling to two
    /// would rename replica 0 and read as a death plus two births.
    #[test]
    fn scaling_up_does_not_rename_the_replica_that_was_already_there() {
        let before = vec![obs("web[0]", ObservedState::Running)];
        let after = vec![
            obs("web[0]", ObservedState::Running),
            obs("web[1]", ObservedState::Running),
        ];

        let events = derive_events(&before, &after, at(10));
        assert_eq!(
            kinds(&events),
            vec![(EventKind::ReplicaStarted, "web[1]")],
            "only the new replica is news",
        );
    }

    /// Where the subject already is the instance — a pod, a task — the check
    /// must never fire on its own.
    #[test]
    fn an_unchanged_instance_reports_nothing() {
        let mut before = obs("pod-abc", ObservedState::Running);
        before.instance = Some("pod-abc".to_string());
        let mut after = obs("pod-abc", ObservedState::Running);
        after.instance = Some("pod-abc".to_string());

        assert!(derive_events(&[before], &[after], at(10)).is_empty());
    }

    /// A restart *and* a state change in one tick are two different facts and
    /// both are reported.
    #[test]
    fn a_restart_that_also_ended_in_an_exit_reports_both() {
        let mut before = obs("web[0]", ObservedState::Running);
        before.restart_count = Some(0);
        let mut after = obs("web[0]", ObservedState::Exited);
        after.restart_count = Some(1);
        after.exit_code = Some(2);

        let events = derive_events(&[before], &[after], at(10));
        let mut seen: Vec<EventKind> = events.iter().map(|e| e.kind).collect();
        seen.sort_by_key(|k| k.as_str());
        assert_eq!(
            seen,
            vec![EventKind::ReplicaRestarted, EventKind::ReplicaTerminated],
        );
    }

    /// An unrecognised runtime state must not be guessed at — deriving from it
    /// would invent transitions that never happened.
    #[test]
    fn an_unknown_state_derives_nothing() {
        let before = vec![obs("web[0]", ObservedState::Running)];
        let after = vec![obs("web[0]", ObservedState::Unknown)];
        assert!(derive_events(&before, &after, at(10)).is_empty());
    }
}
