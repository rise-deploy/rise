//! Rendering a [`DesiredContainer`] into deployment-scoped native-provider
//! service labels, plus the fail-closed predicate that decides whether a
//! container may participate in public routing.
//!
//! Shared by every backend that fronts workloads with Traefik. The label keys are
//! provider-agnostic; only [`TraefikRenderConfig::network`] differs (the Docker
//! provider needs `traefik.docker.network`, the ECS provider has no analogue and
//! must not receive one).

use std::collections::HashMap;

use rise_deployment_spec::AccessRequirement;

use rise_backend_core::desired::DesiredContainer;
use rise_backend_core::effective_access_requirement;

use crate::labels;

/// Default Traefik health-check interval (Go duration `10s`) when the
/// `health_check` spec sets no `period_seconds`.
const DEFAULT_HEALTHCHECK_INTERVAL_SECS: i32 = 10;
/// Default Traefik health-check timeout (Go duration `5s`) when the
/// `health_check` spec sets no `timeout_seconds`. Matches the Kubernetes
/// default (`HealthProbeConfig::timeout_seconds` = 5 in
/// `resource_builder::create_http_probe_with_override`) so the same public
/// input — a `health_check` with no explicit `timeout_seconds` — yields the
/// same effective timeout on both backends (no Docker-stricter divergence
/// that could mark a slow-but-healthy endpoint DOWN on Docker but UP on K8s).
const DEFAULT_HEALTHCHECK_TIMEOUT_SECS: i32 = 5;

use crate::naming::{deployment_service_base, group_service_name};

/// Static configuration the Traefik label renderer needs, independent of any one
/// runtime. Backends build this from their own richer config struct.
pub struct TraefikRenderConfig<'a> {
    pub label_namespace: &'a str,
    pub controller_class: &'a str,
    pub traefik_entrypoint: &'a str,
    /// Loopback-only entrypoint used to materialize native-provider services
    /// without allowing the provider to synthesize a public default router.
    pub catalog_entrypoint: &'a str,
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
            effective_access_requirement(route_override, project_requirement),
            AccessRequirement::None
        )
    })
}

/// Render the full Traefik label map for a desired container.
///
/// Empty when the container is not routable, has no port (worker), or has no
/// host to route. Each infra-bearing deployment publishes a deployment-scoped
/// native-provider service for the HTTP provider to select. `routable` is
/// `false` only when the router is
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
    match cfg.access_classes.get(&desired.access_class) {
        Some(_) => {}
        None => {
            tracing::error!(
                project = %desired.project,
                access_class = %desired.access_class,
                "Access class not configured — withholding all Traefik routers \
                 to avoid a silent public route"
            );
            return out;
        }
    }

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
    // Deployment-scoped service names keep old and incoming server pools
    // independent. Distinct per-route service names (`{base}-{idx}` when
    // there is more than one route) keep multiple path prefixes from colliding;
    // [`group_service_name`] is the single source for that derivation.
    let route_count = routes.len();
    let base = deployment_service_base(
        &desired.project,
        &desired.deployment_group,
        &desired.deployment_id,
        &desired.container,
    );
    for (idx, route) in routes.iter().enumerate() {
        if route.hosts.is_empty() {
            continue;
        }
        let router_name = group_service_name(&base, idx, route_count);
        // Native providers automatically create a public default router when a
        // service has no router labels. Define an explicit catalog router on a
        // loopback-only entrypoint so the provider still owns server discovery
        // while the HTTP provider owns every public route.
        out.insert("traefik.enable".to_string(), "true".to_string());
        out.insert(
            format!("traefik.http.routers.{router_name}-catalog.rule"),
            "PathPrefix(`/`)".to_string(),
        );
        out.insert(
            format!("traefik.http.routers.{router_name}-catalog.entrypoints"),
            cfg.catalog_entrypoint.to_string(),
        );
        out.insert(
            format!("traefik.http.routers.{router_name}-catalog.service"),
            router_name.clone(),
        );
        out.insert(
            format!("traefik.http.services.{router_name}.loadbalancer.server.port"),
            port.to_string(),
        );
        if let Some(network) = cfg.network {
            out.insert("traefik.docker.network".to_string(), network.to_string());
        }

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
                .unwrap_or(DEFAULT_HEALTHCHECK_INTERVAL_SECS);
            let timeout = desired
                .health_check_timeout_secs
                .unwrap_or(DEFAULT_HEALTHCHECK_TIMEOUT_SECS);
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
/// building the full create spec. Used by the reconciler's diff so a native
/// routing-label change or a change in whether the app port is published to a
/// loopback host port forces a recreate.
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
