//! Readiness and in-rotation verdicts, decided by Traefik's `serverStatus`.
//!
//! The rule these encode is the load-bearing one for a safe cutover: for a
//! health-checked container **Traefik's `serverStatus` is authoritative with no
//! fallback**. "Ready" therefore means "Traefik is actually routing to it", not
//! "our own probe succeeded" — so the outgoing deployment is never retired while
//! the incoming one is still invisible to the router.
//!
//! The corollary is deliberate and documented: with no reachable Traefik API, a
//! health-checked deployment can never become Healthy. Both backends log a loud
//! warning for exactly that misconfiguration rather than silently falling back
//! to a weaker signal.

use crate::naming;

/// Group-scoped Traefik service name(s) a container spec's routes emit, used by
/// the reconciler's `serverStatus` lookup so it queries the SAME service(s) the
/// labels stamp. Mirrors [`naming::deployment_service_names`] but derives
/// from the runtime [`rise_deployment_spec::request_spec::ContainerSpec`] +
/// [`rise_deployment_spec::request_spec::RouteSpec`] set (what `reconcile_health`
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
pub fn service_names_for_spec(
    project: &str,
    deployment_group: &str,
    deployment_id: &str,
    spec: &rise_deployment_spec::request_spec::ContainerSpec,
    route_specs: &[rise_deployment_spec::request_spec::RouteSpec],
    primary_hosts: &[String],
) -> Vec<String> {
    // No routable host → no router stamped → no service to query (mirrors the
    // label renderer's per-route `!hosts.is_empty()` gate; the runtime routes all
    // share the deployment's `primary_hosts`).
    if spec.port.is_none() || primary_hosts.is_empty() {
        return Vec::new();
    }
    let base =
        naming::deployment_service_base(project, deployment_group, deployment_id, &spec.name);
    // Routes for this container, sorted longest-path-prefix-first — the SAME
    // ordering `render_traefik_labels_for` uses to index per-route services.
    let mut routes: Vec<&rise_deployment_spec::request_spec::RouteSpec> = route_specs
        .iter()
        .filter(|r| r.container == spec.name)
        .collect();
    routes.sort_by_key(|r| std::cmp::Reverse(r.path.len()));
    let route_count = routes.len();
    if route_count == 0 {
        return Vec::new();
    }
    (0..route_count)
        .map(|idx| naming::group_service_name(&base, idx, route_count))
        .collect()
}

/// Whether a single replica counts as READY, factored out of the per-replica
/// loop in `reconcile_health` so the `serverStatus` selection is unit-testable
/// without a daemon. Inputs:
///
/// - `router_withheld`: the project's Traefik router is withheld (unknown access
///   class, or auth required without an `auth_backend_url`). Traefik then never
///   routes to the container, so it can never be Ready — fails closed regardless
///   of the other inputs, so a misconfigured deploy surfaces as not-Healthy
///   instead of superseding a working one while serving no traffic.
/// - `has_health_path`: the container has an effective health path;
/// - `running`: the live container is `running` on the daemon;
/// - `api_available`: a Traefik API client is configured AND its serverStatus
///   call for this container's service(s) succeeded;
/// - `server_up`: when `api_available`, whether Traefik reports this container's
///   server URL UP (`None` = absent from the map / no IP yet).
///
/// A withheld router short-circuits to `NotReady`; otherwise this is a thin
/// wrapper over [`rolling_rotation_decision`]: in-rotation → `Ready`, everything
/// else → `NotReady` with the rotation reason.
#[derive(Debug, Clone, PartialEq)]
pub enum ReadyVerdict {
    Ready,
    NotReady(String),
}

pub fn replica_ready(
    router_withheld: bool,
    has_health_path: bool,
    running: bool,
    api_available: bool,
    server_up: Option<bool>,
) -> ReadyVerdict {
    if router_withheld {
        return ReadyVerdict::NotReady(
            "router withheld (access class is unknown or required authentication cannot be wired)"
                .to_string(),
        );
    }
    match rolling_rotation_decision(has_health_path, running, api_available, server_up) {
        RotationDecision::InRotation => ReadyVerdict::Ready,
        RotationDecision::NotInRotation(reason) => ReadyVerdict::NotReady(reason),
    }
}

/// Outcome of the in-rotation decision for a single container.
#[derive(Debug, Clone, PartialEq)]
pub enum RotationDecision {
    /// The container's server is in Traefik's rotation (or running, for a
    /// ready-when-running container) → counts toward deployment readiness.
    InRotation,
    /// The container is NOT in rotation; the string is a human-readable reason
    /// for the deployment's not-ready diagnostic.
    NotInRotation(String),
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
/// Rules:
/// - `!has_health_path` → ready-when-running: in-rotation IFF the container is
///   `running` (Traefik routes to running servers immediately; with no health
///   check it publishes no `serverStatus`, so run-state is the only signal).
/// - `has_health_path` → Traefik's `serverStatus` is AUTHORITATIVE with no
///   fallback: in-rotation IFF the server is reported UP. A DOWN/absent server,
///   OR an unavailable Traefik signal (`!api_available` — API unset/unreachable
///   or the service not yet registered) → NOT in rotation. A health-checked
///   container is "ready" only once Traefik is actually routing to it, so the
///   old deployment is never retired while the new server is invisible to
///   Traefik (no traffic) — which is exactly what an unregistered server is.
///
/// Kept pure (no `self`, no I/O) so it can be unit-tested without a daemon.
pub fn rolling_rotation_decision(
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
    // health_check configured → Traefik's serverStatus is authoritative.
    if !api_available {
        // No Traefik signal (API unset/unreachable, or the service is not yet
        // registered). Traefik isn't routing to this server, so it is NOT ready.
        return RotationDecision::NotInRotation(
            "Traefik serverStatus unavailable (API unset/unreachable or service not yet \
             registered)"
                .to_string(),
        );
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
    use rise_backend_core::desired::DesiredRoute;
    use rise_backend_core::test_helpers::desired;

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
        use rise_deployment_spec::request_spec::{ContainerSpec, RouteSpec};
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
            access: None,
        }];
        // A single-route container uses the bare base name — exactly what the
        // labels stamp (`deployment_service_names` over the matching DesiredContainer).
        // The base carries the injective deployment hash suffix
        // appends, so derive it the same way rather than hardcoding the hash.
        let deployment_id = "20260902-120000";
        let base = naming::deployment_service_base("myapp", "default", deployment_id, "app");
        let hosts = ["myapp.rise.dev".to_string()];
        assert_eq!(
            service_names_for_spec("myapp", "default", deployment_id, &spec, &routes, &hosts),
            vec![base]
        );
    }

    #[test]
    fn service_names_for_spec_multi_route_uses_per_route_indices() {
        use rise_deployment_spec::request_spec::{ContainerSpec, RouteSpec};
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
                access: None,
            },
            RouteSpec {
                path: "/api/v1".to_string(),
                container: "api".to_string(),
                access: None,
            },
            RouteSpec {
                path: "/".to_string(),
                container: "other".to_string(),
                access: None,
            },
        ];
        // Per-route services, longest path-prefix first (-0 = /api/v1, -1 = /),
        // matching the renderer's `{base}-{idx}` services for a multi-route
        // container. The bare base is NOT queried (it 404s in the Traefik API).
        // The base carries the injective hash suffix, so derive the per-route
        // names via `group_service_name` rather than hardcoding the hash.
        let deployment_id = "20260902-120000";
        let base = naming::deployment_service_base("myapp", "default", deployment_id, "api");
        let hosts = ["myapp.rise.dev".to_string()];
        assert_eq!(
            service_names_for_spec("myapp", "default", deployment_id, &spec, &routes, &hosts),
            vec![
                naming::group_service_name(&base, 0, 2),
                naming::group_service_name(&base, 1, 2),
            ]
        );
    }

    #[test]
    fn service_names_for_spec_derivation_matches_container_builder() {
        // The reconciler-side derivation must agree with the builder-side
        // `deployment_service_names` (the labels) for the same routes — single AND
        // multi-route — so the lookup never drifts from what's stamped.
        use crate::naming::deployment_service_names;
        use rise_deployment_spec::request_spec::{ContainerSpec, RouteSpec};

        let mk_desired = |container: &str, paths: &[&str]| {
            let mut d = desired(container, "img:1", "h1");
            d.routes = paths
                .iter()
                .map(|p| DesiredRoute {
                    hosts: vec!["myapp.rise.dev".to_string()],
                    path_prefix: Some(p.to_string()),
                    access: None,
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
                    access: None,
                })
                .collect();
            (spec, routes)
        };

        // The desired routes the labels are derived from all carry this host, so
        // the runtime derivation must be given the same host set to agree.
        let hosts = ["myapp.rise.dev".to_string()];
        for paths in [&["/"][..], &["/", "/api/v1"][..], &["/", "/a", "/bb"][..]] {
            let (spec, routes) = mk_spec_routes("app", paths);
            let desired = mk_desired("app", paths);
            let from_spec = service_names_for_spec(
                "myapp",
                "default",
                &desired.deployment_id,
                &spec,
                &routes,
                &hosts,
            );
            let from_labels = deployment_service_names(&desired);
            assert_eq!(
                from_spec, from_labels,
                "service-name derivation must match the labels for paths {paths:?}"
            );
        }
    }

    #[test]
    fn service_names_for_spec_worker_has_no_services() {
        use rise_deployment_spec::request_spec::ContainerSpec;
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
        assert!(service_names_for_spec(
            "myapp",
            "default",
            "20260902-120000",
            &worker,
            &[],
            &hosts
        )
        .is_empty());
    }

    #[test]
    fn service_names_for_spec_hostless_yields_empty() {
        // A routable HTTP container whose deployment resolved NO ingress host
        // emits no Traefik router (the label renderer gates each route on a
        // non-empty host set), so the reconciler must query NO service — otherwise
        // it would hit `serverStatus` for a service Traefik never registered and
        // log a misleading WARN every tick. Mirrors `render_traefik_labels_for`.
        use rise_deployment_spec::request_spec::{ContainerSpec, RouteSpec};
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
            access: None,
        }];
        // Same spec/routes that yield a service WITH a host — but with no host the
        // result is empty.
        assert!(
            service_names_for_spec("myapp", "default", "20260902-120000", &spec, &routes, &[])
                .is_empty(),
            "no routable host → no service names"
        );
    }

    #[test]
    fn replica_ready_server_up_is_ready() {
        // HC + Traefik UP → Ready.
        assert_eq!(
            replica_ready(false, true, true, true, Some(true)),
            ReadyVerdict::Ready
        );
    }

    #[test]
    fn replica_ready_server_down_is_not_ready() {
        match replica_ready(false, true, true, true, Some(false)) {
            ReadyVerdict::NotReady(reason) => assert!(reason.contains("DOWN"), "got: {reason}"),
            other => panic!("expected NotReady, got {other:?}"),
        }
    }

    #[test]
    fn replica_ready_absent_server_is_not_ready() {
        match replica_ready(false, true, true, true, None) {
            ReadyVerdict::NotReady(reason) => {
                assert!(reason.contains("serverStatus"), "got: {reason}")
            }
            other => panic!("expected NotReady, got {other:?}"),
        }
    }

    #[test]
    fn replica_ready_api_unavailable_is_not_ready() {
        // HC but no Traefik signal (API unset/unreachable or service not yet
        // registered) → NOT ready, with no fallback. `running` is irrelevant.
        match replica_ready(false, true, true, false, None) {
            ReadyVerdict::NotReady(reason) => {
                assert!(reason.contains("serverStatus"), "got: {reason}")
            }
            other => panic!("expected NotReady, got {other:?}"),
        }
    }

    #[test]
    fn replica_ready_no_health_check_is_ready_when_running() {
        // No HC → ready-when-running; Traefik signal ignored.
        assert_eq!(
            replica_ready(false, false, true, false, None),
            ReadyVerdict::Ready
        );
        assert!(matches!(
            replica_ready(false, false, false, false, None),
            ReadyVerdict::NotReady(_)
        ));
    }

    #[test]
    fn replica_ready_router_withheld_is_never_ready() {
        // A withheld Traefik router (unknown access class or unavailable auth
        // backend) means Traefik never routes to the container, so it is never
        // Ready — regardless of run-state, health check, or serverStatus.
        // Without this a ready-when-running container would go Healthy and
        // supersede a working deployment while serving no traffic.
        for (has_health_path, running, api_available, server_up) in [
            (false, true, false, None),      // ready-when-running, would be Ready
            (true, true, true, Some(true)),  // HC + Traefik UP, would be Ready
            (false, true, true, Some(true)), // running + UP, would be Ready
        ] {
            match replica_ready(true, has_health_path, running, api_available, server_up) {
                ReadyVerdict::NotReady(reason) => {
                    assert!(reason.contains("router withheld"), "got: {reason}")
                }
                other => panic!("expected NotReady (router withheld), got {other:?}"),
            }
        }
    }

    #[test]
    fn rotation_health_check_api_unavailable_is_not_in_rotation() {
        // HC configured but Traefik API unset/errored (or service not yet
        // registered) → NOT in rotation, no fallback. `running`/`server_up` are
        // irrelevant: without a Traefik signal the server isn't receiving traffic.
        match rolling_rotation_decision(true, true, false, None) {
            RotationDecision::NotInRotation(reason) => {
                assert!(reason.contains("serverStatus"), "got: {reason}")
            }
            other => panic!("expected NotInRotation, got {other:?}"),
        }
        assert!(matches!(
            rolling_rotation_decision(true, false, false, Some(true)),
            RotationDecision::NotInRotation(_)
        ));
    }
}
