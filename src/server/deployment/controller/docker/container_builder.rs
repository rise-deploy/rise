//! Pure `Deployment` → bollard create-spec mapping.
//!
//! Everything here is deterministic and daemon-free so it can be unit-tested:
//! the reconciler resolves the dynamic inputs (env vars, image, URLs) and calls
//! [`build_container`] to produce the [`bollard::container::Config`] plus the
//! container name and labels.

use std::collections::HashMap;

use bollard::container::Config;
use bollard::secret::{EndpointSettings, HostConfig, RestartPolicy, RestartPolicyNameEnum};

use super::labels::{self, BookkeepingLabels, ForwardAuth, TraefikRoute};
use crate::server::deployment::quantity::{parse_cpu_millicores, parse_memory_bytes};
use crate::server::settings::AccessRequirement;

/// One ingress route attached to a routable container.
#[derive(Debug, Clone)]
pub struct DesiredRoute {
    /// Hosts that resolve to this container, priority order.
    pub hosts: Vec<String>,
    /// Optional path prefix (`None` / `/` → host-only).
    pub path_prefix: Option<String>,
}

/// Fully-resolved description of a single container Rise wants running. Built by
/// the reconciler; consumed by [`build_container`]. All identity fields are
/// owned strings so the struct can be tested without a live deployment row.
#[derive(Debug, Clone)]
pub struct DesiredContainer {
    pub project: String,
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
    /// Whether this container should be routable (emit Traefik router/service
    /// labels). `true` only for the *active* deployment of its group (mirroring
    /// the K8s path, which builds the Ingress from `is_active` deployments).
    /// Everything else — `Deploying`/`Pushed`/superseded/`Terminating` — is
    /// `false` so its `Host(...)` rule is dropped and only the active deployment
    /// is routed.
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
    /// probe is disabled or the container is a port-less worker. Used by the
    /// rolling-recreate throttle to gate a running drifted replica's recreate on
    /// every OTHER replica being healthy. NOT an identity field and NOT fed into
    /// any hash (so it never causes drift on its own).
    pub health_path: Option<String>,
}

/// Static controller configuration the builder needs.
pub struct BuilderConfig<'a> {
    pub label_namespace: &'a str,
    pub controller_class: &'a str,
    pub container_prefix: &'a str,
    pub traefik_network: &'a str,
    pub traefik_entrypoint: &'a str,
    pub traefik_certresolver: Option<&'a str>,
    /// Internal URL Traefik uses to reach the Rise backend for the forwardAuth
    /// subrequest (e.g. `http://rise:3000`). Empty disables forwardAuth.
    pub auth_backend_url: &'a str,
    /// Access-class name → access requirement. Used to resolve a project's
    /// access requirement and decide whether to stamp forwardAuth labels.
    pub access_classes: &'a HashMap<String, AccessRequirement>,
    /// **LOCAL-DEV ONLY.** Hostname(s) to alias to `app_backend_ip` via the
    /// container's `HostConfig.extra_hosts`, so an app can reach the public
    /// issuer host (e.g. `rise.localhost`) at the Rise backend to validate the
    /// `rise_jwt` cookie / perform OIDC discovery. Empty in production, where
    /// public DNS + Traefik resolve the issuer (with correct TLS). Injection
    /// only happens when this is non-empty AND `app_backend_ip` is set.
    pub app_backend_host_aliases: &'a [String],
    /// The Rise backend's IP on the shared network (resolved at reconcile
    /// startup). Paired with `app_backend_host_aliases` to build `extra_hosts`.
    /// `None` disables injection.
    pub app_backend_ip: Option<&'a str>,
    /// **DEV-ONLY.** When `true`, each routable container with a `port` also
    /// publishes that port to a random `127.0.0.1` host port (empty `host_port`
    /// → Docker assigns a free one, bound to loopback). Lets a host-run backend
    /// (Docker Desktop, where the container bridge IP isn't routable from the
    /// host) health-probe the app directly. Off in production — worker
    /// containers (no port) and the disabled case get no port bindings.
    pub publish_app_ports: bool,
}

/// Result of building one container: the deterministic name and the bollard
/// create config (which already carries the network attachment).
pub struct BuiltContainer {
    pub name: String,
    pub config: Config<String>,
}

/// Maximum length of a Docker container name segment we emit before hashing.
const MAX_NAME_LEN: usize = 63;

/// Compute the live container's `--name`:
/// `<prefix>_<project>_<group>_<deploymentid>_<container>_r<replica>_g<generation>`,
/// sanitized to `[a-zA-Z0-9_.-]`, hash-suffixed when longer than
/// [`MAX_NAME_LEN`]. The `_r{n}_g{n}` suffix is folded into the raw string BEFORE
/// the length cap, so the >63-char hash-truncation branch still caps the whole
/// string (suffix included). The replica index keeps each replica's name
/// distinct; the generation makes a recreated container's name visibly newer
/// than the one it replaced. Matching is by bookkeeping LABELS (the stable
/// identity tuple including the replica), never by this name — see
/// [`group_app_name`] for the group-scoped, deployment-id-free identity used by
/// DNS / env.
pub fn container_name(
    prefix: &str,
    project: &str,
    deployment_group: &str,
    deployment_id: &str,
    container: &str,
    replica: u32,
    generation: u32,
) -> String {
    let raw = format!(
        "{prefix}_{project}_{deployment_group}_{deployment_id}_{container}_r{replica}_g{generation}"
    );
    sanitize_and_cap(&raw)
}

/// Replica- and generation-FREE stable identity name:
/// `<prefix>_<project>_<group>_<deploymentid>_<container>`, sanitized + capped
/// the same way as [`container_name`] but with no `_r{n}` / `_g{n}` suffix.
///
/// Still deployment-id-BEARING (unlike [`group_app_name`]). Used only to
/// synthesize a stable per-replica placeholder pod name for diagnostics when a
/// replica has no live container yet (the diff appends `_r{n}`). The DNS-facing
/// names — the network alias and the `RISE_CONTAINER_HOST__<NAME>` discovery host
/// — are GROUP-scoped now (see [`group_app_name`]) so they stay stable across
/// deployments, not just across replicas/generations of one deployment.
pub fn stable_identity_name(
    prefix: &str,
    project: &str,
    deployment_group: &str,
    deployment_id: &str,
    container: &str,
) -> String {
    let raw = format!("{prefix}_{project}_{deployment_group}_{deployment_id}_{container}");
    sanitize_and_cap(&raw)
}

/// Group-scoped, deployment-id-FREE application name:
/// `<prefix>_<project>_<group>_<container>`, sanitized + capped exactly like
/// [`container_name`] but with NO deployment-id, replica, or generation segment.
///
/// This is the stable, deployment-id-free name shared by ALL of a group's
/// deployments and replicas. EVERY container that belongs to a (project, group,
/// container) — regardless of which deployment created it — attaches this same
/// name as its Docker NETWORK ALIAS, so Docker's embedded DNS ROUND-ROBINS the
/// alias across whatever containers currently carry it (matching the Kubernetes
/// group Service, whose name is likewise deployment-id-free). It is also the
/// `RISE_CONTAINER_HOST__<NAME>` sibling-discovery host. Because the name is
/// stable across deployments, an old and a new deployment of the same group can
/// share one DNS name during a rolling overlap — foundational for
/// health-driven rolling-overlap routing.
pub fn group_app_name(
    prefix: &str,
    project: &str,
    deployment_group: &str,
    container: &str,
) -> String {
    let raw = format!("{prefix}_{project}_{deployment_group}_{container}");
    sanitize_and_cap(&raw)
}

/// Sanitize a raw name to `[a-zA-Z0-9_.-]` and hash-truncate it when it exceeds
/// [`MAX_NAME_LEN`]. Shared by [`container_name`], [`stable_identity_name`] and
/// [`group_app_name`] so the 63-char cap logic lives in one place.
/// Deterministic: same input → same output.
fn sanitize_and_cap(raw: &str) -> String {
    let sanitized = sanitize_name(raw);
    if sanitized.len() <= MAX_NAME_LEN {
        return sanitized;
    }
    // Hash the full sanitized name and truncate the prefix to leave room for a
    // short stable suffix. Deterministic: same inputs → same name.
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(sanitized.as_bytes());
    let suffix: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    let suffix = &suffix[..10];
    let keep = MAX_NAME_LEN - suffix.len() - 1;
    format!("{}_{}", &sanitized[..keep], suffix)
}

fn sanitize_name(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Build the bollard create spec for one desired container.
pub fn build_container(desired: &DesiredContainer, cfg: &BuilderConfig<'_>) -> BuiltContainer {
    let name = container_name(
        cfg.container_prefix,
        &desired.project,
        &desired.deployment_group,
        &desired.deployment_id,
        &desired.container,
        desired.replica,
        desired.generation,
    );

    // ── Labels: Traefik (routable only) ────────────────────────────────
    // Render the Traefik label set first so its recreate-signature hash can be
    // stamped as a bookkeeping label (`route-hash`). That hash lets the diff
    // detect create-time-only changes Docker can't apply to a running container
    // in place — routing transitions (a deployment becoming/ceasing to be
    // active) AND whether the app port is published to a loopback host port.
    let traefik_labels = render_traefik_labels_for(desired, cfg);
    let route_hash = labels::hash_recreate_signature(
        &traefik_labels,
        desired.port.is_some() && cfg.publish_app_ports,
    );

    // ── Labels: bookkeeping ─────────────────────────────────────────────
    let mut all_labels = BookkeepingLabels {
        label_namespace: cfg.label_namespace,
        controller_class: cfg.controller_class,
        project: &desired.project,
        deployment_group: &desired.deployment_group,
        deployment_id: &desired.deployment_id,
        deployment_uuid: &desired.deployment_uuid,
        container: &desired.container,
        environment: desired.environment.as_deref(),
        env_hash: &desired.env_hash,
        image: &desired.image,
        route_hash: &route_hash,
        generation: desired.generation,
        replica: desired.replica,
    }
    .render();
    all_labels.extend(traefik_labels);

    // ── Env ────────────────────────────────────────────────────────────
    let env: Vec<String> = desired
        .env
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();

    // ── Resources ──────────────────────────────────────────────────────
    // cpu millicores → nano_cpus (1 core = 1e9 nano_cpus = 1000 millicores).
    let nano_cpus = parse_cpu_millicores(&desired.cpu)
        .ok()
        .map(|millicores| (millicores as i64) * 1_000_000);
    let memory = parse_memory_bytes(&desired.memory)
        .ok()
        .map(|bytes| bytes as i64);

    // ── Health check ────────────────────────────────────────────────────
    // We deliberately inject *no* Docker HEALTHCHECK. The reconciler's HTTP
    // probe (over the Traefik network) is the single source of truth for
    // deployment health. A baked-in `wget`/`curl` check breaks on the many
    // images that ship neither (distroless / scratch / slim), surfacing a
    // misleading `unhealthy` status in `docker ps` with no bearing on routing.

    // ── Network attachment ─────────────────────────────────────────────
    // Attach a NETWORK ALIAS equal to the GROUP-scoped, deployment-id-FREE app
    // name (see [`group_app_name`]) so siblings keep resolving this container
    // across recreates AND across deployments of the same group: the live
    // container's `--name` carries the per-recreate `_g{n}` generation suffix and
    // the per-deployment id, but the injected `RISE_CONTAINER_HOST__<NAME>`
    // discovery env points at this stable name. Docker's embedded DNS resolves
    // the alias on the shared user-defined network and ROUND-ROBINS it across
    // whatever containers currently carry it, so the env hash never drifts per
    // generation/replica and old+new deployments of the group share one DNS name
    // (foundational for rolling overlap), mirroring the K8s group Service.
    let group_alias = group_app_name(
        cfg.container_prefix,
        &desired.project,
        &desired.deployment_group,
        &desired.container,
    );
    let mut endpoints = HashMap::new();
    endpoints.insert(
        cfg.traefik_network.to_string(),
        EndpointSettings {
            aliases: Some(vec![group_alias]),
            ..Default::default()
        },
    );

    // ── Local app→backend host aliases (extra_hosts) ───────────────────
    // LOCAL-DEV ONLY: map the configured alias host(s) (e.g. `rise.localhost`)
    // to the Rise backend's IP on the shared network so apps can reach the
    // public issuer/control-plane host to validate the `rise_jwt` cookie or do
    // OIDC discovery. Without this the public host resolves to the app
    // container's own loopback in local dev. Production leaves the alias list
    // empty (public DNS + Traefik handle the issuer host with correct TLS), so
    // `extra_hosts` stays `None` there.
    //
    // Staleness caveat: the backend IP is captured at container-create time. If
    // the backend container restarts and changes IP, existing app containers
    // keep the stale entry until recreated. Acceptable for local dev — we do
    // NOT track backend-IP drift.
    let extra_hosts = match cfg.app_backend_ip {
        Some(ip) if !cfg.app_backend_host_aliases.is_empty() => Some(
            cfg.app_backend_host_aliases
                .iter()
                .map(|alias| format!("{alias}:{ip}"))
                .collect::<Vec<String>>(),
        ),
        _ => None,
    };

    // ── DEV-ONLY: publish the app port to a random loopback host port ──
    // When `publish_app_ports` is on, publish every HTTP container's `{port}/tcp`
    // to a random `127.0.0.1` host port (empty `host_port` → Docker picks a free
    // one). This lets a host-run backend on Docker Desktop — where the container
    // bridge IP isn't routable from the host — health-probe the app directly on
    // loopback. NOTE: this is deliberately NOT gated on `desired.routable`: a
    // not-yet-active deployment is unroutable (no Traefik router) but must still
    // be probed to become Healthy, so it needs the published port too — gating on
    // routable would deadlock (port→health→active→routable→port). Worker
    // containers (no port) and the disabled case get neither an exposed port nor a
    // binding.
    let publish_port = desired.port.filter(|_| cfg.publish_app_ports);
    let (exposed_ports, port_bindings) = match publish_port {
        Some(port) => {
            let key = format!("{port}/tcp");
            let mut exposed: HashMap<String, HashMap<(), ()>> = HashMap::new();
            exposed.insert(key.clone(), HashMap::new());
            let mut bindings: bollard::models::PortMap = HashMap::new();
            bindings.insert(
                key,
                Some(vec![bollard::models::PortBinding {
                    host_ip: Some("127.0.0.1".to_string()),
                    // Empty → Docker assigns a random free host port.
                    host_port: Some(String::new()),
                }]),
            );
            (Some(exposed), Some(bindings))
        }
        None => (None, None),
    };

    let host_config = HostConfig {
        nano_cpus,
        memory,
        restart_policy: Some(RestartPolicy {
            name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
            maximum_retry_count: None,
        }),
        network_mode: Some(cfg.traefik_network.to_string()),
        extra_hosts,
        port_bindings,
        ..Default::default()
    };

    let config = Config {
        image: Some(desired.image.clone()),
        env: if env.is_empty() { None } else { Some(env) },
        labels: Some(all_labels),
        exposed_ports,
        host_config: Some(host_config),
        networking_config: Some(bollard::container::NetworkingConfig {
            endpoints_config: endpoints,
        }),
        ..Default::default()
    };

    BuiltContainer { name, config }
}

/// Render the full Traefik label map for a desired container.
///
/// Empty when the container is not routable (superseded / not-yet-active
/// deployment), has no port (worker), or has no host to route. A
/// not-yet-active deployment keeps its container running for health probing but
/// must not advertise a Traefik router, otherwise two routers would match the
/// same `Host(...)` rule and Traefik would split traffic. This mirrors the K8s
/// path, which builds the Ingress only from the active deployment per group.
fn render_traefik_labels_for(
    desired: &DesiredContainer,
    cfg: &BuilderConfig<'_>,
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
    // path, which errors on an unknown class — see
    // `ResourceBuilder::build_ingress_annotations`). The builder's signature
    // can't propagate an error (it feeds the create spec and a precomputed
    // route hash), so we treat an unknown class as the most restrictive
    // requirement (`Member`) rather than defaulting to public `None`. The app
    // is then routed only behind forwardAuth — never as an open public route.
    let requirement = match cfg.access_classes.get(&desired.access_class) {
        Some(req) => req.clone(),
        None => {
            tracing::error!(
                project = %desired.project,
                access_class = %desired.access_class,
                "Access class not configured — failing closed (treating as Member, \
                 routing only behind forwardAuth) to avoid a silent public route"
            );
            AccessRequirement::Member
        }
    };
    let forward_auth_address: Option<String> = match requirement {
        AccessRequirement::None => None,
        AccessRequirement::Authenticated | AccessRequirement::Member => {
            if cfg.auth_backend_url.is_empty() {
                // Invariant: startup (`init_docker_backend`) fails CLOSED when a
                // non-`None` access class is configured without an
                // `auth_backend_url` (see
                // `settings::docker_access_classes_missing_auth_backend_url`),
                // so this branch is unreachable in a running backend. We still
                // refuse to stamp a half-broken (auth-less) middleware here
                // rather than silently emit an open route.
                None
            } else {
                Some(format!(
                    "{}/api/v1/auth/ingress?project={}&signin_redirect=1",
                    cfg.auth_backend_url.trim_end_matches('/'),
                    urlencoding::encode(&desired.project)
                ))
            }
        }
    };

    // One router per (host-set × route). Single-container apps have a single
    // `/` route, so this yields exactly one router. Longest path-prefix first
    // matches the nginx semantics used by the K8s path.
    let mut routes = desired.routes.clone();
    routes.sort_by(|a, b| {
        let al = a.path_prefix.as_deref().unwrap_or("/").len();
        let bl = b.path_prefix.as_deref().unwrap_or("/").len();
        bl.cmp(&al)
    });
    for (idx, route) in routes.iter().enumerate() {
        if route.hosts.is_empty() {
            continue;
        }
        // GROUP-scoped router/service base name — deployment-id-FREE, so ALL
        // routers/services/middlewares for a (project, group, container) are
        // named IDENTICALLY across every deployment of the group. This lets an
        // old and a new deployment share one Traefik service (their replica
        // containers register as servers of the same load balancer), which sets
        // up health-driven rolling overlap. Mirrors the K8s group Service/Ingress
        // naming, which is likewise deployment-id-free.
        let base = labels::sanitize_router_name(&format!(
            "{}-{}-{}",
            desired.project, desired.deployment_group, desired.container
        ));
        // Distinct router per route so multiple path prefixes don't collide.
        let router_name = if routes.len() > 1 {
            format!("{base}-{idx}")
        } else {
            base
        };
        let traefik = labels::render_traefik_labels(&TraefikRoute {
            router_name: &router_name,
            hosts: &route.hosts,
            path_prefix: route.path_prefix.as_deref(),
            port,
            entrypoint: cfg.traefik_entrypoint,
            network: cfg.traefik_network,
            certresolver: cfg.traefik_certresolver,
            forward_auth: forward_auth_address.as_deref().map(|address| ForwardAuth {
                address,
                auth_response_headers: "X-Auth-Request-Email,X-Auth-Request-User",
            }),
        });
        out.extend(traefik);
    }
    out
}

/// Compute the `route-hash` recreate signature for a desired container without
/// building the full create spec. Used by the reconciler's diff so a routing
/// transition (a deployment becoming or ceasing to be active) OR a change in
/// whether the app port is published to a loopback host port forces a recreate.
/// Must stay consistent with the hash stamped by [`build_container`].
pub fn route_hash_for(desired: &DesiredContainer, cfg: &BuilderConfig<'_>) -> String {
    labels::hash_recreate_signature(
        &render_traefik_labels_for(desired, cfg),
        desired.port.is_some() && cfg.publish_app_ports,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::OnceLock;

    /// Shared empty access-class map (no forwardAuth) for tests that don't
    /// exercise authentication.
    fn empty_access_classes() -> &'static HashMap<String, AccessRequirement> {
        static MAP: OnceLock<HashMap<String, AccessRequirement>> = OnceLock::new();
        MAP.get_or_init(HashMap::new)
    }

    fn test_cfg() -> BuilderConfig<'static> {
        BuilderConfig {
            label_namespace: "rise.dev",
            controller_class: "default",
            container_prefix: "rise",
            traefik_network: "rise_default",
            traefik_entrypoint: "web",
            traefik_certresolver: None,
            auth_backend_url: "",
            access_classes: empty_access_classes(),
            app_backend_host_aliases: &[],
            app_backend_ip: None,
            publish_app_ports: false,
        }
    }

    fn single_container() -> DesiredContainer {
        DesiredContainer {
            project: "myapp".to_string(),
            access_class: "public".to_string(),
            deployment_group: "default".to_string(),
            deployment_id: "20260101-120000".to_string(),
            deployment_uuid: "11111111-1111-1111-1111-111111111111".to_string(),
            container: "app".to_string(),
            environment: Some("production".to_string()),
            image: "registry.example.test/rise/myapp:20260101-120000".to_string(),
            port: Some(8080),
            cpu: "500m".to_string(),
            memory: "256Mi".to_string(),
            env: vec![
                ("FOO".to_string(), "bar".to_string()),
                (
                    "RISE_APP_URL".to_string(),
                    "https://myapp.rise.dev".to_string(),
                ),
            ],
            env_hash: "abc123".to_string(),
            routes: vec![DesiredRoute {
                hosts: vec!["myapp.rise.dev".to_string()],
                path_prefix: None,
            }],
            routable: true,
            // `build_container` recomputes the route-hash from the rendered
            // Traefik labels, so the value stored here is irrelevant to these
            // builder tests. `route_hash_for` is exercised separately below.
            route_hash: String::new(),
            generation: 1,
            replica: 0,
            health_path: Some("/".to_string()),
        }
    }

    #[test]
    fn deterministic_name() {
        let n1 = container_name("rise", "myapp", "default", "20260101-120000", "app", 0, 1);
        let n2 = container_name("rise", "myapp", "default", "20260101-120000", "app", 0, 1);
        assert_eq!(n1, n2);
        assert_eq!(n1, "rise_myapp_default_20260101-120000_app_r0_g1");
    }

    #[test]
    fn container_name_includes_replica_index() {
        // Each replica gets a distinct `_r{n}` segment so its live `--name` is
        // unique, even though they share the identity tuple's other fields.
        let r0 = container_name("rise", "myapp", "default", "20260101-120000", "app", 0, 1);
        let r1 = container_name("rise", "myapp", "default", "20260101-120000", "app", 1, 1);
        assert_eq!(r0, "rise_myapp_default_20260101-120000_app_r0_g1");
        assert_eq!(r1, "rise_myapp_default_20260101-120000_app_r1_g1");
        assert_ne!(r0, r1);
    }

    #[test]
    fn long_name_is_hashed_but_stable() {
        let long_project = "a".repeat(100);
        let n1 = container_name(
            "rise",
            &long_project,
            "default",
            "20260101-120000",
            "app",
            0,
            1,
        );
        let n2 = container_name(
            "rise",
            &long_project,
            "default",
            "20260101-120000",
            "app",
            0,
            1,
        );
        assert_eq!(n1, n2);
        assert!(n1.len() <= MAX_NAME_LEN);
    }

    #[test]
    fn stable_identity_name_has_no_generation_suffix() {
        let n = stable_identity_name("rise", "myapp", "default", "20260101-120000", "app");
        assert_eq!(n, "rise_myapp_default_20260101-120000_app");
        assert!(
            !n.contains("_g"),
            "stable identity name must not carry a _g suffix"
        );
    }

    #[test]
    fn group_app_name_is_deployment_id_free() {
        // The group-scoped name is `{prefix}_{project}_{group}_{container}` with
        // NO deployment-id, replica, or generation segment — stable across
        // deployments. It is also the `RISE_CONTAINER_HOST__REDIS` discovery host.
        let n = group_app_name("rise", "myapp", "default", "redis");
        assert_eq!(n, "rise_myapp_default_redis");
        // Use the `worker` container to assert no `_r{n}` / `_g{n}` suffix without
        // tripping over the literal "_r" inside "redis".
        let w = group_app_name("rise", "myapp", "default", "worker");
        assert_eq!(w, "rise_myapp_default_worker");
        assert!(!w.contains("_r"), "must not carry a _r replica suffix");
        assert!(!w.contains("_g"), "must not carry a _g generation suffix");
        // Independent of deployment id by construction (no id param).
        assert_eq!(
            group_app_name("rise", "myapp", "default", "redis"),
            "rise_myapp_default_redis"
        );
    }

    #[test]
    fn network_alias_is_group_scoped_without_deployment_id_or_generation() {
        // The Docker network alias must be the GROUP-scoped, deployment-id-FREE
        // app name so siblings resolve this container across recreates AND across
        // deployments of the group, and the injected discovery env doesn't drift.
        let mut desired = single_container();
        desired.generation = 7;
        let built = build_container(&desired, &test_cfg());
        // The container NAME carries the deployment id + replica + generation...
        assert_eq!(built.name, "rise_myapp_default_20260101-120000_app_r0_g7");
        let aliases = built
            .config
            .networking_config
            .as_ref()
            .unwrap()
            .endpoints_config
            .get("rise_default")
            .unwrap()
            .aliases
            .as_ref()
            .expect("network endpoint must carry an alias");
        // ...but the alias is the group-scoped `{prefix}_{project}_{group}_{container}`.
        assert_eq!(aliases, &vec!["rise_myapp_default_app".to_string()]);
        assert!(
            !aliases[0].contains("20260101-120000"),
            "network alias must not carry the deployment id"
        );
        assert!(
            !aliases[0].contains("_g"),
            "network alias must not carry a _g generation suffix"
        );
        assert!(
            !aliases[0].contains("_r"),
            "network alias must not carry a _r replica suffix"
        );
    }

    #[test]
    fn network_alias_stable_across_different_deployment_ids() {
        // Two DIFFERENT deployment_ids of the same (project, group, container)
        // must attach the IDENTICAL group-scoped network alias, so Docker DNS can
        // round-robin across both deployments' containers during a rolling overlap.
        let cfg = test_cfg();
        let mut d1 = single_container();
        d1.deployment_id = "20260101-120000".to_string();
        let mut d2 = single_container();
        d2.deployment_id = "20260202-235959".to_string();

        let alias_of = |d: &DesiredContainer| -> Vec<String> {
            build_container(d, &cfg)
                .config
                .networking_config
                .unwrap()
                .endpoints_config
                .get("rise_default")
                .unwrap()
                .aliases
                .clone()
                .unwrap()
        };
        assert_eq!(
            alias_of(&d1),
            alias_of(&d2),
            "different deployments of a group must share one network alias"
        );
        assert_eq!(alias_of(&d1), vec!["rise_myapp_default_app".to_string()]);
    }

    #[test]
    fn replicas_share_one_replica_free_network_alias() {
        // Two replicas of the same spec must attach the IDENTICAL, replica-free
        // network alias so Docker's embedded DNS round-robins the alias across
        // both running replicas (and the sibling-discovery host points at it).
        let cfg = test_cfg();
        let mut r0 = single_container();
        r0.replica = 0;
        let mut r1 = single_container();
        r1.replica = 1;

        let alias_of = |d: &DesiredContainer| -> Vec<String> {
            build_container(d, &cfg)
                .config
                .networking_config
                .unwrap()
                .endpoints_config
                .get("rise_default")
                .unwrap()
                .aliases
                .clone()
                .unwrap()
        };
        let a0 = alias_of(&r0);
        let a1 = alias_of(&r1);
        assert_eq!(a0, a1, "replicas must share one network alias");
        assert_eq!(a0, vec!["rise_myapp_default_app".to_string()]);
        assert!(
            !a0[0].contains("_r"),
            "shared alias must not carry a _r replica suffix"
        );
    }

    #[test]
    fn replicas_render_identical_traefik_labels_one_service() {
        // All Traefik labels (router rule/entrypoint, service loadbalancer port,
        // service/router NAMES) must be identical across replicas so Traefik's
        // Docker provider registers each replica container as a SERVER of the ONE
        // service → round-robin load balancing. Only the `replica` field (→ the
        // `--name` and the `replica` bookkeeping label) differs.
        let cfg = test_cfg();
        let mut r0 = single_container();
        r0.replica = 0;
        let mut r1 = single_container();
        r1.replica = 1;

        let traefik_of = |d: &DesiredContainer| -> std::collections::BTreeMap<String, String> {
            build_container(d, &cfg)
                .config
                .labels
                .unwrap()
                .into_iter()
                .filter(|(k, _)| k.starts_with("traefik."))
                .collect()
        };
        let t0 = traefik_of(&r0);
        let t1 = traefik_of(&r1);
        assert_eq!(
            t0, t1,
            "replicas must render identical Traefik labels (shared router+service)"
        );
        // Spot-check the shared service/router name is group-scoped (replica- and
        // deployment-id-free).
        assert!(t0.contains_key("traefik.http.services.myapp-default-app.loadbalancer.server.port"));
        assert!(t0.contains_key("traefik.http.routers.myapp-default-app.rule"));
        assert!(
            !t0.keys().any(|k| k.contains("-r0") || k.contains("-r1")),
            "Traefik router/service names must not include a replica index"
        );
        assert!(
            !t0.keys().any(|k| k.contains("20260101-120000")),
            "Traefik router/service names must not include the deployment id"
        );
    }

    #[test]
    fn traefik_names_stable_across_different_deployment_ids() {
        // Two DIFFERENT deployment_ids of the same (project, group, container)
        // must render the SAME Traefik service/router names, so an old and a new
        // deployment register as servers of ONE Traefik service (rolling overlap).
        let cfg = test_cfg();
        let mut d1 = single_container();
        d1.deployment_id = "20260101-120000".to_string();
        let mut d2 = single_container();
        d2.deployment_id = "20260202-235959".to_string();

        let traefik_of = |d: &DesiredContainer| -> std::collections::BTreeMap<String, String> {
            build_container(d, &cfg)
                .config
                .labels
                .unwrap()
                .into_iter()
                .filter(|(k, _)| k.starts_with("traefik."))
                .collect()
        };
        assert_eq!(
            traefik_of(&d1),
            traefik_of(&d2),
            "different deployments of a group must share one Traefik router+service"
        );
        let t = traefik_of(&d1);
        assert!(t.contains_key("traefik.http.routers.myapp-default-app.rule"));
        assert!(t.contains_key("traefik.http.services.myapp-default-app.loadbalancer.server.port"));
    }

    #[test]
    fn single_container_maps_labels_env_resources() {
        let desired = single_container();
        let built = build_container(&desired, &test_cfg());

        assert_eq!(built.name, "rise_myapp_default_20260101-120000_app_r0_g1");

        let labels = built.config.labels.as_ref().unwrap();
        assert_eq!(
            labels.get("rise.dev/managed-by").map(String::as_str),
            Some("rise")
        );
        assert_eq!(
            labels.get("rise.dev/project").map(String::as_str),
            Some("myapp")
        );
        assert_eq!(
            labels.get("rise.dev/env-hash").map(String::as_str),
            Some("abc123")
        );
        assert_eq!(
            labels.get("rise.dev/image").map(String::as_str),
            Some("registry.example.test/rise/myapp:20260101-120000")
        );
        // Traefik router present for the single `/` route.
        assert_eq!(
            labels.get("traefik.enable").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            labels
                .get("traefik.http.routers.myapp-default-app.rule")
                .map(String::as_str),
            Some("Host(`myapp.rise.dev`)")
        );
        assert_eq!(
            labels
                .get("traefik.http.services.myapp-default-app.loadbalancer.server.port")
                .map(String::as_str),
            Some("8080")
        );

        // Env merged into KEY=VALUE list.
        let env = built.config.env.as_ref().unwrap();
        assert!(env.contains(&"FOO=bar".to_string()));
        assert!(env.contains(&"RISE_APP_URL=https://myapp.rise.dev".to_string()));

        // Resources: 500m → 500 * 1e6 nano_cpus; 256Mi → bytes.
        let hc = built.config.host_config.as_ref().unwrap();
        assert_eq!(hc.nano_cpus, Some(500_000_000));
        assert_eq!(hc.memory, Some(256 * 1024 * 1024));
        assert_eq!(hc.network_mode.as_deref(), Some("rise_default"));
        assert_eq!(
            hc.restart_policy.as_ref().and_then(|r| r.name),
            Some(RestartPolicyNameEnum::UNLESS_STOPPED)
        );
    }

    #[test]
    fn extra_hosts_injected_when_aliases_and_backend_ip_present() {
        // LOCAL-DEV: aliases configured + a resolved backend IP → the container's
        // HostConfig.extra_hosts maps each alias to the backend IP.
        let aliases = ["rise.localhost".to_string()];
        let cfg = BuilderConfig {
            app_backend_host_aliases: &aliases,
            app_backend_ip: Some("172.20.0.5"),
            ..test_cfg()
        };
        let built = build_container(&single_container(), &cfg);
        let hc = built.config.host_config.as_ref().unwrap();
        let extra = hc.extra_hosts.as_ref().expect("extra_hosts must be set");
        assert!(
            extra.contains(&"rise.localhost:172.20.0.5".to_string()),
            "extra_hosts {extra:?} must contain rise.localhost:<ip>"
        );
    }

    #[test]
    fn extra_hosts_supports_multiple_aliases() {
        let aliases = ["rise.localhost".to_string(), "rise.local".to_string()];
        let cfg = BuilderConfig {
            app_backend_host_aliases: &aliases,
            app_backend_ip: Some("10.0.0.9"),
            ..test_cfg()
        };
        let built = build_container(&single_container(), &cfg);
        let extra = built
            .config
            .host_config
            .as_ref()
            .unwrap()
            .extra_hosts
            .as_ref()
            .unwrap();
        assert!(extra.contains(&"rise.localhost:10.0.0.9".to_string()));
        assert!(extra.contains(&"rise.local:10.0.0.9".to_string()));
    }

    #[test]
    fn extra_hosts_none_when_aliases_empty() {
        // PROD: no aliases → extra_hosts stays None even if an IP were present.
        let built = build_container(&single_container(), &test_cfg());
        assert!(built
            .config
            .host_config
            .as_ref()
            .unwrap()
            .extra_hosts
            .is_none());

        // An IP without aliases also yields None (the alias list gates it).
        let cfg = BuilderConfig {
            app_backend_ip: Some("172.20.0.5"),
            ..test_cfg()
        };
        let built = build_container(&single_container(), &cfg);
        assert!(built
            .config
            .host_config
            .as_ref()
            .unwrap()
            .extra_hosts
            .is_none());
    }

    #[test]
    fn extra_hosts_none_when_backend_ip_unresolved() {
        // Aliases configured but the backend IP couldn't be resolved → skip
        // injection rather than emit a broken `alias:` entry.
        let aliases = ["rise.localhost".to_string()];
        let cfg = BuilderConfig {
            app_backend_host_aliases: &aliases,
            app_backend_ip: None,
            ..test_cfg()
        };
        let built = build_container(&single_container(), &cfg);
        assert!(built
            .config
            .host_config
            .as_ref()
            .unwrap()
            .extra_hosts
            .is_none());
    }

    #[test]
    fn publish_app_ports_binds_routable_port_to_loopback() {
        // DEV-ONLY: with publish_app_ports on, a routable container gets an
        // exposed port + a 127.0.0.1 binding with an empty (random) host port.
        let cfg = BuilderConfig {
            publish_app_ports: true,
            ..test_cfg()
        };
        let built = build_container(&single_container(), &cfg);

        let exposed = built
            .config
            .exposed_ports
            .as_ref()
            .expect("exposed_ports must be set when publishing");
        assert!(exposed.contains_key("8080/tcp"));

        let bindings = built
            .config
            .host_config
            .as_ref()
            .unwrap()
            .port_bindings
            .as_ref()
            .expect("port_bindings must be set when publishing");
        let binding = bindings
            .get("8080/tcp")
            .and_then(|b| b.as_ref())
            .and_then(|v| v.first())
            .expect("a binding for 8080/tcp");
        assert_eq!(binding.host_ip.as_deref(), Some("127.0.0.1"));
        // Empty host_port → Docker assigns a random free port.
        assert_eq!(binding.host_port.as_deref(), Some(""));
    }

    #[test]
    fn publish_app_ports_off_adds_no_bindings() {
        // Default (production): no exposed ports, no port bindings.
        let built = build_container(&single_container(), &test_cfg());
        assert!(built.config.exposed_ports.is_none());
        assert!(built
            .config
            .host_config
            .as_ref()
            .unwrap()
            .port_bindings
            .is_none());
    }

    #[test]
    fn publish_app_ports_skips_worker_without_port() {
        // A worker (no port) never gets a binding, even with publishing on.
        let cfg = BuilderConfig {
            publish_app_ports: true,
            ..test_cfg()
        };
        let mut desired = single_container();
        desired.container = "worker".to_string();
        desired.port = None;
        desired.routes = vec![];
        let built = build_container(&desired, &cfg);
        assert!(built.config.exposed_ports.is_none());
        assert!(built
            .config
            .host_config
            .as_ref()
            .unwrap()
            .port_bindings
            .is_none());
    }

    #[test]
    fn publish_app_ports_publishes_non_routable_container_for_probing() {
        // A not-yet-active (non-routable) container STILL gets a published port:
        // it has no Traefik router yet, but the reconciler must health-probe it to
        // promote it to Healthy/active. Gating publish on `routable` would deadlock
        // (port→health→active→routable→port). Worker containers (no port) are still
        // skipped — see publish_app_ports_skips_worker_without_port.
        let cfg = BuilderConfig {
            publish_app_ports: true,
            ..test_cfg()
        };
        let mut desired = single_container();
        desired.routable = false;
        let built = build_container(&desired, &cfg);
        assert!(built.config.exposed_ports.is_some());
        assert!(built
            .config
            .host_config
            .as_ref()
            .unwrap()
            .port_bindings
            .is_some());
    }

    #[test]
    fn worker_container_has_no_traefik_labels() {
        let mut desired = single_container();
        desired.container = "worker".to_string();
        desired.port = None;
        desired.routes = vec![];
        let built = build_container(&desired, &test_cfg());
        let labels = built.config.labels.as_ref().unwrap();
        assert!(!labels.contains_key("traefik.enable"));
        // Bookkeeping labels still present.
        assert_eq!(
            labels.get("rise.dev/container").map(String::as_str),
            Some("worker")
        );
    }

    #[test]
    fn non_routable_container_drops_traefik_labels() {
        // A superseded (Terminating) deployment keeps its container running but
        // must not advertise a Traefik router, so only the active deployment is
        // routable for the shared Host during the supersession window.
        let mut desired = single_container();
        desired.routable = false;
        let built = build_container(&desired, &test_cfg());
        let labels = built.config.labels.as_ref().unwrap();
        assert!(!labels.contains_key("traefik.enable"));
        assert!(!labels
            .keys()
            .any(|k| k.starts_with("traefik.http.routers.")));
        // Bookkeeping labels still present so GC/diff can find the container.
        assert_eq!(
            labels.get("rise.dev/container").map(String::as_str),
            Some("app")
        );
        // The route-hash reflects the empty (non-routable) label set, so the
        // diff sees a routing transition the moment this deployment becomes
        // active. It must differ from a routable container's route-hash.
        let non_routable_hash = labels.get("rise.dev/route-hash").cloned().unwrap();
        let routable_hash = build_container(&single_container(), &test_cfg())
            .config
            .labels
            .unwrap()
            .get("rise.dev/route-hash")
            .cloned()
            .unwrap();
        assert_ne!(non_routable_hash, routable_hash);
    }

    #[test]
    fn routable_container_stamps_nonempty_route_hash_matching_route_hash_for() {
        // A routable container carries a non-empty `route-hash`, and the diff's
        // `route_hash_for` helper produces exactly the same value the builder
        // stamps — so the diff comparison is exact (no spurious recreates).
        let desired = single_container();
        let cfg = test_cfg();
        let built = build_container(&desired, &cfg);
        let stamped = built
            .config
            .labels
            .as_ref()
            .unwrap()
            .get("rise.dev/route-hash")
            .cloned()
            .unwrap();
        assert!(!stamped.is_empty());
        assert_eq!(stamped, route_hash_for(&desired, &cfg));
    }

    #[test]
    fn route_hash_for_changes_with_routability() {
        // Flipping `routable` (active → inactive) changes the route-hash, which
        // is what drives the diff to recreate the container at cutover.
        let cfg = test_cfg();
        let mut desired = single_container();
        let active = route_hash_for(&desired, &cfg);
        desired.routable = false;
        let inactive = route_hash_for(&desired, &cfg);
        assert_ne!(active, inactive);
    }

    #[test]
    fn route_hash_for_changes_with_publish_app_ports() {
        // Toggling publish_app_ports for a port-bearing container changes the
        // recreate-signature, so a container created before the flag was enabled
        // (no published port) is recreated to gain the loopback binding — no
        // manual redeploy needed.
        let desired = single_container();
        assert!(desired.port.is_some(), "fixture must have a port");
        let mut cfg = test_cfg();
        cfg.publish_app_ports = false;
        let without = route_hash_for(&desired, &cfg);
        cfg.publish_app_ports = true;
        let with = route_hash_for(&desired, &cfg);
        assert_ne!(without, with);
    }

    #[test]
    fn route_hash_for_ignores_publish_for_worker() {
        // A worker (no port) is never published, so the publish flag must not
        // change its recreate-signature (no spurious churn of port-less
        // containers when the flag is toggled).
        let mut desired = single_container();
        desired.port = None;
        desired.routes = vec![];
        let mut cfg = test_cfg();
        cfg.publish_app_ports = false;
        let off = route_hash_for(&desired, &cfg);
        cfg.publish_app_ports = true;
        let on = route_hash_for(&desired, &cfg);
        assert_eq!(off, on);
    }

    #[test]
    fn multi_container_routes_get_distinct_routers_longest_prefix_first() {
        let mut desired = single_container();
        desired.container = "api".to_string();
        desired.routes = vec![
            DesiredRoute {
                hosts: vec!["myapp.rise.dev".to_string()],
                path_prefix: Some("/".to_string()),
            },
            DesiredRoute {
                hosts: vec!["myapp.rise.dev".to_string()],
                path_prefix: Some("/api/v1".to_string()),
            },
        ];
        let built = build_container(&desired, &test_cfg());
        let labels = built.config.labels.as_ref().unwrap();
        // Two routers, index suffixed. Longest prefix (/api/v1) sorts first → -0.
        assert_eq!(
            labels
                .get("traefik.http.routers.myapp-default-api-0.rule")
                .map(String::as_str),
            Some("Host(`myapp.rise.dev`) && PathPrefix(`/api/v1`)")
        );
        assert_eq!(
            labels
                .get("traefik.http.routers.myapp-default-api-1.rule")
                .map(String::as_str),
            Some("Host(`myapp.rise.dev`)")
        );
    }

    #[test]
    fn certresolver_adds_tls_labels() {
        let cfg = BuilderConfig {
            traefik_certresolver: Some("le"),
            ..test_cfg()
        };
        let desired = single_container();
        let built = build_container(&desired, &cfg);
        let labels = built.config.labels.as_ref().unwrap();
        assert_eq!(
            labels
                .get("traefik.http.routers.myapp-default-app.tls")
                .map(String::as_str),
            Some("true")
        );
        assert_eq!(
            labels
                .get("traefik.http.routers.myapp-default-app.tls.certresolver")
                .map(String::as_str),
            Some("le")
        );
    }

    /// An access-class map where `public` is `None` and `private` is `Member`.
    fn public_private_map() -> HashMap<String, AccessRequirement> {
        let mut map = HashMap::new();
        map.insert("public".to_string(), AccessRequirement::None);
        map.insert("private".to_string(), AccessRequirement::Member);
        map
    }

    #[test]
    fn private_access_class_stamps_forward_auth_labels() {
        let map = public_private_map();
        let cfg = BuilderConfig {
            auth_backend_url: "http://rise:3000",
            access_classes: &map,
            ..test_cfg()
        };
        let mut desired = single_container();
        desired.access_class = "private".to_string();
        let built = build_container(&desired, &cfg);
        let labels = built.config.labels.as_ref().unwrap();
        let r = "myapp-default-app";
        assert_eq!(
            labels
                .get(&format!(
                    "traefik.http.middlewares.{r}-auth.forwardauth.address"
                ))
                .map(String::as_str),
            Some("http://rise:3000/api/v1/auth/ingress?project=myapp&signin_redirect=1")
        );
        assert_eq!(
            labels
                .get(&format!(
                    "traefik.http.middlewares.{r}-auth.forwardauth.authResponseHeaders"
                ))
                .map(String::as_str),
            Some("X-Auth-Request-Email,X-Auth-Request-User")
        );
        assert_eq!(
            labels
                .get(&format!("traefik.http.routers.{r}.middlewares"))
                .map(String::as_str),
            Some(format!("{r}-auth@docker").as_str())
        );
    }

    #[test]
    fn public_access_class_stamps_no_forward_auth() {
        let map = public_private_map();
        let cfg = BuilderConfig {
            auth_backend_url: "http://rise:3000",
            access_classes: &map,
            ..test_cfg()
        };
        let mut desired = single_container();
        desired.access_class = "public".to_string();
        let built = build_container(&desired, &cfg);
        let labels = built.config.labels.as_ref().unwrap();
        assert!(!labels
            .keys()
            .any(|k| k.contains("forwardauth") || k.ends_with(".middlewares")));
    }

    #[test]
    fn private_access_class_without_backend_url_stamps_no_forward_auth() {
        // Member requirement but no auth_backend_url → forwardAuth disabled.
        // In a running backend this state is unreachable because startup fails
        // closed (see settings::docker_access_classes_missing_auth_backend_url);
        // the builder still refuses to stamp an auth-less middleware here.
        let mut map = HashMap::new();
        map.insert("private".to_string(), AccessRequirement::Member);
        let cfg = BuilderConfig {
            auth_backend_url: "",
            access_classes: &map,
            ..test_cfg()
        };
        let mut desired = single_container();
        desired.access_class = "private".to_string();
        let built = build_container(&desired, &cfg);
        let labels = built.config.labels.as_ref().unwrap();
        assert!(!labels.keys().any(|k| k.contains("forwardauth")));
    }

    #[test]
    fn unknown_access_class_fails_closed_not_public() {
        // A project whose access_class is absent from the map must NOT be served
        // as an open public route. We fail closed: the route is stamped behind
        // forwardAuth (most-restrictive) rather than defaulting to None/public.
        let map = public_private_map(); // does not contain "ghost"
        let cfg = BuilderConfig {
            auth_backend_url: "http://rise:3000",
            access_classes: &map,
            ..test_cfg()
        };
        let mut desired = single_container();
        desired.access_class = "ghost".to_string();
        let built = build_container(&desired, &cfg);
        let labels = built.config.labels.as_ref().unwrap();
        let r = "myapp-default-app";
        // forwardAuth middleware IS stamped (route is protected, not open).
        assert_eq!(
            labels
                .get(&format!(
                    "traefik.http.middlewares.{r}-auth.forwardauth.address"
                ))
                .map(String::as_str),
            Some("http://rise:3000/api/v1/auth/ingress?project=myapp&signin_redirect=1")
        );
        assert_eq!(
            labels
                .get(&format!("traefik.http.routers.{r}.middlewares"))
                .map(String::as_str),
            Some(format!("{r}-auth@docker").as_str())
        );
    }

    #[test]
    fn forward_auth_changes_route_hash() {
        let map = public_private_map();
        let cfg = BuilderConfig {
            auth_backend_url: "http://rise:3000",
            access_classes: &map,
            ..test_cfg()
        };
        let mut public_c = single_container();
        public_c.access_class = "public".to_string();
        let mut private_c = single_container();
        private_c.access_class = "private".to_string();
        assert_ne!(
            route_hash_for(&public_c, &cfg),
            route_hash_for(&private_c, &cfg),
            "access-class change must change the route hash to trigger recreate"
        );
    }
}
