//! Pure container diff: classify desired vs. observed Rise-managed containers
//! into create / recreate / remove actions, with the stable-identity matching
//! and protected-deployment GC rules. No daemon access, so it is fully
//! unit-testable.

use std::collections::{HashMap, HashSet};

use super::container_builder::{self, DesiredContainer};

/// Observed Rise-managed container, reduced to the fields the diff needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActualContainer {
    pub id: String,
    pub name: String,
    /// The `project` bookkeeping label. Part of the stable identity tuple used
    /// to match this container to a desired one across generations (the live
    /// `--name` carries a per-recreate `_g{n}` suffix and can't be matched
    /// directly). `None` for legacy containers missing the label.
    pub project: Option<String>,
    /// The `deployment-group` bookkeeping label (identity tuple member).
    pub deployment_group: Option<String>,
    /// The `container` bookkeeping label (identity tuple member).
    pub container: Option<String>,
    /// The `deployment-id` bookkeeping label stamped on the live container.
    /// Used by the diff to protect a container from GC when its owning
    /// deployment's desired computation failed this tick, and as a member of the
    /// stable identity tuple. `None` for legacy containers created before this
    /// label was read (treated as unprotected).
    pub deployment_id_label: Option<String>,
    /// The `deployment-uuid` bookkeeping label — the globally-unique deployment
    /// identifier. Used to match a container to its deployment row without the
    /// same-second-collision risk of the timestamp-based `deployment_id`. `None`
    /// for legacy containers created before this label was read.
    pub deployment_uuid_label: Option<String>,
    /// Monotonic generation parsed from the `generation` label. The NEXT
    /// generation on a recreate is `generation + 1`. Defaults to `0` when the
    /// label is missing/legacy/unparseable, so the first recreate of a legacy
    /// container yields generation `1`.
    pub generation: u32,
    /// Zero-based replica index parsed from the `replica` label. Member of the
    /// stable identity tuple, so each replica is matched/recreated independently.
    /// Defaults to `0` when the label is missing/legacy/unparseable, so a
    /// pre-replica container maps onto replica 0 of its spec.
    pub replica: u32,
    pub image_label: Option<String>,
    pub env_hash_label: Option<String>,
    /// The `route-hash` recreate-signature label stamped on the live container
    /// (sha256 of its rendered Traefik label set plus its published-port intent).
    /// `None` for legacy containers created before this label existed. Compared
    /// against the desired container's `route_hash` so a routing transition (a
    /// deployment becoming or ceasing to be active) OR a published-port change
    /// forces a recreate — Docker can't mutate a running container's labels or
    /// port bindings in place.
    pub route_hash_label: Option<String>,
    /// Daemon-reported lifecycle state (e.g. "running", "exited", "created",
    /// "dead", "restarting", "paused"). `None` when the daemon didn't report
    /// it. A container that matches on image + env but is in a terminal-ish
    /// state (`exited`/`created`/`dead`/`None`) is recreated; transient managed
    /// states (`running`/`restarting`/`paused`) are left alone — see
    /// `ActualContainer::needs_recreate`.
    pub state: Option<String>,
}

impl ActualContainer {
    /// Stable identity of this container from its bookkeeping LABELS — the tuple
    /// (project, deployment_group, deployment_id, container). Used to match an
    /// actual container to a desired one WITHOUT relying on the live `--name`,
    /// which now carries a per-recreate `_g{n}` generation suffix. Returns
    /// `None` when any identity label is missing (a legacy/foreign container
    /// that can't be matched — it's only ever a removable orphan, if not
    /// protected).
    pub(crate) fn identity(&self) -> Option<String> {
        Some(identity_key(
            self.project.as_deref()?,
            self.deployment_group.as_deref()?,
            self.deployment_id_label.as_deref()?,
            self.container.as_deref()?,
            self.replica,
        ))
    }

    /// Spec-level key (identity minus replica) for grouping replicas of a spec.
    /// `None` when any spec-identity label is missing (a legacy/foreign
    /// container that can't be grouped).
    pub(crate) fn spec_identity(&self) -> Option<String> {
        Some(spec_key(
            self.project.as_deref()?,
            self.deployment_group.as_deref()?,
            self.deployment_id_label.as_deref()?,
            self.container.as_deref()?,
        ))
    }

    /// Whether the daemon reports this container in a terminal-ish state that
    /// won't self-recover, so the reconciler must recreate it.
    ///
    /// Only `exited`/`created`/`dead` (and an unreported state, `None`) qualify:
    /// these never start serving again on their own. Transient daemon-managed
    /// states — `running`, `restarting` (Docker's restart policy applying
    /// backoff after a crash) and `paused` (operator-initiated) — must NOT be
    /// force-recreated: doing so every reconcile tick would reset Docker's
    /// restart history and fight its exponential backoff. The health probe
    /// drives the Unhealthy status independently, so a crash-looping container
    /// is reflected in status without the reconciler churning it.
    fn needs_recreate(&self) -> bool {
        matches!(
            self.state.as_deref(),
            Some("exited") | Some("created") | Some("dead") | None
        )
    }
}

/// Owned snapshot of one `inspect_container` call, captured once per reconcile
/// tick and reused by both the health probe and the `pod_status` builder. All
/// fields are owned so the value can be stored in a map and unit-tested without
/// a live daemon.
#[derive(Debug, Clone)]
pub struct InspectedContainer {
    /// `state.status` as the lowercase Docker API string ("running", "exited",
    /// "created", "restarting", "dead", …). `None` if absent.
    pub status: Option<String>,
    /// `state.running`.
    pub running: bool,
    /// `state.started_at` (RFC3339 string from the daemon).
    pub started_at: Option<String>,
    /// `state.finished_at`.
    pub finished_at: Option<String>,
    /// `state.exit_code`.
    pub exit_code: Option<i64>,
    /// TOP-LEVEL `restart_count` from the inspect response (not inside state).
    pub restart_count: Option<i64>,
    /// `state.health.status` ("none"/"starting"/"healthy"/"unhealthy").
    pub health: Option<String>,
    /// `state.error` (non-empty only).
    pub error: Option<String>,
    /// IPv4 address on the configured Traefik network (the non-published probe
    /// target). `None` if not attached/assigned yet.
    pub ip: Option<String>,
    /// The random `127.0.0.1` host port Docker published for the app port (only
    /// when `publish_app_ports` created a binding). The published probe target.
    pub published_host_port: Option<String>,
}

/// What to do with one container slot.
///
/// Create/Recreate carry the stable `identity` (so `apply_actions` can find the
/// desired container by its identity tuple), the resolved generation-ful `name`
/// (for logging/clarity), and the `generation` to stamp on the new container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileAction {
    /// No matching actual container — create + start it at `generation`.
    Create {
        identity: String,
        name: String,
        generation: u32,
    },
    /// Actual exists but drifted (image / env-hash / route-hash / not running)
    /// — replace it, bumping to `generation` (live generation + 1).
    Recreate {
        identity: String,
        name: String,
        existing_id: String,
        generation: u32,
    },
    /// Actual exists with no desired match — garbage-collect it.
    Remove { id: String, name: String },
}

/// Classify each desired/actual container into create / recreate / remove.
///
/// Pure: no daemon access. Matching is by the STABLE IDENTITY tuple (project,
/// deployment_group, deployment_id, container) read from bookkeeping labels —
/// NOT by container name, which now carries a per-recreate `_g{n}` generation
/// suffix. `container_prefix` is the configured prefix used only to render the
/// resolved generation-ful `name` carried on each Create/Recreate action (for
/// logging); `apply_actions` looks the desired container up by `identity`.
///
/// `protected_deployment_ids` is the set of deployment-ids whose desired
/// computation FAILED this tick. An actual container whose `deployment-id`
/// label is in this set is never classified as `Remove`, even though it has no
/// desired match — its absence from `desired` is an artifact of the transient
/// failure, not a genuine orphan. This keeps healthy, actively-serving
/// containers running across a brief KMS/DB hiccup, matching the K8s model
/// (children untouched on a failed sync). Containers for deployments that
/// successfully computed and are genuinely orphaned still get GC'd.
pub fn diff_desired_vs_actual(
    desired: &[DesiredContainer],
    actual: &[ActualContainer],
    container_prefix: &str,
    protected_deployment_ids: &HashSet<String>,
) -> Vec<ReconcileAction> {
    // Desired containers keyed by their STABLE IDENTITY tuple (project, group,
    // deployment-id, container) — NOT by their live `--name`, which now carries
    // a per-recreate `_g{n}` generation suffix that differs between an actual
    // container and the next-generation name we'd compute for it.
    let mut desired_by_identity: HashMap<String, &DesiredContainer> = HashMap::new();
    for d in desired {
        let key = identity_key(
            &d.project,
            &d.deployment_group,
            &d.deployment_id,
            &d.container,
            d.replica,
        );
        desired_by_identity.insert(key, d);
    }

    // Actual containers keyed by the identity tuple read from their bookkeeping
    // labels. Containers missing identity labels (`identity() == None`) are not
    // indexed here — they can never match a desired slot and are only ever
    // removable orphans (handled in the GC pass below).
    let actual_by_identity: HashMap<String, &ActualContainer> = actual
        .iter()
        .filter_map(|a| a.identity().map(|id| (id, a)))
        .collect();

    let mut actions = Vec::new();
    let mut matched_actual_ids: HashSet<String> = HashSet::new();

    for (identity, d) in &desired_by_identity {
        match actual_by_identity.get(identity) {
            None => {
                // Brand-new slot: create at generation 1.
                actions.push(ReconcileAction::Create {
                    identity: identity.clone(),
                    name: container_builder::container_name(
                        container_prefix,
                        &d.project,
                        &d.deployment_group,
                        &d.deployment_id,
                        &d.container,
                        d.replica,
                        1,
                    ),
                    generation: 1,
                });
            }
            Some(a) => {
                matched_actual_ids.insert(a.id.clone());
                let image_drift = a.image_label.as_deref() != Some(d.image.as_str());
                let env_drift = a.env_hash_label.as_deref() != Some(d.env_hash.as_str());
                // Recreate-signature drift: the live container's stamped
                // `route-hash` differs from the desired one. This makes
                // create-time-only properties a first-class part of the diff —
                // when a container's rendered Traefik labels change (its router is
                // withheld/wired by an access-class change, its health-check labels
                // appear/disappear, or its route set changes), OR when its app port
                // starts/stops being published to a loopback host port, the hash
                // changes and forces a recreate to apply it. Docker can't mutate a running
                // container's labels or port bindings in place, so recreation is
                // the only way. Idempotent: once the stamped hash equals the
                // desired one nothing recreates. A legacy container missing the
                // label (`None`) is recreated once to gain it, then converges.
                //
                // NOTE: the generation is deliberately NOT part of any of these
                // drift checks (it isn't fed into image/env/route hashes either),
                // so a stable container produces ZERO actions regardless of its
                // `_g{n}` suffix — the key no-infinite-recreate invariant.
                let route_drift = a.route_hash_label.as_deref() != Some(d.route_hash.as_str());
                // A container that matches on image + env but is in a
                // terminal-ish state (created-but-never-started, exited, or
                // crash-give-up `dead`) must be recreated so the deployment
                // recovers. Transient managed states (`restarting`, `paused`)
                // are left to Docker's restart policy so its backoff can settle.
                let not_running = a.needs_recreate();
                if image_drift || env_drift || route_drift || not_running {
                    // Bump to the next generation so the replacement's name is
                    // visibly newer than the one it replaces.
                    let generation = a.generation + 1;
                    actions.push(ReconcileAction::Recreate {
                        identity: identity.clone(),
                        name: container_builder::container_name(
                            container_prefix,
                            &d.project,
                            &d.deployment_group,
                            &d.deployment_id,
                            &d.container,
                            d.replica,
                            generation,
                        ),
                        existing_id: a.id.clone(),
                        generation,
                    });
                }
            }
        }
    }

    for a in actual {
        if matched_actual_ids.contains(&a.id) {
            continue;
        }
        // Protect containers belonging to a deployment whose desired
        // computation failed this tick: treat them as matched rather than
        // orphaned, so a transient error can't GC a healthy container.
        if a.deployment_id_label
            .as_deref()
            .is_some_and(|id| protected_deployment_ids.contains(id))
        {
            continue;
        }
        actions.push(ReconcileAction::Remove {
            id: a.id.clone(),
            name: a.name.clone(),
        });
    }

    // Deterministic ordering for testability.
    actions.sort_by_key(action_key);
    actions
}

pub(crate) fn action_key(a: &ReconcileAction) -> (u8, String) {
    match a {
        ReconcileAction::Create { name, .. } => (0, name.clone()),
        ReconcileAction::Recreate { name, .. } => (1, name.clone()),
        ReconcileAction::Remove { name, .. } => (2, name.clone()),
    }
}

/// Stable identity key for a container slot, joining the identity tuple
/// (project, deployment_group, deployment_id, container, replica) with a NUL
/// separator (which can't appear in any of these names). Built from desired
/// fields and from actual bookkeeping labels so the two match across name
/// generations. The replica index is the matching unit: each replica of a spec
/// is reconciled independently.
pub(crate) fn identity_key(
    project: &str,
    group: &str,
    deployment_id: &str,
    container: &str,
    replica: u32,
) -> String {
    format!("{project}\u{0}{group}\u{0}{deployment_id}\u{0}{container}\u{0}{replica}")
}

/// Spec-level key = the identity tuple MINUS the replica index
/// (project, group, deployment_id, container). Groups all replicas of a single
/// spec together — used by the rolling-recreate throttle to reason per-spec.
pub(crate) fn spec_key(project: &str, group: &str, deployment_id: &str, container: &str) -> String {
    format!("{project}\u{0}{group}\u{0}{deployment_id}\u{0}{container}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::deployment::controller::docker::test_helpers::*;

    #[test]
    fn diff_creates_missing() {
        let d = desired("app", "img:1", "h1");
        let actions =
            diff_desired_vs_actual(std::slice::from_ref(&d), &[], "rise", &no_protected());
        assert_eq!(
            actions,
            vec![ReconcileAction::Create {
                identity: identity_of(&d),
                name: name_of_gen(&d, 1),
                generation: 1,
            }]
        );
    }

    #[test]
    fn diff_no_action_when_matched() {
        let d = desired("app", "img:1", "h1");
        let actual = vec![actual_for(&d, "img:1", "h1")];
        let actions = diff_desired_vs_actual(&[d], &actual, "rise", &no_protected());
        assert!(actions.is_empty());
    }

    #[test]
    fn diff_recreates_on_image_drift() {
        let d = desired("app", "img:2", "h1");
        let actual = vec![actual_for(&d, "img:1", "h1")];
        let actions =
            diff_desired_vs_actual(std::slice::from_ref(&d), &actual, "rise", &no_protected());
        assert_eq!(
            actions,
            vec![ReconcileAction::Recreate {
                identity: identity_of(&d),
                name: name_of_gen(&d, 2),
                existing_id: "cid".to_string(),
                generation: 2,
            }]
        );
    }

    #[test]
    fn diff_recreates_on_env_hash_drift() {
        let d = desired("app", "img:1", "h2");
        let actual = vec![actual_for(&d, "img:1", "h1")];
        let actions =
            diff_desired_vs_actual(std::slice::from_ref(&d), &actual, "rise", &no_protected());
        assert_eq!(
            actions,
            vec![ReconcileAction::Recreate {
                identity: identity_of(&d),
                name: name_of_gen(&d, 2),
                existing_id: "cid".to_string(),
                generation: 2,
            }]
        );
    }

    #[test]
    fn diff_recreates_when_matched_but_not_running() {
        // Image + env match, but the container exited / never started — it must
        // be recreated so the deployment recovers.
        let d = desired("app", "img:1", "h1");
        for state in [Some("exited"), Some("created"), Some("dead"), None] {
            let actual = vec![ActualContainer {
                state: state.map(str::to_string),
                ..actual_for(&d, "img:1", "h1")
            }];
            let actions =
                diff_desired_vs_actual(std::slice::from_ref(&d), &actual, "rise", &no_protected());
            assert_eq!(
                actions,
                vec![ReconcileAction::Recreate {
                    identity: identity_of(&d),
                    name: name_of_gen(&d, 2),
                    existing_id: "cid".to_string(),
                    generation: 2,
                }],
                "state {state:?} should force recreate"
            );
        }
    }

    #[test]
    fn diff_leaves_transient_states_to_docker() {
        // Image + env match and the container is in a daemon-managed transient
        // state: `running` (healthy), `restarting` (Docker's restart policy
        // applying backoff after a crash) or `paused` (operator-initiated).
        // None of these may be force-recreated — doing so each tick would reset
        // Docker's restart backoff. The health probe drives Unhealthy status
        // independently.
        let d = desired("app", "img:1", "h1");
        for state in [Some("running"), Some("restarting"), Some("paused")] {
            let actual = vec![ActualContainer {
                state: state.map(str::to_string),
                ..actual_for(&d, "img:1", "h1")
            }];
            let actions =
                diff_desired_vs_actual(std::slice::from_ref(&d), &actual, "rise", &no_protected());
            assert!(
                actions.is_empty(),
                "state {state:?} must not force a recreate"
            );
        }
    }

    #[test]
    fn diff_removes_orphan() {
        let actual = vec![ActualContainer {
            id: "old".to_string(),
            name: "rise_myapp_default_oldid_app_g1".to_string(),
            project: Some("myapp".to_string()),
            deployment_group: Some("default".to_string()),
            container: Some("app".to_string()),
            deployment_id_label: Some("oldid".to_string()),
            deployment_uuid_label: None,
            generation: 1,
            replica: 0,
            image_label: Some("img:1".to_string()),
            env_hash_label: Some("h1".to_string()),
            route_hash_label: Some("rh-active".to_string()),
            state: Some("running".to_string()),
        }];
        let actions = diff_desired_vs_actual(&[], &actual, "rise", &no_protected());
        assert_eq!(
            actions,
            vec![ReconcileAction::Remove {
                id: "old".to_string(),
                name: "rise_myapp_default_oldid_app_g1".to_string()
            }]
        );
    }

    #[test]
    fn diff_supersession_creates_new_removes_old() {
        // New deployment desired; old deployment's container still present.
        let new = desired("app", "img:2", "h2");
        let actual = vec![ActualContainer {
            id: "old".to_string(),
            name: "rise_myapp_default_oldid_app_g1".to_string(),
            project: Some("myapp".to_string()),
            deployment_group: Some("default".to_string()),
            container: Some("app".to_string()),
            deployment_id_label: Some("oldid".to_string()),
            deployment_uuid_label: None,
            generation: 1,
            replica: 0,
            image_label: Some("img:1".to_string()),
            env_hash_label: Some("h1".to_string()),
            route_hash_label: Some("rh-active".to_string()),
            state: Some("running".to_string()),
        }];
        let actions =
            diff_desired_vs_actual(std::slice::from_ref(&new), &actual, "rise", &no_protected());
        assert_eq!(
            actions,
            vec![
                ReconcileAction::Create {
                    identity: identity_of(&new),
                    name: name_of_gen(&new, 1),
                    generation: 1,
                },
                ReconcileAction::Remove {
                    id: "old".to_string(),
                    name: "rise_myapp_default_oldid_app_g1".to_string()
                },
            ]
        );
    }

    #[test]
    fn diff_protects_container_when_its_deployment_failed_to_compute() {
        // A deployment whose desired computation failed this tick (e.g. a brief
        // KMS/DB hiccup) is absent from `desired`, so its still-running container
        // looks orphaned. With its deployment-id protected, the diff must NOT
        // remove it — matching the K8s model (children untouched on failed sync).
        let actual = vec![ActualContainer {
            id: "live".to_string(),
            name: "rise_myapp_default_protectedid_app_g1".to_string(),
            project: Some("myapp".to_string()),
            deployment_group: Some("default".to_string()),
            container: Some("app".to_string()),
            deployment_id_label: Some("protectedid".to_string()),
            deployment_uuid_label: None,
            generation: 1,
            replica: 0,
            image_label: Some("img:1".to_string()),
            env_hash_label: Some("h1".to_string()),
            route_hash_label: Some("rh-active".to_string()),
            state: Some("running".to_string()),
        }];
        let mut protected = HashSet::new();
        protected.insert("protectedid".to_string());
        let actions = diff_desired_vs_actual(&[], &actual, "rise", &protected);
        assert!(
            actions.is_empty(),
            "protected deployment's container must not be GC'd"
        );
    }

    #[test]
    fn diff_still_gcs_unprotected_orphan_alongside_protected() {
        // Only the failed deployment's container is protected; a genuinely
        // orphaned container belonging to a different, successfully-computed
        // deployment is still removed.
        let actual = vec![
            ActualContainer {
                id: "protected".to_string(),
                name: "rise_myapp_default_protectedid_app_g1".to_string(),
                project: Some("myapp".to_string()),
                deployment_group: Some("default".to_string()),
                container: Some("app".to_string()),
                deployment_id_label: Some("protectedid".to_string()),
                deployment_uuid_label: None,
                generation: 1,
                replica: 0,
                image_label: Some("img:1".to_string()),
                env_hash_label: Some("h1".to_string()),
                route_hash_label: Some("rh-active".to_string()),
                state: Some("running".to_string()),
            },
            ActualContainer {
                id: "orphan".to_string(),
                name: "rise_myapp_default_orphanid_app_g1".to_string(),
                project: Some("myapp".to_string()),
                deployment_group: Some("default".to_string()),
                container: Some("app".to_string()),
                deployment_id_label: Some("orphanid".to_string()),
                deployment_uuid_label: None,
                generation: 1,
                replica: 0,
                image_label: Some("img:1".to_string()),
                env_hash_label: Some("h1".to_string()),
                route_hash_label: Some("rh-active".to_string()),
                state: Some("running".to_string()),
            },
        ];
        let mut protected = HashSet::new();
        protected.insert("protectedid".to_string());
        let actions = diff_desired_vs_actual(&[], &actual, "rise", &protected);
        assert_eq!(
            actions,
            vec![ReconcileAction::Remove {
                id: "orphan".to_string(),
                name: "rise_myapp_default_orphanid_app_g1".to_string()
            }],
            "only the unprotected orphan should be removed"
        );
    }

    #[test]
    fn diff_recreates_on_route_hash_drift_active_gains_labels() {
        // A deployment that just became active: desired now carries the active
        // route-hash, but the live container was created while non-routable and
        // still bears the empty/non-routable hash. Image + env match, but the
        // routing changed — Docker can't edit labels in place, so the container
        // must be recreated WITH the Traefik labels.
        let d = desired("app", "img:1", "h1"); // route_hash = "rh-active"
        let actual = vec![ActualContainer {
            // Was created non-routable (empty route-hash).
            route_hash_label: Some(String::new()),
            ..actual_for(&d, "img:1", "h1")
        }];
        let actions =
            diff_desired_vs_actual(std::slice::from_ref(&d), &actual, "rise", &no_protected());
        assert_eq!(
            actions,
            vec![ReconcileAction::Recreate {
                identity: identity_of(&d),
                name: name_of_gen(&d, 2),
                existing_id: "cid".to_string(),
                generation: 2,
            }]
        );
    }

    #[test]
    fn diff_recreates_on_route_hash_drift_deactivated_loses_labels() {
        // A deployment that stopped being active: desired is now non-routable
        // (empty route-hash) while the live container still carries the active
        // routing hash. Image + env match, but its Traefik labels must be
        // removed — force a recreate.
        let mut d = desired("app", "img:1", "h1");
        d.routable = false;
        d.route_hash = String::new();
        let actual = vec![ActualContainer {
            route_hash_label: Some("rh-active".to_string()),
            ..actual_for(&d, "img:1", "h1")
        }];
        let actions =
            diff_desired_vs_actual(std::slice::from_ref(&d), &actual, "rise", &no_protected());
        assert_eq!(
            actions,
            vec![ReconcileAction::Recreate {
                identity: identity_of(&d),
                name: name_of_gen(&d, 2),
                existing_id: "cid".to_string(),
                generation: 2,
            }]
        );
    }

    #[test]
    fn diff_no_action_when_route_image_env_all_match() {
        // Idempotence: once image + env + route-hash all match and the container
        // is running, the diff emits NO action — no recreate oscillation.
        let d = desired("app", "img:1", "h1"); // route_hash = "rh-active"
        let actual = vec![actual_for(&d, "img:1", "h1")];
        let actions =
            diff_desired_vs_actual(std::slice::from_ref(&d), &actual, "rise", &no_protected());
        assert!(actions.is_empty(), "matching container must not recreate");
    }

    #[test]
    fn diff_no_action_when_matched_with_different_generation_suffix() {
        // The KEY anti-infinite-recreate test: a stable container (no image/env/
        // route drift, running) that was recreated several times — so its live
        // name carries `_g5` and its generation label is 5 — must still match the
        // desired slot by IDENTITY and produce ZERO actions. The generation never
        // feeds any matching key or hash, so the `_g{n}` suffix is irrelevant.
        let d = desired("app", "img:1", "h1"); // route_hash = "rh-active"
        let actual = vec![actual_for_gen(&d, "img:1", "h1", 5)];
        assert!(
            actual[0].name.ends_with("_g5"),
            "fixture must carry a non-1 generation suffix"
        );
        let actions =
            diff_desired_vs_actual(std::slice::from_ref(&d), &actual, "rise", &no_protected());
        assert!(
            actions.is_empty(),
            "a stable container must not recreate regardless of its _g{{n}} suffix"
        );
    }

    #[test]
    fn diff_recreate_bumps_generation_from_actual() {
        // A drifted match at generation 5 recreates at generation 6 (actual + 1),
        // and the carried name reflects the bumped generation.
        let d = desired("app", "img:2", "h1"); // image drift
        let actual = vec![actual_for_gen(&d, "img:1", "h1", 5)];
        let actions =
            diff_desired_vs_actual(std::slice::from_ref(&d), &actual, "rise", &no_protected());
        assert_eq!(
            actions,
            vec![ReconcileAction::Recreate {
                identity: identity_of(&d),
                name: name_of_gen(&d, 6),
                existing_id: "cid".to_string(),
                generation: 6,
            }]
        );
    }

    #[test]
    fn diff_create_uses_generation_one() {
        // A missing slot always creates at generation 1.
        let d = desired("app", "img:1", "h1");
        let actions =
            diff_desired_vs_actual(std::slice::from_ref(&d), &[], "rise", &no_protected());
        assert_eq!(
            actions,
            vec![ReconcileAction::Create {
                identity: identity_of(&d),
                name: name_of_gen(&d, 1),
                generation: 1,
            }]
        );
    }

    #[test]
    fn diff_scale_up_creates_new_replica_indices() {
        // Desired 3 replicas (r0..r2); only r0 exists → r1 and r2 are Created,
        // r0 is left untouched (image/env/route all match).
        let desired_all = vec![desired_replica(0), desired_replica(1), desired_replica(2)];
        let actual = vec![actual_replica(0, "running", "img:1")];
        let actions = diff_desired_vs_actual(&desired_all, &actual, "rise", &no_protected());
        let creates: Vec<u32> = actions
            .iter()
            .filter_map(|a| match a {
                ReconcileAction::Create { name, .. } if name.contains("_r1_") => Some(1),
                ReconcileAction::Create { name, .. } if name.contains("_r2_") => Some(2),
                _ => None,
            })
            .collect();
        assert_eq!(
            creates,
            vec![1, 2],
            "scale-up creates the new replica slots"
        );
        assert!(
            !actions.iter().any(|a| matches!(
                a,
                ReconcileAction::Recreate { .. } | ReconcileAction::Remove { .. }
            )),
            "scale-up must not recreate or remove the existing replica"
        );
    }

    #[test]
    fn diff_scale_down_removes_surplus_replicas() {
        // Desired 1 replica (r0); r0..r2 exist → r1 and r2 are surplus → Removed.
        let desired_all = vec![desired_replica(0)];
        let actual = vec![
            actual_replica(0, "running", "img:1"),
            actual_replica(1, "running", "img:1"),
            actual_replica(2, "running", "img:1"),
        ];
        let actions = diff_desired_vs_actual(&desired_all, &actual, "rise", &no_protected());
        let mut removed: Vec<String> = actions
            .iter()
            .filter_map(|a| match a {
                ReconcileAction::Remove { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        removed.sort();
        assert_eq!(removed, vec!["cid-r1".to_string(), "cid-r2".to_string()]);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, ReconcileAction::Create { .. })),
            "scale-down adds no containers"
        );
    }

    #[test]
    fn diff_scale_down_respects_protected_deployment_ids() {
        // A surplus replica whose deployment-id is protected is NOT removed.
        let desired_all = vec![desired_replica(0)];
        let actual = vec![
            actual_replica(0, "running", "img:1"),
            actual_replica(1, "running", "img:1"),
        ];
        let mut protected = HashSet::new();
        protected.insert("20260101-120000".to_string()); // desired()'s deployment_id
        let actions = diff_desired_vs_actual(&desired_all, &actual, "rise", &protected);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, ReconcileAction::Remove { .. })),
            "protected surplus replica must not be GC'd"
        );
    }
}
