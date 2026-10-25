//! The fully-resolved description of a container Rise wants running.
//!
//! Computed by each backend's reconciler and consumed by that backend's
//! runtime-specific builder (`build_container` on Docker, the task-definition
//! builder on ECS). Backend-agnostic: it names the desired state, not how any
//! one runtime realizes it.

use rise_deployment_spec::AccessRequirement;

/// One ingress route attached to a routable container.
#[derive(Debug, Clone)]
pub struct DesiredRoute {
    /// Hosts that resolve to this container, priority order.
    pub hosts: Vec<String>,
    /// Optional path prefix (`None` / `/` → host-only).
    pub path_prefix: Option<String>,
    /// Per-route ingress auth requirement override (`.rise.toml` `[routes].access`).
    /// `None` means the route inherits the project's access-class requirement; the
    /// effective requirement decides this router's forwardAuth middleware.
    pub access: Option<AccessRequirement>,
}

/// Fully-resolved description of a single container Rise wants running. Built by
/// the reconciler; consumed by [`build_container`]. All identity fields are
/// owned strings so the struct can be tested without a live deployment row.
#[derive(Debug, Clone)]
pub struct DesiredContainer {
    pub project: String,
    /// The project's immutable identity, stamped alongside `project` so a
    /// reconciler can recognise a workload after the project is renamed. The
    /// project name is mutable and is not a safe matching key on its own: a
    /// rename leaves previously-created workloads tagged with the old name,
    /// invisible to any lookup keyed on the current one.
    pub project_uuid: String,
    /// Name of the project's access class (looked up in
    /// [`BuilderConfig::access_classes`] to decide whether to stamp Traefik
    /// forwardAuth middleware labels).
    pub access_class: String,
    pub deployment_group: String,
    pub deployment_id: String,
    pub deployment_uuid: String,
    /// Container name within the deployment (`app` for single-container).
    pub container: String,
    pub environment: Option<String>,
    pub image: String,
    /// `None` for workers (no Service / routing / probe).
    pub port: Option<u16>,
    pub cpu: String,
    pub memory: String,
    /// Merged env vars as `(KEY, VALUE)` pairs, already in final precedence.
    pub env: Vec<(String, String)>,
    /// sha256 of the *entire* merged env (plain + system/RISE_* + secret),
    /// computed over a deterministically-sorted copy. Drift here forces
    /// recreation, so editing/deleting any env var of any kind recreates the
    /// container. See [`super::reconciler::hash_env`].
    pub env_hash: String,
    /// Routes for this container (empty for workers / unrouted containers).
    pub routes: Vec<DesiredRoute>,
    /// Whether this container should be routable (emit its deployment-scoped
    /// native-provider Traefik service). `true` for every infra-bearing
    /// deployment whose route can be exposed safely. `false` when the router would be
    /// withheld (unknown access class, or auth required without an
    /// `auth_backend_url`), so the readiness path doesn't report a never-routed
    /// container as Healthy.
    pub routable: bool,
    /// Recreate-signature hash: sha256 of the fully-rendered Traefik label set
    /// for this container PLUS whether its app port is published to a loopback
    /// host port (`publish_app_ports`). Precomputed by the reconciler via
    /// [`route_hash_for`] so the (pure) diff can compare it against the
    /// `route-hash` bookkeeping label stamped on the actual container, and force
    /// a recreate when either changes — a deployment becoming/ceasing to be
    /// active, or the published-port binding being added/removed. Docker can't
    /// mutate a running container's labels or port bindings in place, so such a
    /// transition must be reconciled by recreation.
    pub route_hash: String,
    /// Resolved monotonic generation for this container's `--name` suffix
    /// (`..._g{n}`). NOT an identity field and NOT fed into routing or any hash.
    /// `compute_desired_for_deployment` can't know it (it depends on the live
    /// container's current generation), so it seeds `1` as a placeholder and the
    /// diff resolves the real value before apply: a brand-new slot → `1`, a
    /// recreate → live `g{n}` + 1.
    pub generation: u32,
    /// Zero-based replica index of this container within its spec (`0..N`). IS an
    /// identity field (folded into [`identity_key`] and the `..._r{n}` name
    /// segment) so each replica is matched/recreated independently. Deliberately
    /// NOT fed into routing/recreate hashes, the network alias, or the Traefik
    /// labels: all replicas of a spec carry one replica-free alias (Docker DNS
    /// round-robins) and one router+service (Traefik load-balances).
    pub replica: u32,
    /// Effective HTTP health-probe path for this container (already resolved from
    /// the spec's `health_check`): `Some("/healthz")` to probe, or `None` when the
    /// probe is disabled or the container is a port-less worker. Drives the
    /// Traefik load-balancer health-check labels (and is thus folded into the
    /// rendered label set the route-hash covers, so changing the effective health
    /// path forces a recreate), and gates the rolling-recreate throttle's
    /// requirement that every OTHER replica be healthy before a running drifted
    /// replica is recreated. NOT an identity field.
    pub health_path: Option<String>,
    /// Traefik load-balancer health-check interval, in seconds (the spec's
    /// `period_seconds` when set). Only consulted when `health_path` is `Some`, to
    /// render `...loadbalancer.healthcheck.interval`. `None` → a sensible default.
    pub health_check_interval_secs: Option<i32>,
    /// Traefik load-balancer health-check timeout, in seconds (the spec's
    /// `timeout_seconds` when set). Same gating as `health_check_interval_secs`;
    /// renders `...loadbalancer.healthcheck.timeout`. `None` → a sensible
    /// default.
    pub health_check_timeout_secs: Option<i32>,
}
