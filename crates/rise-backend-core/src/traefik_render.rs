//! Rendering a [`DesiredContainer`] into the Traefik dynamic-configuration label
//! set, plus the fail-closed predicate that decides whether a container may
//! advertise a router at all.
//!
//! Shared by every backend that fronts workloads with Traefik. The label keys are
//! provider-agnostic; only [`TraefikRenderConfig::network`] differs (the Docker
//! provider needs `traefik.docker.network`, the ECS provider has no analogue and
//! must not receive one).

use std::collections::HashMap;

use rise_deployment_spec::AccessRequirement;

use crate::desired::{DesiredContainer, RouteForwardAuth};
use crate::labels::{self, ForwardAuth, TraefikRoute};
use crate::naming::{group_service_base, group_service_name};

/// Static configuration the Traefik label renderer needs, independent of any one
/// runtime. Backends build this from their own richer config struct.
pub struct TraefikRenderConfig<'a> {
    pub label_namespace: &'a str,
    pub controller_class: &'a str,
    pub traefik_entrypoint: &'a str,
    pub traefik_certresolver: Option<&'a str>,
    /// Value for `traefik.docker.network`. `Some` for the Docker provider;
    /// **`None` for the ECS provider**, which resolves task ENIs itself and
    /// mis-resolves if handed a Docker network name.
    pub network: Option<&'a str>,
    /// Internal URL Traefik uses for the forwardAuth subrequest. Empty disables
    /// forwardAuth (and, for a route that requires auth, withholds the router).
    pub auth_backend_url: &'a str,
    pub access_classes: &'a HashMap<String, AccessRequirement>,
}

/// public project tightens only one route.
pub fn routes_withheld<'a>(
    access_class: &str,
    access_classes: &HashMap<String, AccessRequirement>,
    auth_backend_url: &str,
    route_overrides: impl IntoIterator<Item = Option<&'a AccessRequirement>>,
) -> bool {
    let mut route_overrides = route_overrides.into_iter().peekable();
    // A removed/renamed class is a control-plane configuration error. Route
    // overrides must never weaken that failure into public access: Kubernetes
    // emits no ingress for the same state, so Docker withholds every router too.
    let Some(project_requirement) = access_classes.get(access_class) else {
        return route_overrides.peek().is_some();
    };
    if !auth_backend_url.trim().is_empty() {
        return false;
    }
    route_overrides.any(|route_override| {
        !matches!(
            crate::effective_access_requirement(route_override, project_requirement),
            AccessRequirement::None
        )
    })
}

/// Render the full Traefik label map for a desired container.
///
/// Empty when the container is not routable, has no port (worker), or has no
/// host to route. Every infra-bearing deployment of a group is routable — the
/// old active and the new Deploying deployment both advertise a router on the
/// shared `Host(...)` rule and join the one group-scoped Traefik service, and
/// Traefik's per-server health check drains the old servers as the new ones come
/// up (the rolling overlap). `routable` is `false` only when the router is
/// withheld (unknown access class, or auth required without an
/// `auth_backend_url`), so a misconfigured deployment never advertises an
/// unauthenticated router.
pub fn render_traefik_labels_for(
    desired: &DesiredContainer,
    cfg: &TraefikRenderConfig<'_>,
) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Some(port) = desired.port.filter(|_| desired.routable) else {
        return out;
    };

    // Resolve the project's access requirement. For Authenticated/Member, and
    // when an internal backend URL is configured, build the Traefik forwardAuth
    // address pointing at Rise's ingress-auth endpoint. `signin_redirect=1` puts
    // the handler in Traefik mode (302 to the login page on unauthenticated,
    // since Traefik has no nginx-style auth-signin). The project name is
    // URL-encoded into the query. `None` leaves forward_auth unset → no auth.
    //
    // FAIL CLOSED on an unknown access class: a project referencing a
    // missing/removed class must NOT silently become public (mirroring the K8s
    // path, which errors on an unknown class). Route overrides cannot weaken a
    // missing class, so suppress every router rather than inventing a default.
    let requirement = match cfg.access_classes.get(&desired.access_class) {
        Some(req) => req.clone(),
        None => {
            tracing::error!(
                project = %desired.project,
                access_class = %desired.access_class,
                "Access class not configured — withholding all Traefik routers \
                 to avoid a silent public route"
            );
            return out;
        }
    };
    // Per-route effective requirement decides that router's forwardAuth. The
    // address stamps `&access=<req>` so the shared `ingress_auth` handler enforces
    // exactly this route group's requirement (never re-matching the request path).
    // `signin_redirect=1` puts the handler in Traefik mode (302 to login on
    // unauthenticated). A `None` route gets no middleware (open); an auth route on
    // a project whose `auth_backend_url` is empty is withheld per-route below.
    let route_forward_auth = |route_requirement: &AccessRequirement| -> RouteForwardAuth {
        match route_requirement {
            AccessRequirement::None => RouteForwardAuth::Open,
            AccessRequirement::Authenticated | AccessRequirement::Member => {
                if cfg.auth_backend_url.trim().is_empty() {
                    RouteForwardAuth::Withheld
                } else {
                    RouteForwardAuth::Gated(format!(
                        "{}/api/v1/auth/ingress?project={}&access={}&signin_redirect=1",
                        cfg.auth_backend_url.trim().trim_end_matches('/'),
                        urlencoding::encode(&desired.project),
                        route_requirement.as_query_param(),
                    ))
                }
            }
        }
    };

    // FAIL CLOSED: an unknown access class or an auth-required route without a
    // forwardAuth address must expose nothing. Stamping a router in either state
    // could turn a control-plane error into public access.
    if routes_withheld(
        &desired.access_class,
        cfg.access_classes,
        cfg.auth_backend_url,
        desired
            .routes
            .iter()
            .filter(|route| !route.hosts.is_empty())
            .map(|route| route.access.as_ref()),
    ) {
        tracing::warn!(
            project = %desired.project,
            access_class = %desired.access_class,
            "Withholding Traefik router(s): access class is unknown or forwardAuth \
             could not be wired — refusing to expose an unauthenticated public route"
        );
        return out;
    }

    // One router per (host-set × route). Single-container apps have a single
    // `/` route, so this yields exactly one router. Longest path-prefix first
    // matches the nginx semantics used by the K8s path.
    let mut routes = desired.routes.clone();
    routes.sort_by(|a, b| {
        let al = a.path_prefix.as_deref().unwrap_or("/").len();
        let bl = b.path_prefix.as_deref().unwrap_or("/").len();
        bl.cmp(&al)
    });
    // GROUP-scoped router/service base name — deployment-id-FREE, shared with
    // the reconciler's `serverStatus` lookup via [`group_service_base`] so the
    // two can't drift. Distinct per-route service names (`{base}-{idx}` when
    // there is more than one route) keep multiple path prefixes from colliding;
    // [`group_service_name`] is the single source for that derivation.
    let route_count = routes.len();
    let base = group_service_base(
        &desired.project,
        &desired.deployment_group,
        &desired.container,
    );
    for (idx, route) in routes.iter().enumerate() {
        if route.hosts.is_empty() {
            continue;
        }
        // Effective requirement: the route's `access` override, else the
        // project's (already failed-closed to Member on an unknown class).
        let route_requirement = route.access.clone().unwrap_or_else(|| requirement.clone());
        let forward_auth_address = match route_forward_auth(&route_requirement) {
            RouteForwardAuth::Open => None,
            RouteForwardAuth::Gated(address) => Some(address),
            // Unreachable: the pre-loop scan above withholds the whole container
            // if any route is Withheld. Fail closed defensively regardless.
            RouteForwardAuth::Withheld => continue,
        };
        let router_name = group_service_name(&base, idx, route_count);
        let traefik = labels::render_traefik_labels(&TraefikRoute {
            router_name: &router_name,
            hosts: &route.hosts,
            path_prefix: route.path_prefix.as_deref(),
            port,
            entrypoint: cfg.traefik_entrypoint,
            network: cfg.network,
            certresolver: cfg.traefik_certresolver,
            forward_auth: forward_auth_address.as_deref().map(|address| ForwardAuth {
                address,
                auth_response_headers: "X-Auth-Request-Email,X-Auth-Request-User",
            }),
        });
        out.extend(traefik);

        // Explicit longest-prefix-first router priority (parity with nginx's
        // implicit longest-match). Traefik would otherwise fall back to implicit
        // rule-LENGTH priority, which conflates host-rule length with path
        // specificity; deriving the priority from the route's path-prefix length
        // makes a more-specific prefix (`/api/v1`) deterministically outrank a
        // shorter/host-only one (`/`). The `+1` keeps even a host-only (empty/`/`)
        // route at a positive, non-zero priority.
        let path_len = route
            .path_prefix
            .as_deref()
            .filter(|p| !p.is_empty() && *p != "/")
            .map(str::len)
            .unwrap_or(0);
        out.insert(
            format!("traefik.http.routers.{router_name}.priority"),
            (path_len + 1).to_string(),
        );

        // Traefik load-balancer health-check labels — emitted when this container
        // has an effective health path (a `health_check` is configured). A
        // ready-when-running container (no `health_check`) emits none. The service
        // name matches the router/service base (`router_name`), so the check
        // attaches to this route's service. These labels are part of the rendered
        // set, so toggling them changes the route-hash and forces a recreate
        // automatically.
        if let Some(health_path) = desired.health_path.as_deref() {
            let interval = desired
                .health_check_interval_secs
                .unwrap_or(crate::desired::DEFAULT_HEALTHCHECK_INTERVAL_SECS);
            let timeout = desired
                .health_check_timeout_secs
                .unwrap_or(crate::desired::DEFAULT_HEALTHCHECK_TIMEOUT_SECS);
            out.insert(
                format!("traefik.http.services.{router_name}.loadbalancer.healthcheck.path"),
                health_path.to_string(),
            );
            out.insert(
                format!("traefik.http.services.{router_name}.loadbalancer.healthcheck.interval"),
                format!("{interval}s"),
            );
            out.insert(
                format!("traefik.http.services.{router_name}.loadbalancer.healthcheck.timeout"),
                format!("{timeout}s"),
            );
            out.insert(
                format!("traefik.http.services.{router_name}.loadbalancer.healthcheck.scheme"),
                "http".to_string(),
            );
        }
    }
    out
}

/// Compute the `route-hash` recreate signature for a desired container without
/// building the full create spec. Used by the reconciler's diff so a routing
/// transition (a deployment becoming or ceasing to be active) OR a change in
/// whether the app port is published to a loopback host port forces a recreate.
/// Must stay consistent with the hash stamped by [`build_container`].
///
/// `publish_port` folds the Docker backend's `publish_app_ports` binding into the
/// signature (a create-time-only property there). Backends with no such concept —
/// ECS — pass `false`.
pub fn route_hash_for(
    desired: &DesiredContainer,
    cfg: &TraefikRenderConfig<'_>,
    publish_port: bool,
) -> String {
    labels::hash_recreate_signature(&render_traefik_labels_for(desired, cfg), publish_port)
}
