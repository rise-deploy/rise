//! The deployment event vocabulary.
//!
//! An event records something that *happened* to a deployment, as distinct from
//! [`DeploymentStatus`](crate::models::DeploymentStatus) and the
//! `controller_metadata` snapshot, which both describe what is true now. A
//! snapshot cannot express a sequence: a container that restarted eleven times
//! carries a counter saying so and nothing saying when any of them happened.
//!
//! This module is the single home of the vocabulary. The database deliberately
//! does not constrain `kind` — a value list in SQL would be a second copy
//! needing a migration per kind.

use std::fmt;

/// What kind of thing happened.
///
/// Status transitions are **one** kind carrying `from`/`to` rather than one kind
/// per transition: a kind-per-transition vocabulary would have to be kept
/// isomorphic to `DeploymentStatus` by hand, and would be the third place the
/// state machine is mirrored after `state_machine.rs` and the SQL
/// `is_valid_transition`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKind {
    /// A deployment status transition. `attributes` carries `from`, `to` and an
    /// optional `reason`.
    StatusChanged,
    /// A container began running.
    ReplicaStarted,
    /// A container's run ended. `attributes` carries `exit_code` and `reason`.
    ReplicaTerminated,
    /// A container's restart counter advanced. `attributes` carries `from` and
    /// `to` counts, so a jump of more than one stays visible.
    ReplicaRestarted,
    /// The desired replica count changed.
    Scaled,
    /// A runtime-native event passed through. `attributes` carries whatever the
    /// backend supplied, including an upstream `count` where the source already
    /// aggregated repeats (Kubernetes `Event` does).
    BackendEvent,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::StatusChanged => "status_changed",
            Self::ReplicaStarted => "replica_started",
            Self::ReplicaTerminated => "replica_terminated",
            Self::ReplicaRestarted => "replica_restarted",
            Self::Scaled => "scaled",
            Self::BackendEvent => "backend_event",
        }
    }

    /// Parse a stored value. Unknown kinds are rejected rather than mapped to a
    /// catch-all: a reader that does not understand a kind should say so, not
    /// silently mis-file it.
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "status_changed" => Self::StatusChanged,
            "replica_started" => Self::ReplicaStarted,
            "replica_terminated" => Self::ReplicaTerminated,
            "replica_restarted" => Self::ReplicaRestarted,
            "scaled" => Self::Scaled,
            "backend_event" => Self::BackendEvent,
            _ => return None,
        })
    }

    /// Whether events of this kind carry a dedupe key.
    ///
    /// Only observation-derived kinds do. A repeated status transition is
    /// legitimate — `Healthy → Unhealthy → Healthy` is two real
    /// `became healthy` moments — so collapsing them would lose information.
    pub fn is_deduplicated(self) -> bool {
        !matches!(self, Self::StatusChanged | Self::Scaled)
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The attribute vocabulary: what a `status_changed` event may carry, and what
/// each key means.
///
/// This module is the single place that decides an attribute's *meaning*. It is
/// deliberately not an allowlist — readers render whatever an event carries, so
/// a backend can report something described here later, or not at all, without
/// anything downstream having to agree first.
///
/// **Naming is load-bearing.** Readers format by the key's suffix rather than by
/// consulting a table, so a duration must end `_ms`, a size `_bytes`, and an
/// instant `_at`. A key named `build_time` renders as a bare number; a key named
/// `build_ms` renders as `4.0s` with nothing added anywhere.
pub mod attributes {
    /// The status moved from, absent on the creation event — nothing preceded it.
    pub const FROM: &str = "from";
    /// The status moved to. Always present.
    pub const TO: &str = "to";
    /// Why, in words, when the mover can say. Rendered beside the transition.
    pub const REASON: &str = "reason";

    /// Keys the transition itself owns. A reporter cannot set these: they are
    /// applied last so that what actually happened cannot be overwritten by
    /// what a reporter claims happened.
    pub const RESERVED: &[&str] = &[FROM, TO, REASON];

    /// Email of whoever asked for the deployment. Creation only.
    pub const CREATED_BY: &str = "created_by";
    /// Deployment group, replica count and resource ask, as requested at
    /// creation. Recorded because the row can be edited afterwards; the event
    /// keeps what was asked for.
    pub const GROUP: &str = "group";
    pub const REPLICAS: &str = "replicas";
    pub const CPU: &str = "cpu";
    pub const MEMORY: &str = "memory";
    /// Declared container names, and the image the deployment was created with.
    pub const CONTAINERS: &str = "containers";
    pub const IMAGE: &str = "image";
    /// The deployment this one was rolled back from, by its `deployment_id`.
    pub const ROLLED_BACK_FROM: &str = "rolled_back_from";
    /// CI provenance, when the caller supplied it.
    pub const JOB_URL: &str = "job_url";
    pub const PULL_REQUEST_URL: &str = "pull_request_url";

    /// The deployment that replaced this one, by its `deployment_id` — the
    /// human name, not the UUID, because it is what a reader follows.
    ///
    /// Known when the deployment is marked `Terminating` (the replacement is in
    /// hand there) and carried onto `Superseded`, which is the row a reader
    /// actually lands on.
    pub const SUPERSEDED_BY: &str = "superseded_by";

    /// Registry the images were pushed to, and the per-image breakdown: a list
    /// of objects, one per container, each carrying `container`, `image`,
    /// `build_method`, `build_ms` and — when a separate push makes it
    /// observable — `push_ms`.
    ///
    /// Reported by the CLI, which is the only thing that observes the build.
    pub const REGISTRY: &str = "registry";
    pub const IMAGES: &str = "images";
}

/// How much attention an occurrence deserves.
///
/// A property of the occurrence, not of the kind: the same
/// [`EventKind::ReplicaTerminated`] is routine during a rollout and an error on
/// a non-zero exit, so the emitting backend decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EventSeverity {
    /// Detail that is useful mid-incident and noise otherwise — a health-status
    /// transition every thirty seconds, a probe flapping inside its threshold.
    /// Below the default read floor.
    Debug,
    Info,
    Warning,
    Error,
}

impl EventSeverity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "debug" => Self::Debug,
            "info" => Self::Info,
            "warning" => Self::Warning,
            "error" => Self::Error,
            _ => return None,
        })
    }
}

impl fmt::Display for EventSeverity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which component observed the event. Recorded so a parity gap is legible in
/// the data rather than only in documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventSource {
    ControlPlane,
    Kubernetes,
    Docker,
    Ecs,
}

impl EventSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ControlPlane => "control-plane",
            Self::Kubernetes => "kubernetes",
            Self::Docker => "docker",
            Self::Ecs => "ecs",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "control-plane" => Self::ControlPlane,
            "kubernetes" => Self::Kubernetes,
            "docker" => Self::Docker,
            "ecs" => Self::Ecs,
            _ => return None,
        })
    }
}

impl fmt::Display for EventSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_KINDS: &[EventKind] = &[
        EventKind::StatusChanged,
        EventKind::ReplicaStarted,
        EventKind::ReplicaTerminated,
        EventKind::ReplicaRestarted,
        EventKind::Scaled,
        EventKind::BackendEvent,
    ];

    const ALL_SEVERITIES: &[EventSeverity] = &[
        EventSeverity::Debug,
        EventSeverity::Info,
        EventSeverity::Warning,
        EventSeverity::Error,
    ];

    const ALL_SOURCES: &[EventSource] = &[
        EventSource::ControlPlane,
        EventSource::Kubernetes,
        EventSource::Docker,
        EventSource::Ecs,
    ];

    #[test]
    fn every_kind_round_trips_through_its_stored_form() {
        for kind in ALL_KINDS {
            assert_eq!(EventKind::parse(kind.as_str()), Some(*kind));
        }
    }

    #[test]
    fn every_severity_round_trips_and_matches_the_database_constraint() {
        // These strings are also the CHECK on deployment_events.severity; a
        // rename here without a migration would fail every insert.
        let expected = ["debug", "info", "warning", "error"];
        for (severity, name) in ALL_SEVERITIES.iter().zip(expected) {
            assert_eq!(severity.as_str(), name);
            assert_eq!(EventSeverity::parse(name), Some(*severity));
        }
    }

    #[test]
    fn every_source_round_trips_through_its_stored_form() {
        for source in ALL_SOURCES {
            assert_eq!(EventSource::parse(source.as_str()), Some(*source));
        }
    }

    #[test]
    fn unknown_values_are_rejected_rather_than_mapped_to_a_catch_all() {
        assert_eq!(EventKind::parse("became_healthy"), None);
        assert_eq!(EventSeverity::parse("critical"), None);
        assert_eq!(EventSource::parse("nomad"), None);
    }

    #[test]
    fn severity_orders_from_quietest_to_loudest() {
        // The read API filters with `>=`, so the ordering is load-bearing.
        assert!(EventSeverity::Debug < EventSeverity::Info);
        assert!(EventSeverity::Info < EventSeverity::Warning);
        assert!(EventSeverity::Warning < EventSeverity::Error);
    }

    #[test]
    fn only_observation_kinds_are_deduplicated() {
        assert!(!EventKind::StatusChanged.is_deduplicated());
        assert!(!EventKind::Scaled.is_deduplicated());
        assert!(EventKind::ReplicaStarted.is_deduplicated());
        assert!(EventKind::ReplicaTerminated.is_deduplicated());
        assert!(EventKind::ReplicaRestarted.is_deduplicated());
        assert!(EventKind::BackendEvent.is_deduplicated());
    }
}
