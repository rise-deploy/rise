//! Pure helpers for the rolling-recreate throttle and the in-rotation /
//! readiness decisions of the Docker cutover. All `&self`-free so they can be
//! unit-tested without a daemon or Traefik API.

use std::collections::{HashMap, HashSet};

use super::container_builder;
use super::diff::{action_key, ActualContainer, ReconcileAction};

/// Apply the ROLLING-RECREATE throttle to a diff's actions, run AFTER
/// [`super::diff::diff_desired_vs_actual`] and BEFORE `apply_actions`. Enforces,
/// per spec (grouped by the spec key = identity minus replica), that a *running
/// but drifted* replica is recreated at most ONE-AT-A-TIME and only while every
/// OTHER replica of that spec is currently HEALTHY — so a rollout never drops
/// more than one replica of capacity at once.
///
/// Pass-through rules (PER SPEC):
/// - **Create** / **Remove** → ALWAYS pass (initial rollout / scale-up add
///   capacity; scale-down / GC remove it). Never throttled.
/// - **Recreate** of a recovery replica — the matched live container's `state`
///   is NOT "running" (crashed / exited / created / missing) → ALWAYS pass; it's
///   already down, so recreating only restores capacity.
/// - **Recreate** of a rollout replica — the matched live container IS "running"
///   but drifted → emit AT MOST ONE per spec this tick, the lowest replica index,
///   and ONLY IF every OTHER replica of the spec is HEALTHY
///   (`healthy_by_identity`). If ANY sibling is unhealthy/starting, defer ALL
///   rollout recreates for the spec this tick.
///
/// INVARIANT: for a single spec with R running+drifted+healthy replicas this
/// yields exactly ONE rollout Recreate; with any sibling unhealthy it yields
/// ZERO; recovery Recreates and all Creates/Removes pass regardless.
///
/// `actual` provides each container's run state + identity (to map an action's
/// identity → its spec + replica index + run state). `healthy_by_identity` maps
/// an `identity_key(...)` to whether that live replica passed the HTTP health
/// probe this tick (absent → treated as NOT healthy).
pub(crate) fn filter_rolling_actions(
    actions: Vec<ReconcileAction>,
    actual: &[ActualContainer],
    healthy_by_identity: &HashMap<String, bool>,
) -> Vec<ReconcileAction> {
    // identity → the live container (for run state + replica/spec grouping).
    let actual_by_identity: HashMap<String, &ActualContainer> = actual
        .iter()
        .filter_map(|a| a.identity().map(|id| (id, a)))
        .collect();

    // Collect rollout-recreate candidates per spec; pass everything else through.
    // A candidate is (replica_index, action) where the matched live container is
    // running. We then admit at most one per spec, gated on sibling health.
    let mut passed: Vec<ReconcileAction> = Vec::new();
    // spec_key → Vec<(replica, identity, action)> of running+drifted recreates.
    let mut rollout_candidates: HashMap<String, Vec<(u32, String, ReconcileAction)>> =
        HashMap::new();

    for action in actions {
        match &action {
            ReconcileAction::Create { .. } | ReconcileAction::Remove { .. } => {
                // Unthrottled: capacity add (Create) / scale-down or GC (Remove).
                passed.push(action);
            }
            ReconcileAction::Recreate { identity, .. } => {
                match actual_by_identity.get(identity) {
                    Some(a) if a.state.as_deref() == Some("running") => {
                        // Rollout candidate: a running, drifted replica. Defer the
                        // admission decision until we've seen all of them per spec.
                        let key = a.spec_identity().unwrap_or_else(|| identity.clone());
                        rollout_candidates.entry(key).or_default().push((
                            a.replica,
                            identity.clone(),
                            action,
                        ));
                    }
                    // Recovery (not running) OR no matched live container (already
                    // gone) → pass unthrottled: recreating restores capacity.
                    _ => passed.push(action),
                }
            }
        }
    }

    // Admit at most one rollout recreate per spec, the lowest replica index, and
    // only when every OTHER replica of the spec is healthy.
    for (spec, mut candidates) in rollout_candidates {
        candidates.sort_by_key(|(replica, _, _)| *replica);
        // Identities of this spec's replicas slated for a rollout recreate — they
        // don't count as "must be healthy" siblings (they're already drifted and
        // about to be replaced), so a uniform drift across all replicas still
        // rolls one-by-one rather than deadlocking.
        let candidate_identities: HashSet<String> =
            candidates.iter().map(|(_, id, _)| id.clone()).collect();

        // Gate: every OTHER replica of this spec (a live replica NOT itself a
        // rollout candidate) must be healthy. If any such sibling is
        // unhealthy/starting, defer ALL rollout recreates for this spec.
        let siblings_healthy = actual
            .iter()
            .filter(|a| a.spec_identity().as_deref() == Some(spec.as_str()))
            .filter_map(|a| a.identity())
            .filter(|id| !candidate_identities.contains(id))
            .all(|id| healthy_by_identity.get(&id).copied().unwrap_or(false));

        if siblings_healthy {
            if let Some((_, _, action)) = candidates.into_iter().next() {
                passed.push(action);
            }
        }
        // else: defer ALL of this spec's rollout recreates this tick.
    }

    passed.sort_by_key(action_key);
    passed
}

/// Group-scoped Traefik service name(s) a container spec's routes emit, used by
/// the reconciler's `serverStatus` lookup so it queries the SAME service(s) the
/// labels stamp. Mirrors [`container_builder::group_service_names`] but derives
/// from the runtime [`crate::server::deployment::models::ContainerSpec`] +
/// [`crate::server::deployment::models::RouteSpec`] set (what `reconcile_health`
/// has on hand): a single-route container yields the bare base
/// `{project}-{group}-{container}`; a multi-route container yields per-route
/// `{base}-{idx}` names (longest path-prefix first, matching the renderer).
///
/// `primary_hosts` is the deployment's resolved ingress hosts. The label renderer
/// emits a router (and thus a Traefik service) only for routes that have at least
/// one host (`render_traefik_labels_for` gates each route on `!hosts.is_empty()`,
/// and the runtime routes all share `primary_hosts`). When `primary_hosts` is
/// empty NO router is stamped, so this returns empty too — otherwise the
/// reconciler would query `serverStatus` for services Traefik never registered
/// and log a misleading WARN every tick.
///
/// A port-less worker has no routes/service → empty. Routability is implicit:
/// every infra-bearing deployment is routable.
pub(crate) fn service_names_for_spec(
    project: &str,
    deployment_group: &str,
    spec: &crate::server::deployment::models::ContainerSpec,
    route_specs: &[crate::server::deployment::models::RouteSpec],
    primary_hosts: &[String],
) -> Vec<String> {
    // No routable host → no router stamped → no service to query (mirrors the
    // label renderer's per-route `!hosts.is_empty()` gate; the runtime routes all
    // share the deployment's `primary_hosts`).
    if spec.port.is_none() || primary_hosts.is_empty() {
        return Vec::new();
    }
    let base = container_builder::group_service_base(project, deployment_group, &spec.name);
    // Routes for this container, sorted longest-path-prefix-first — the SAME
    // ordering `render_traefik_labels_for` uses to index per-route services.
    let mut routes: Vec<&crate::server::deployment::models::RouteSpec> = route_specs
        .iter()
        .filter(|r| r.container == spec.name)
        .collect();
    routes.sort_by_key(|r| std::cmp::Reverse(r.path.len()));
    let route_count = routes.len();
    if route_count == 0 {
        return Vec::new();
    }
    (0..route_count)
        .map(|idx| container_builder::group_service_name(&base, idx, route_count))
        .collect()
}

/// Whether a single replica counts as READY, factored out of the per-replica
/// loop in `reconcile_health` so the `serverStatus`/fallback selection is
/// unit-testable without a daemon. Inputs:
///
/// - `has_health_path`: the container has an effective health path;
/// - `running`: the live container is `running` on the daemon;
/// - `api_available`: a Traefik API client is configured AND its serverStatus
///   call for this container's service(s) succeeded;
/// - `server_up`: when `api_available`, whether Traefik reports this container's
///   server URL UP (`None` = absent from the map / no IP yet);
/// - `probe_ok`: the result of Rise's own HTTP probe when one was performed
///   (`Some(true/false)`), or `None` when no probe has been run yet.
///
/// Returns [`ReadyVerdict::NeedsProbe`] when the decision depends on Rise's own
/// probe but `probe_ok` is `None` — the caller then runs `probe_container` and
/// re-resolves (or passes the result back in). Delegates to
/// [`rolling_rotation_decision`], mirroring with Rise's own probe when Traefik
/// has no authoritative signal.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ReadyVerdict {
    Ready,
    NotReady(String),
    /// The verdict needs Rise's own HTTP probe; the caller must run it and fold
    /// the result back in (via `probe_ok`).
    NeedsProbe,
}

pub(crate) fn replica_ready(
    has_health_path: bool,
    running: bool,
    api_available: bool,
    server_up: Option<bool>,
    probe_ok: Option<bool>,
) -> ReadyVerdict {
    match rolling_rotation_decision(has_health_path, running, api_available, server_up) {
        RotationDecision::InRotation => ReadyVerdict::Ready,
        RotationDecision::NotInRotation(reason) => ReadyVerdict::NotReady(reason),
        RotationDecision::FallBackToProbe => match probe_ok {
            Some(true) => ReadyVerdict::Ready,
            Some(false) => ReadyVerdict::NotReady("health probe failed".to_string()),
            None => ReadyVerdict::NeedsProbe,
        },
    }
}

/// Outcome of the in-rotation decision for a single container.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RotationDecision {
    /// The container's server is in Traefik's rotation (or running, for a
    /// ready-when-running container) → counts toward deployment readiness.
    InRotation,
    /// The container is NOT in rotation; the string is a human-readable reason
    /// for the deployment's not-ready diagnostic.
    NotInRotation(String),
    /// No authoritative Traefik signal is available (a `health_check` is
    /// configured but the Traefik API is unset or errored). The caller must
    /// MIRROR — fall back to Rise's own `probe_container` as the in-rotation
    /// proxy (today's behavior).
    FallBackToProbe,
}

/// Pure decision for whether a single container is "in rotation", given:
///
/// - `has_health_path`: the container has an effective health path (a
///   `health_check` is configured AND not disabled);
/// - `running`: the live container is `running` on the daemon;
/// - `api_available`: a Traefik API client is configured AND its call for this
///   service succeeded (so `server_up` is authoritative);
/// - `server_up`: when `api_available`, whether Traefik's `serverStatus`
///   reports this container's server URL (`http://{ip}:{port}`) as UP. `None`
///   means the server URL was absent from the map (Traefik doesn't know it yet).
///
/// Rules (see the module / Step B spec):
/// - has_health_path + api_available → in-rotation IFF Traefik reports the
///   server UP; a DOWN/absent server is NOT in rotation (so the old deployment
///   is kept until the new server is confirmed serving).
/// - !has_health_path → ready-when-running: in-rotation IFF the container is
///   `running` (Traefik routes to running servers immediately, no HC).
/// - has_health_path + !api_available → `FallBackToProbe`: the caller mirrors
///   with Rise's own probe (graceful degradation when no Traefik API).
///
/// Kept pure (no `self`, no I/O) so it can be unit-tested without a daemon.
pub(crate) fn rolling_rotation_decision(
    has_health_path: bool,
    running: bool,
    api_available: bool,
    server_up: Option<bool>,
) -> RotationDecision {
    if !has_health_path {
        // Ready-when-running: no health check, so Traefik routes to the server
        // as soon as it's a running container.
        return if running {
            RotationDecision::InRotation
        } else {
            RotationDecision::NotInRotation("not running (no health check)".to_string())
        };
    }
    // health_check configured.
    if !api_available {
        // No authoritative Traefik signal — mirror with Rise's own probe.
        return RotationDecision::FallBackToProbe;
    }
    match server_up {
        Some(true) => RotationDecision::InRotation,
        Some(false) => RotationDecision::NotInRotation("Traefik reports server DOWN".to_string()),
        None => RotationDecision::NotInRotation(
            "Traefik does not yet report this server (absent from serverStatus)".to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::deployment::controller::docker::diff::diff_desired_vs_actual;
    use crate::server::deployment::controller::docker::test_helpers::*;

    #[test]
    fn rolling_throttle_running_drifted_emits_exactly_one_recreate() {
        // INVARIANT (a): 3 running + drifted (image) + healthy replicas →
        // the diff produces 3 Recreates, but the rolling throttle admits exactly
        // ONE (the lowest replica index, r0).
        let desired_all = vec![desired_replica(0), desired_replica(1), desired_replica(2)];
        let actual = vec![
            actual_replica(0, "running", "img:OLD"),
            actual_replica(1, "running", "img:OLD"),
            actual_replica(2, "running", "img:OLD"),
        ];
        let raw = diff_desired_vs_actual(&desired_all, &actual, "rise", &no_protected());
        assert_eq!(
            raw.iter()
                .filter(|a| matches!(a, ReconcileAction::Recreate { .. }))
                .count(),
            3,
            "diff alone wants to recreate all three"
        );
        let healthy = all_healthy(&actual);
        let throttled = filter_rolling_actions(raw, &actual, &healthy);
        let recreates: Vec<&ReconcileAction> = throttled
            .iter()
            .filter(|a| matches!(a, ReconcileAction::Recreate { .. }))
            .collect();
        assert_eq!(recreates.len(), 1, "rolling admits exactly one per spec");
        match recreates[0] {
            ReconcileAction::Recreate { name, .. } => {
                assert!(
                    name.contains("_r0_"),
                    "lowest replica index rolls first: {name}"
                );
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn rolling_throttle_defers_all_when_a_sibling_is_unhealthy() {
        // INVARIANT (b): mid-rollout. r0 was already recreated (now matches
        // desired, so it is NOT a candidate) but is still STARTING (unhealthy);
        // r1 and r2 are still drifted (running, img:OLD → rollout candidates).
        // Because the non-candidate sibling r0 is unhealthy, ALL rollout recreates
        // for the spec are deferred this tick → ZERO Recreates. This is the gate
        // that makes the rollout wait for the previous replica to come back up.
        let desired_all = vec![desired_replica(0), desired_replica(1), desired_replica(2)];
        let actual = vec![
            actual_replica(0, "running", "img:1"), // already updated, matches
            actual_replica(1, "running", "img:OLD"), // drifted → candidate
            actual_replica(2, "running", "img:OLD"), // drifted → candidate
        ];
        let raw = diff_desired_vs_actual(&desired_all, &actual, "rise", &no_protected());
        // r0 (the just-recreated, non-candidate sibling) is still unhealthy.
        let mut healthy = all_healthy(&actual);
        healthy.insert(actual[0].identity().unwrap(), false);
        let throttled = filter_rolling_actions(raw, &actual, &healthy);
        assert_eq!(
            throttled
                .iter()
                .filter(|a| matches!(a, ReconcileAction::Recreate { .. }))
                .count(),
            0,
            "an unhealthy (still-starting) sibling defers ALL rollout recreates"
        );
    }

    #[test]
    fn rolling_throttle_uniform_drift_rolls_one_at_a_time() {
        // When EVERY replica is drifted (all candidates), there are no
        // non-candidate siblings to gate on, so the rollout begins: exactly one
        // (r0) is recreated this tick. On the next tick r0 matches desired and
        // becomes the sibling whose health gates the rest — yielding the
        // one-at-a-time roll across ticks.
        let desired_all = vec![desired_replica(0), desired_replica(1), desired_replica(2)];
        let actual = vec![
            actual_replica(0, "running", "img:OLD"),
            actual_replica(1, "running", "img:OLD"),
            actual_replica(2, "running", "img:OLD"),
        ];
        let raw = diff_desired_vs_actual(&desired_all, &actual, "rise", &no_protected());
        let healthy = all_healthy(&actual);
        let throttled = filter_rolling_actions(raw, &actual, &healthy);
        assert_eq!(
            throttled
                .iter()
                .filter(|a| matches!(a, ReconcileAction::Recreate { .. }))
                .count(),
            1,
            "uniform drift still rolls one replica at a time"
        );
    }

    #[test]
    fn rolling_throttle_recovery_recreate_passes_even_with_unhealthy_sibling() {
        // INVARIANT (c): a CRASHED (state != running) replica's Recreate is a
        // recovery — it passes UNTHROTTLED even though a sibling is unhealthy.
        // Here r0 has exited; r1 is running-drifted-unhealthy. Only r0's recovery
        // recreate is admitted (r1's rollout recreate is gated out).
        let desired_all = vec![desired_replica(0), desired_replica(1)];
        let actual = vec![
            actual_replica(0, "exited", "img:1"),    // crashed → recovery
            actual_replica(1, "running", "img:OLD"), // running-drifted → rollout
        ];
        let raw = diff_desired_vs_actual(&desired_all, &actual, "rise", &no_protected());
        // The crashed r0 is (realistically) unhealthy; it is r1's only sibling, so
        // r1's rollout recreate is GATED OUT. r0's recovery recreate still passes.
        let mut healthy = all_healthy(&actual);
        healthy.insert(actual[0].identity().unwrap(), false);
        let throttled = filter_rolling_actions(raw, &actual, &healthy);
        let recreates: Vec<&ReconcileAction> = throttled
            .iter()
            .filter(|a| matches!(a, ReconcileAction::Recreate { .. }))
            .collect();
        assert_eq!(recreates.len(), 1, "only the recovery recreate passes");
        match recreates[0] {
            ReconcileAction::Recreate { name, .. } => {
                assert!(
                    name.contains("_r0_"),
                    "recovery (crashed) replica passes: {name}"
                );
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn rolling_throttle_creates_and_removes_always_pass() {
        // INVARIANT (d): Create (scale-up) and Remove (scale-down/GC) actions are
        // never throttled — capacity changes apply immediately. Mix a Create
        // (r2 missing), a Remove (surplus r3), and a running-drifted r0 (rollout)
        // with r1 healthy. The Create + Remove pass; exactly one rollout recreate
        // passes too (r0, siblings healthy).
        let desired_all = vec![desired_replica(0), desired_replica(1), desired_replica(2)];
        let actual = vec![
            actual_replica(0, "running", "img:OLD"), // rollout recreate
            actual_replica(1, "running", "img:1"),   // matches → no action
            actual_replica(3, "running", "img:1"),   // surplus → Remove
        ];
        let raw = diff_desired_vs_actual(&desired_all, &actual, "rise", &no_protected());
        let healthy = all_healthy(&actual);
        let throttled = filter_rolling_actions(raw, &actual, &healthy);
        assert!(
            throttled.iter().any(
                |a| matches!(a, ReconcileAction::Create { name, .. } if name.contains("_r2_"))
            ),
            "scale-up Create passes"
        );
        assert!(
            throttled
                .iter()
                .any(|a| matches!(a, ReconcileAction::Remove { id, .. } if id == "cid-r3")),
            "scale-down Remove passes"
        );
        assert_eq!(
            throttled
                .iter()
                .filter(|a| matches!(a, ReconcileAction::Recreate { .. }))
                .count(),
            1,
            "exactly one rollout recreate (r0) admitted"
        );
    }

    #[test]
    fn rotation_health_check_up_is_in_rotation() {
        // HC configured, Traefik API available, server UP → in rotation.
        assert_eq!(
            rolling_rotation_decision(true, true, true, Some(true)),
            RotationDecision::InRotation
        );
    }

    #[test]
    fn rotation_health_check_down_is_not_in_rotation() {
        // HC configured, API available, server DOWN → NOT in rotation, with the
        // Traefik-specific reason (distinct from the probe reasons).
        match rolling_rotation_decision(true, true, true, Some(false)) {
            RotationDecision::NotInRotation(reason) => {
                assert!(
                    reason.contains("DOWN"),
                    "reason should mention Traefik server DOWN, got: {reason}"
                );
            }
            other => panic!("expected NotInRotation, got {other:?}"),
        }
    }

    #[test]
    fn rotation_health_check_absent_server_is_not_in_rotation() {
        // HC configured, API available, but the server URL isn't in the
        // serverStatus map yet → not in rotation, distinct reason.
        match rolling_rotation_decision(true, true, true, None) {
            RotationDecision::NotInRotation(reason) => {
                assert!(reason.contains("serverStatus"), "got: {reason}");
            }
            other => panic!("expected NotInRotation, got {other:?}"),
        }
    }

    #[test]
    fn rotation_no_health_check_running_is_in_rotation() {
        // No HC → ready-when-running: a running container is in rotation
        // regardless of any Traefik signal.
        assert_eq!(
            rolling_rotation_decision(false, true, false, None),
            RotationDecision::InRotation
        );
        assert_eq!(
            rolling_rotation_decision(false, true, true, Some(false)),
            RotationDecision::InRotation,
            "no-HC + running ignores Traefik serverStatus"
        );
    }

    #[test]
    fn rotation_no_health_check_not_running_is_not_in_rotation() {
        match rolling_rotation_decision(false, false, false, None) {
            RotationDecision::NotInRotation(reason) => {
                assert!(reason.contains("not running"), "got: {reason}");
            }
            other => panic!("expected NotInRotation, got {other:?}"),
        }
    }

    #[test]
    fn service_names_for_spec_single_route_is_bare_base() {
        use crate::server::deployment::models::{ContainerSpec, RouteSpec};
        let spec = ContainerSpec {
            name: "app".to_string(),
            image: None,
            port: Some(8080),
            replicas: None,
            cpu: None,
            memory: None,
            env_overrides: vec![],
            health_check: None,
        };
        let routes = vec![RouteSpec {
            path: "/".to_string(),
            container: "app".to_string(),
        }];
        // A single-route container uses the bare base name — exactly what the
        // labels stamp (`group_service_names` over the matching DesiredContainer).
        // The base carries the injective hash suffix `group_service_base`
        // appends, so derive it the same way rather than hardcoding the hash.
        let base = container_builder::group_service_base("myapp", "default", "app");
        let hosts = ["myapp.rise.dev".to_string()];
        assert_eq!(
            service_names_for_spec("myapp", "default", &spec, &routes, &hosts),
            vec![base]
        );
    }

    #[test]
    fn service_names_for_spec_multi_route_uses_per_route_indices() {
        use crate::server::deployment::models::{ContainerSpec, RouteSpec};
        let spec = ContainerSpec {
            name: "api".to_string(),
            image: None,
            port: Some(8080),
            replicas: None,
            cpu: None,
            memory: None,
            env_overrides: vec![],
            health_check: None,
        };
        // Two routes for `api` (+ one for another container, which is ignored).
        let routes = vec![
            RouteSpec {
                path: "/".to_string(),
                container: "api".to_string(),
            },
            RouteSpec {
                path: "/api/v1".to_string(),
                container: "api".to_string(),
            },
            RouteSpec {
                path: "/".to_string(),
                container: "other".to_string(),
            },
        ];
        // Per-route services, longest path-prefix first (-0 = /api/v1, -1 = /),
        // matching the renderer's `{base}-{idx}` services for a multi-route
        // container. The bare base is NOT queried (it 404s in the Traefik API).
        // The base carries the injective hash suffix, so derive the per-route
        // names via `group_service_name` rather than hardcoding the hash.
        let base = container_builder::group_service_base("myapp", "default", "api");
        let hosts = ["myapp.rise.dev".to_string()];
        assert_eq!(
            service_names_for_spec("myapp", "default", &spec, &routes, &hosts),
            vec![
                container_builder::group_service_name(&base, 0, 2),
                container_builder::group_service_name(&base, 1, 2),
            ]
        );
    }

    #[test]
    fn service_names_for_spec_derivation_matches_container_builder() {
        // The reconciler-side derivation must agree with the builder-side
        // `group_service_names` (the labels) for the same routes — single AND
        // multi-route — so the lookup never drifts from what's stamped.
        use crate::server::deployment::models::{ContainerSpec, RouteSpec};
        use container_builder::{group_service_names, DesiredRoute};

        let mk_desired = |container: &str, paths: &[&str]| {
            let mut d = desired(container, "img:1", "h1");
            d.routes = paths
                .iter()
                .map(|p| DesiredRoute {
                    hosts: vec!["myapp.rise.dev".to_string()],
                    path_prefix: Some(p.to_string()),
                })
                .collect();
            d
        };
        let mk_spec_routes = |container: &str, paths: &[&str]| {
            let spec = ContainerSpec {
                name: container.to_string(),
                image: None,
                port: Some(8080),
                replicas: None,
                cpu: None,
                memory: None,
                env_overrides: vec![],
                health_check: None,
            };
            let routes: Vec<RouteSpec> = paths
                .iter()
                .map(|p| RouteSpec {
                    path: p.to_string(),
                    container: container.to_string(),
                })
                .collect();
            (spec, routes)
        };

        // The desired routes the labels are derived from all carry this host, so
        // the runtime derivation must be given the same host set to agree.
        let hosts = ["myapp.rise.dev".to_string()];
        for paths in [&["/"][..], &["/", "/api/v1"][..], &["/", "/a", "/bb"][..]] {
            let (spec, routes) = mk_spec_routes("app", paths);
            let from_spec = service_names_for_spec("myapp", "default", &spec, &routes, &hosts);
            let from_labels = group_service_names(&mk_desired("app", paths));
            assert_eq!(
                from_spec, from_labels,
                "service-name derivation must match the labels for paths {paths:?}"
            );
        }
    }

    #[test]
    fn service_names_for_spec_worker_has_no_services() {
        use crate::server::deployment::models::ContainerSpec;
        let worker = ContainerSpec {
            name: "worker".to_string(),
            image: None,
            port: None,
            replicas: None,
            cpu: None,
            memory: None,
            env_overrides: vec![],
            health_check: None,
        };
        let hosts = ["myapp.rise.dev".to_string()];
        assert!(service_names_for_spec("myapp", "default", &worker, &[], &hosts).is_empty());
    }

    #[test]
    fn service_names_for_spec_hostless_yields_empty() {
        // A routable HTTP container whose deployment resolved NO ingress host
        // emits no Traefik router (the label renderer gates each route on a
        // non-empty host set), so the reconciler must query NO service — otherwise
        // it would hit `serverStatus` for a service Traefik never registered and
        // log a misleading WARN every tick. Mirrors `render_traefik_labels_for`.
        use crate::server::deployment::models::{ContainerSpec, RouteSpec};
        let spec = ContainerSpec {
            name: "app".to_string(),
            image: None,
            port: Some(8080),
            replicas: None,
            cpu: None,
            memory: None,
            env_overrides: vec![],
            health_check: None,
        };
        let routes = vec![RouteSpec {
            path: "/".to_string(),
            container: "app".to_string(),
        }];
        // Same spec/routes that yield a service WITH a host — but with no host the
        // result is empty.
        assert!(
            service_names_for_spec("myapp", "default", &spec, &routes, &[]).is_empty(),
            "no routable host → no service names"
        );
    }

    #[test]
    fn replica_ready_server_up_is_ready() {
        // HC + Traefik UP → Ready, no probe needed.
        assert_eq!(
            replica_ready(true, true, true, Some(true), None),
            ReadyVerdict::Ready
        );
    }

    #[test]
    fn replica_ready_server_down_is_not_ready() {
        match replica_ready(true, true, true, Some(false), None) {
            ReadyVerdict::NotReady(reason) => assert!(reason.contains("DOWN"), "got: {reason}"),
            other => panic!("expected NotReady, got {other:?}"),
        }
    }

    #[test]
    fn replica_ready_absent_server_is_not_ready() {
        match replica_ready(true, true, true, None, None) {
            ReadyVerdict::NotReady(reason) => {
                assert!(reason.contains("serverStatus"), "got: {reason}")
            }
            other => panic!("expected NotReady, got {other:?}"),
        }
    }

    #[test]
    fn replica_ready_fallback_needs_probe_then_folds_result() {
        // HC but Traefik API unavailable → fall back to Rise's probe. With no
        // probe result yet → NeedsProbe; folding the result in resolves.
        assert_eq!(
            replica_ready(true, true, false, None, None),
            ReadyVerdict::NeedsProbe
        );
        assert_eq!(
            replica_ready(true, true, false, None, Some(true)),
            ReadyVerdict::Ready
        );
        assert!(matches!(
            replica_ready(true, true, false, None, Some(false)),
            ReadyVerdict::NotReady(_)
        ));
    }

    #[test]
    fn replica_ready_no_health_check_is_ready_when_running() {
        // No HC → ready-when-running; Traefik signal ignored.
        assert_eq!(
            replica_ready(false, true, false, None, None),
            ReadyVerdict::Ready
        );
        assert!(matches!(
            replica_ready(false, false, false, None, None),
            ReadyVerdict::NotReady(_)
        ));
    }

    #[test]
    fn rotation_health_check_api_unavailable_falls_back_to_probe() {
        // HC configured but Traefik API unset/errored → MIRROR with Rise's own
        // probe (graceful degradation). The `running`/`server_up` inputs are
        // ignored in this branch.
        assert_eq!(
            rolling_rotation_decision(true, true, false, None),
            RotationDecision::FallBackToProbe
        );
        assert_eq!(
            rolling_rotation_decision(true, false, false, Some(true)),
            RotationDecision::FallBackToProbe
        );
    }
}
