//! Pure `Deployment` → bollard create-spec mapping.
//!
//! Everything here is deterministic and daemon-free so it can be unit-tested:
//! the reconciler resolves the dynamic inputs (env vars, image, URLs) and calls
//! [`build_container`] to produce the [`bollard::container::Config`] plus the
//! container name and labels.

use std::collections::HashMap;

use bollard::container::Config;
use bollard::secret::{EndpointSettings, HostConfig, RestartPolicy, RestartPolicyNameEnum};

use super::labels::{self, BookkeepingLabels};
use rise_backend_core::quantity::{
    parse_cpu_millicores, parse_cpu_request_limit, parse_memory_bytes, parse_memory_request_limit,
};
use rise_backend_core::traefik_render::TraefikRenderConfig;
use rise_backend_core::AccessRequirement;

// The desired-state shape, the name derivations and the Traefik label renderer
// are backend-agnostic and now live in `rise-backend-core`, shared with the ECS
// backend. Re-exported here so every existing `container_builder::…` path — in
// this crate and in `rise-deploy` — resolves unchanged.
pub use rise_backend_core::desired::{DesiredContainer, DesiredRoute};
pub use rise_backend_core::naming::{
    container_name, group_app_name, group_service_base, group_service_name, stable_identity_name,
};
pub use rise_backend_core::naming::{group_service_names, MAX_NAME_LEN, MAX_SERVICE_BASE_LEN};
pub use rise_backend_core::traefik_render::{
    render_traefik_labels_for, routes_withheld, TraefikRenderConfig as _TraefikRenderConfig,
};

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

impl<'a> BuilderConfig<'a> {
    /// Project onto the backend-agnostic renderer config. The Docker provider
    /// needs `traefik.docker.network`, so the network is always `Some` here.
    pub fn render_config(&self) -> TraefikRenderConfig<'_> {
        TraefikRenderConfig {
            label_namespace: self.label_namespace,
            controller_class: self.controller_class,
            traefik_entrypoint: self.traefik_entrypoint,
            traefik_certresolver: self.traefik_certresolver,
            network: Some(self.traefik_network),
            auth_backend_url: self.auth_backend_url,
            access_classes: self.access_classes,
        }
    }
}

/// Compute the `route-hash` recreate signature for a desired container without
/// building the full create spec, folding in this backend's `publish_app_ports`
/// binding (a create-time-only property Docker can't mutate in place).
pub fn route_hash_for(desired: &DesiredContainer, cfg: &BuilderConfig<'_>) -> String {
    rise_backend_core::traefik_render::route_hash_for(
        desired,
        &cfg.render_config(),
        desired.port.is_some() && cfg.publish_app_ports,
    )
}

/// Result of building one container: the deterministic name and the bollard
/// create config (which already carries the network attachment).
pub struct BuiltContainer {
    pub name: String,
    pub config: Config<String>,
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
    let traefik_labels = render_traefik_labels_for(desired, &cfg.render_config());
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
    // A cpu/memory value is either fixed (`"500m"`) or a documented
    // `request-limit` range (`"500m-1"`). Docker has a single hard cap per
    // resource (`nano_cpus`/`memory`), so we apply the LIMIT half — mirroring the
    // K8s builder, which puts the limit half into the pod's `limits` (the hard
    // cap) while the request half feeds `requests` (scheduling only). Using the
    // single-value parsers here would reject any range and silently drop the cap,
    // leaving the container UNBOUNDED while K8s caps it — a strict-parity break.
    // On a genuine parse failure we `warn!` (with the offending value) instead of
    // silently dropping the limit.
    let nano_cpus = match parse_cpu_request_limit(&desired.cpu) {
        // cpu millicores → nano_cpus (1 core = 1e9 nano_cpus = 1000 millicores).
        Ok((_req, lim)) => parse_cpu_millicores(&lim)
            .ok()
            .map(|millicores| (millicores as i64) * 1_000_000),
        Err(e) => {
            tracing::warn!(
                cpu = %desired.cpu,
                "Failed to parse CPU quantity; leaving nano_cpus unset (no hard cap): {e:?}"
            );
            None
        }
    };
    let memory = match parse_memory_request_limit(&desired.memory) {
        Ok((_req, lim)) => parse_memory_bytes(&lim).ok().map(|bytes| bytes as i64),
        Err(e) => {
            tracing::warn!(
                memory = %desired.memory,
                "Failed to parse memory quantity; leaving memory unset (no hard cap): {e:?}"
            );
            None
        }
    };

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
    // non-routable container (router withheld by an auth misconfiguration) still
    // runs and must still be probed so the reconciler can report it
    // Healthy/Unhealthy, so it needs the published port too. Worker containers (no
    // port) and the disabled case get neither an exposed port nor a binding.
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

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::OnceLock;

    /// Shared default access-class map for tests that don't exercise
    /// authentication: the fixture's `public` class maps to `AccessRequirement::None`
    /// so the common-case container is a properly-CONFIGURED public app (routed,
    /// no forwardAuth). An empty map would instead make `public` an UNKNOWN class
    /// that correctly fails closed — and, with the empty `auth_backend_url` here,
    /// withholds the router entirely — which is not what these baseline tests mean
    /// to exercise.
    fn default_access_classes() -> &'static HashMap<String, AccessRequirement> {
        static MAP: OnceLock<HashMap<String, AccessRequirement>> = OnceLock::new();
        MAP.get_or_init(|| {
            let mut m = HashMap::new();
            m.insert("public".to_string(), AccessRequirement::None);
            m
        })
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
            access_classes: default_access_classes(),
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
                access: None,
            }],
            routable: true,
            // `build_container` recomputes the route-hash from the rendered
            // Traefik labels, so the value stored here is irrelevant to these
            // builder tests. `route_hash_for` is exercised separately below.
            route_hash: String::new(),
            generation: 1,
            replica: 0,
            health_path: Some("/".to_string()),
            health_check_interval_secs: None,
            health_check_timeout_secs: None,
        }
    }

    #[test]
    fn group_service_base_is_injective_across_separator_and_case_collisions() {
        // `sanitize_router_name` is lossy: `a.b` and `a-b` both sanitize to
        // `a-b`, and `Foo`/`foo` both lowercase to `foo`. The injective hash
        // suffix (over the STRUCTURED tuple) must keep the two distinct so two
        // projects are never pooled behind one Traefik load balancer.
        assert_ne!(
            group_service_base("a.b", "default", "app"),
            group_service_base("a-b", "default", "app"),
            "tuples differing only in separators must not collide"
        );
        assert_ne!(
            group_service_base("Foo", "default", "app"),
            group_service_base("foo", "default", "app"),
            "tuples differing only in case must not collide"
        );
        // The collision also can't be shifted across field boundaries: the NUL
        // separator means `("ab", "c", …)` and `("a", "bc", …)` are distinct.
        assert_ne!(
            group_service_base("ab", "c", "app"),
            group_service_base("a", "bc", "app"),
            "field boundaries must be hash-significant (NUL separator)"
        );
    }

    #[test]
    fn group_service_base_is_stable_across_calls() {
        // The same tuple must always yield the same name so the labels and the
        // reconciler's serverStatus lookup agree across ticks/processes.
        assert_eq!(
            group_service_base("myapp", "default", "app"),
            group_service_base("myapp", "default", "app"),
        );
    }

    #[test]
    fn group_service_base_normal_tuple_is_readable_and_valid() {
        // A normal tuple stays human-readable (the sanitized base is preserved)
        // and the whole name is a valid Traefik router/service name
        // (`[a-z0-9-]`, no leading/trailing `-`, comfortably under the limit).
        let base = group_service_base("myapp", "default", "app");
        assert!(
            base.starts_with("myapp-default-app-"),
            "readable base preserved: {base}"
        );
        assert!(
            base.len() <= MAX_SERVICE_BASE_LEN + 1 + 16,
            "name stays within limits: {base} ({} chars)",
            base.len()
        );
        assert!(
            base.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "only [a-z0-9-] allowed: {base}"
        );
        assert!(
            !base.starts_with('-') && !base.ends_with('-'),
            "no leading/trailing dash: {base}"
        );
        // A pathologically long project still produces a valid, capped name with
        // the injective suffix intact.
        let long = group_service_base(&"p".repeat(200), "default", "app");
        assert!(
            long.len() <= MAX_SERVICE_BASE_LEN + 1 + 16,
            "long tuple is length-capped: {long} ({} chars)",
            long.len()
        );
        assert!(!long.ends_with('-') && !long.contains("--"));
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
        // deployment-id-free). The base carries the injective `-{hex16}` suffix.
        let base = group_service_base("myapp", "default", "app");
        assert!(t0.contains_key(&format!(
            "traefik.http.services.{base}.loadbalancer.server.port"
        )));
        assert!(t0.contains_key(&format!("traefik.http.routers.{base}.rule")));
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
        let base = group_service_base("myapp", "default", "app");
        assert!(t.contains_key(&format!("traefik.http.routers.{base}.rule")));
        assert!(t.contains_key(&format!(
            "traefik.http.services.{base}.loadbalancer.server.port"
        )));
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
        let base = group_service_base("myapp", "default", "app");
        assert_eq!(
            labels
                .get(&format!("traefik.http.routers.{base}.rule"))
                .map(String::as_str),
            Some("Host(`myapp.rise.dev`)")
        );
        assert_eq!(
            labels
                .get(&format!(
                    "traefik.http.services.{base}.loadbalancer.server.port"
                ))
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
    fn resource_range_applies_the_limit_half_as_hard_cap() {
        // A `request-limit` range must set the Docker hard cap (nano_cpus/memory)
        // from the LIMIT half — mirroring the K8s builder, which feeds the limit
        // half into the pod's `limits`. The single-value parsers reject a range,
        // so without this the container would run UNBOUNDED (a parity break).
        let mut desired = single_container();
        desired.cpu = "500m-1".to_string();
        desired.memory = "256Mi-1Gi".to_string();
        let built = build_container(&desired, &test_cfg());
        let hc = built.config.host_config.as_ref().unwrap();
        // Limit half: cpu `1` core → 1000 millicores → 1e9 nano_cpus.
        assert_eq!(hc.nano_cpus, Some(1_000_000_000));
        // Limit half: memory `1Gi` → bytes.
        assert_eq!(hc.memory, Some(1024 * 1024 * 1024));
    }

    #[test]
    fn resource_bare_value_still_applies_as_cap() {
        // A bare (fixed) value still parses and caps both resources (request ==
        // limit), so the common single-value case is unaffected by range support.
        let mut desired = single_container();
        desired.cpu = "250m".to_string();
        desired.memory = "128Mi".to_string();
        let built = build_container(&desired, &test_cfg());
        let hc = built.config.host_config.as_ref().unwrap();
        assert_eq!(hc.nano_cpus, Some(250_000_000));
        assert_eq!(hc.memory, Some(128 * 1024 * 1024));
    }

    #[test]
    fn resource_unparseable_value_leaves_cap_unset() {
        // A genuinely unparseable value leaves the cap unset (logged via warn!)
        // rather than panicking or applying a bogus cap.
        let mut desired = single_container();
        desired.cpu = "not-a-cpu".to_string();
        desired.memory = "garbage".to_string();
        let built = build_container(&desired, &test_cfg());
        let hc = built.config.host_config.as_ref().unwrap();
        assert_eq!(hc.nano_cpus, None);
        assert_eq!(hc.memory, None);
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
        // A non-routable container (router withheld by an auth misconfiguration)
        // STILL gets a published port: it has no Traefik router, but the
        // reconciler must health-probe it to report it Healthy/Unhealthy. Worker
        // containers (no port) are still skipped — see
        // publish_app_ports_skips_worker_without_port.
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
        // A non-routable container (router withheld — auth required but no
        // `auth_backend_url` to wire forwardAuth) keeps its container running for
        // health probing but advertises no Traefik router, so a misconfigured
        // deployment never serves an unauthenticated route.
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
        // Flipping `routable` (e.g. a router withheld → wired) changes the
        // route-hash, which is what drives the diff to recreate the container.
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
                access: None,
            },
            DesiredRoute {
                hosts: vec!["myapp.rise.dev".to_string()],
                path_prefix: Some("/api/v1".to_string()),
                access: None,
            },
        ];
        let built = build_container(&desired, &test_cfg());
        let labels = built.config.labels.as_ref().unwrap();
        // Two routers, index suffixed. Longest prefix (/api/v1) sorts first → -0.
        let base = group_service_base("myapp", "default", "api");
        let r0 = group_service_name(&base, 0, 2);
        let r1 = group_service_name(&base, 1, 2);
        assert_eq!(
            labels
                .get(&format!("traefik.http.routers.{r0}.rule"))
                .map(String::as_str),
            Some("Host(`myapp.rise.dev`) && (Path(`/api/v1`) || PathPrefix(`/api/v1/`))")
        );
        assert_eq!(
            labels
                .get(&format!("traefik.http.routers.{r1}.rule"))
                .map(String::as_str),
            Some("Host(`myapp.rise.dev`)")
        );
    }

    #[test]
    fn longer_prefix_route_gets_higher_priority_label() {
        // Each path route gets an explicit `priority` label derived from its
        // path-prefix length, so a more-specific prefix (`/api/v1`) outranks a
        // shorter/host-only one (`/`) deterministically rather than relying on
        // Traefik's implicit rule-length heuristic (nginx longest-match parity).
        let mut desired = single_container();
        desired.container = "api".to_string();
        desired.routes = vec![
            DesiredRoute {
                hosts: vec!["myapp.rise.dev".to_string()],
                path_prefix: Some("/".to_string()),
                access: None,
            },
            DesiredRoute {
                hosts: vec!["myapp.rise.dev".to_string()],
                path_prefix: Some("/api/v1".to_string()),
                access: None,
            },
        ];
        let built = build_container(&desired, &test_cfg());
        let labels = built.config.labels.as_ref().unwrap();
        let base = group_service_base("myapp", "default", "api");
        // r0 = longest prefix (/api/v1, sorts first); r1 = host-only (/).
        let r0 = group_service_name(&base, 0, 2);
        let r1 = group_service_name(&base, 1, 2);
        let p0: usize = labels
            .get(&format!("traefik.http.routers.{r0}.priority"))
            .expect("longer-prefix route must carry a priority label")
            .parse()
            .unwrap();
        let p1: usize = labels
            .get(&format!("traefik.http.routers.{r1}.priority"))
            .expect("host-only route must carry a priority label")
            .parse()
            .unwrap();
        assert!(
            p0 > p1,
            "longer prefix /api/v1 (priority {p0}) must outrank host-only / (priority {p1})"
        );
        // Host-only route still has a positive, non-zero priority.
        assert!(p1 >= 1, "host-only route priority must be positive");
    }

    #[test]
    fn single_route_carries_a_priority_label() {
        // Even a single host-only route gets an explicit priority label (a
        // positive value), so routing is never left to Traefik's implicit
        // heuristic.
        let built = build_container(&single_container(), &test_cfg());
        let labels = built.config.labels.as_ref().unwrap();
        let base = group_service_base("myapp", "default", "app");
        assert_eq!(
            labels
                .get(&format!("traefik.http.routers.{base}.priority"))
                .map(String::as_str),
            Some("1"),
            "host-only single route gets priority 1 (path_len 0 + 1)"
        );
    }

    #[test]
    fn group_service_names_single_route_matches_emitted_service_label() {
        // A single-route container's `serverStatus` lookup name must equal the
        // bare base service the labels stamp (`{project}-{group}-{container}`).
        let desired = single_container();
        let built = build_container(&desired, &test_cfg());
        let labels = built.config.labels.unwrap();
        let base = group_service_base("myapp", "default", "app");
        // The label set carries exactly one loadbalancer service for the base.
        assert!(labels.contains_key(&format!(
            "traefik.http.services.{base}.loadbalancer.server.port"
        )));

        let names = group_service_names(&desired);
        assert_eq!(names, vec![base.clone()]);
        // Every derived name corresponds to a service the labels actually emit.
        for name in &names {
            assert!(
                labels.contains_key(&format!(
                    "traefik.http.services.{name}.loadbalancer.server.port"
                )),
                "derived service {name} must match an emitted service label"
            );
        }
    }

    #[test]
    fn group_service_names_multi_route_matches_per_route_service_labels() {
        // A multi-route container emits per-route services `{base}-{idx}`; the
        // reconciler's lookup names must match those EXACTLY (the bug behind the
        // 404 → serverStatus None fallback for multi-route containers).
        let mut desired = single_container();
        desired.container = "api".to_string();
        desired.routes = vec![
            DesiredRoute {
                hosts: vec!["myapp.rise.dev".to_string()],
                path_prefix: Some("/".to_string()),
                access: None,
            },
            DesiredRoute {
                hosts: vec!["myapp.rise.dev".to_string()],
                path_prefix: Some("/api/v1".to_string()),
                access: None,
            },
        ];
        let built = build_container(&desired, &test_cfg());
        let labels = built.config.labels.unwrap();

        // Labels emit per-route services -0 (longest prefix /api/v1) and -1 (/).
        let base = group_service_base("myapp", "default", "api");
        let svc0 = group_service_name(&base, 0, 2);
        let svc1 = group_service_name(&base, 1, 2);
        assert!(labels.contains_key(&format!(
            "traefik.http.services.{svc0}.loadbalancer.server.port"
        )));
        assert!(labels.contains_key(&format!(
            "traefik.http.services.{svc1}.loadbalancer.server.port"
        )));
        // And NO bare-base service (that name 404s in the Traefik API).
        assert!(!labels.contains_key(&format!(
            "traefik.http.services.{base}.loadbalancer.server.port"
        )));

        let names = group_service_names(&desired);
        assert_eq!(
            names,
            vec![svc0.clone(), svc1.clone()],
            "derived names must match the per-route service labels (longest prefix first)"
        );
        for name in &names {
            assert!(
                labels.contains_key(&format!(
                    "traefik.http.services.{name}.loadbalancer.server.port"
                )),
                "derived service {name} must match an emitted service label"
            );
        }
    }

    #[test]
    fn group_service_names_empty_for_non_routable_or_worker() {
        // Non-routable (router-withheld) container: no Traefik service, so no lookup.
        let mut non_routable = single_container();
        non_routable.routable = false;
        assert!(group_service_names(&non_routable).is_empty());
        // Worker (no port): never routed.
        let mut worker = single_container();
        worker.port = None;
        worker.routes = vec![];
        assert!(group_service_names(&worker).is_empty());
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
        let base = group_service_base("myapp", "default", "app");
        assert_eq!(
            labels
                .get(&format!("traefik.http.routers.{base}.tls"))
                .map(String::as_str),
            Some("true")
        );
        assert_eq!(
            labels
                .get(&format!("traefik.http.routers.{base}.tls.certresolver"))
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
            // Config values are trimmed before constructing the middleware URL.
            auth_backend_url: "  http://rise:3000/  ",
            access_classes: &map,
            ..test_cfg()
        };
        let mut desired = single_container();
        desired.access_class = "private".to_string();
        let built = build_container(&desired, &cfg);
        let labels = built.config.labels.as_ref().unwrap();
        let r = group_service_base("myapp", "default", "app");
        assert_eq!(
            labels
                .get(&format!(
                    "traefik.http.middlewares.{r}-auth.forwardauth.address"
                ))
                .map(String::as_str),
            Some("http://rise:3000/api/v1/auth/ingress?project=myapp&access=Member&signin_redirect=1")
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
    fn per_route_access_overrides_forward_auth_independently() {
        // A public project (access_class None) with two routes: `/` inherits
        // public, `/admin` is tightened to Member. Only the `/admin` router gets
        // forwardAuth, and its address stamps `&access=Member`.
        let map = public_private_map();
        let cfg = BuilderConfig {
            auth_backend_url: "http://rise:3000",
            access_classes: &map,
            ..test_cfg()
        };
        let mut desired = single_container();
        desired.access_class = "public".to_string();
        desired.routes = vec![
            DesiredRoute {
                hosts: vec!["myapp.rise.dev".to_string()],
                path_prefix: Some("/".to_string()),
                access: None,
            },
            DesiredRoute {
                hosts: vec!["myapp.rise.dev".to_string()],
                path_prefix: Some("/admin".to_string()),
                access: Some(AccessRequirement::Member),
            },
        ];
        let built = build_container(&desired, &cfg);
        let labels = built.config.labels.as_ref().unwrap();

        // Routers are indexed longest-path-prefix first, so `/admin` (idx 0)
        // precedes `/` (idx 1).
        let base = group_service_base("myapp", "default", "app");
        let member_router = group_service_name(&base, 0, 2);
        let public_router = group_service_name(&base, 1, 2);

        assert_eq!(
            labels
                .get(&format!("traefik.http.routers.{member_router}.rule"))
                .map(String::as_str),
            Some("Host(`myapp.rise.dev`) && (Path(`/admin`) || PathPrefix(`/admin/`))"),
            "the gated route must not capture a neighboring path such as /administrator"
        );

        // `/` router: no forwardAuth middleware.
        assert!(
            labels
                .get(&format!("traefik.http.routers.{public_router}.middlewares"))
                .is_none(),
            "public route must not be gated"
        );
        // `/admin` router: forwardAuth wired with `&access=Member`.
        assert_eq!(
            labels
                .get(&format!(
                    "traefik.http.middlewares.{member_router}-auth.forwardauth.address"
                ))
                .map(String::as_str),
            Some(
                "http://rise:3000/api/v1/auth/ingress?project=myapp&access=Member&signin_redirect=1"
            )
        );
        assert_eq!(
            labels
                .get(&format!("traefik.http.routers.{member_router}.middlewares"))
                .map(String::as_str),
            Some(format!("{member_router}-auth@docker").as_str())
        );
    }

    #[test]
    fn per_route_tighten_without_backend_url_withholds_whole_container() {
        // FAIL CLOSED: a public project (empty auth_backend_url is valid — no
        // access class needs auth) with a Member-tightened `/admin` route cannot
        // wire forwardAuth for `/admin`. Dropping only that router would leave the
        // `/` router serving `/admin/x` unauthenticated, so the WHOLE container
        // must be withheld — zero Traefik labels.
        let mut map = HashMap::new();
        map.insert("public".to_string(), AccessRequirement::None);
        let cfg = BuilderConfig {
            auth_backend_url: "",
            access_classes: &map,
            ..test_cfg()
        };
        let mut desired = single_container();
        desired.access_class = "public".to_string();
        desired.routes = vec![
            DesiredRoute {
                hosts: vec!["myapp.rise.dev".to_string()],
                path_prefix: Some("/".to_string()),
                access: None,
            },
            DesiredRoute {
                hosts: vec!["myapp.rise.dev".to_string()],
                path_prefix: Some("/admin".to_string()),
                access: Some(AccessRequirement::Member),
            },
        ];
        let built = build_container(&desired, &cfg);
        let labels = built.config.labels.as_ref().unwrap();
        assert!(
            !labels.keys().any(|k| k.starts_with("traefik.")),
            "a route that can't be gated must withhold the whole container, \
             not just its own router"
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
        // closed (see settings::access_classes_missing_auth_backend_url);
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
    fn routes_withheld_predicate_matches_render_and_reconciler_contract() {
        use rise_backend_core::AccessRequirement;
        let mut classes: HashMap<String, AccessRequirement> = HashMap::new();
        classes.insert("public".into(), AccessRequirement::None);
        classes.insert("members".into(), AccessRequirement::Member);
        // None requirement → never withheld, even with an empty backend URL.
        assert!(!routes_withheld("public", &classes, "", [None]));
        // Auth-required + empty URL → withheld (can't wire forwardAuth).
        assert!(routes_withheld("members", &classes, "", [None]));
        // Auth-required + configured URL → routed behind forwardAuth, not withheld.
        assert!(!routes_withheld(
            "members",
            &classes,
            "http://rise:3000",
            [None]
        ));
        // A public project tightened on one route must also fail closed.
        assert!(routes_withheld(
            "public",
            &classes,
            "",
            [None, Some(&AccessRequirement::Member)]
        ));
        assert!(routes_withheld(
            "public",
            &classes,
            "   ",
            [Some(&AccessRequirement::Member)]
        ));
        // Conversely, all-public overrides need no auth backend.
        assert!(!routes_withheld(
            "members",
            &classes,
            "",
            [Some(&AccessRequirement::None)]
        ));
        // No effective routes means no router can fail open.
        assert!(!routes_withheld(
            "members",
            &classes,
            "",
            std::iter::empty()
        ));
        // Unknown/removed classes always withhold effective routes, regardless
        // of backend URL or a route-level public override.
        assert!(routes_withheld("stale-removed", &classes, "", [None]));
        assert!(routes_withheld(
            "stale-removed",
            &classes,
            "http://rise:3000",
            [None]
        ));
        assert!(routes_withheld(
            "stale-removed",
            &classes,
            "http://rise:3000",
            [Some(&AccessRequirement::None)]
        ));
    }

    #[test]
    fn auth_required_without_backend_url_emits_no_router_fails_closed() {
        // FAIL CLOSED: a Member-requirement project whose forwardAuth can't be
        // wired (empty auth_backend_url — reachable with a stale/removed access
        // class) must NOT be stamped as an open public router. Zero Traefik
        // routers/services are emitted; the app is simply not routed (parity with
        // K8s, which exposes nothing here).
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
        assert!(
            !labels.keys().any(|k| k.starts_with("traefik.")),
            "no Traefik labels at all when auth is required but forwardAuth can't be wired"
        );
        assert!(!labels.contains_key("traefik.enable"));
        assert!(!labels
            .keys()
            .any(|k| k.starts_with("traefik.http.routers.")));

        // CONTRAST: a None-requirement project with the same empty backend URL
        // DOES get a (public, unauthenticated) router — that's its access class.
        let mut public_map = HashMap::new();
        public_map.insert("public".to_string(), AccessRequirement::None);
        let public_cfg = BuilderConfig {
            auth_backend_url: "",
            access_classes: &public_map,
            ..test_cfg()
        };
        let mut public_c = single_container();
        public_c.access_class = "public".to_string();
        let public_built = build_container(&public_c, &public_cfg);
        let public_labels = public_built.config.labels.as_ref().unwrap();
        assert_eq!(
            public_labels.get("traefik.enable").map(String::as_str),
            Some("true"),
            "None-requirement route is public and IS routed even with empty auth_backend_url"
        );
        assert!(
            !public_labels.keys().any(|k| k.contains("forwardauth")),
            "None-requirement route carries no forwardAuth"
        );

        // CONTRAST: a Member-requirement project WITH a working auth_backend_url
        // DOES get a router, behind forwardAuth.
        let set_cfg = BuilderConfig {
            auth_backend_url: "http://rise:3000",
            access_classes: &map,
            ..test_cfg()
        };
        let mut member_c = single_container();
        member_c.access_class = "private".to_string();
        let member_built = build_container(&member_c, &set_cfg);
        let member_labels = member_built.config.labels.as_ref().unwrap();
        let r = group_service_base("myapp", "default", "app");
        assert_eq!(
            member_labels.get("traefik.enable").map(String::as_str),
            Some("true"),
            "Member route IS routed when forwardAuth can be wired"
        );
        assert_eq!(
            member_labels
                .get(&format!(
                    "traefik.http.middlewares.{r}-auth.forwardauth.address"
                ))
                .map(String::as_str),
            Some("http://rise:3000/api/v1/auth/ingress?project=myapp&access=Member&signin_redirect=1")
        );
    }

    #[test]
    fn unknown_access_class_fails_closed_not_public() {
        // A project whose access_class is absent from the map must emit no
        // router, even if a route explicitly asks to be public.
        let map = public_private_map(); // does not contain "ghost"
        let cfg = BuilderConfig {
            auth_backend_url: "http://rise:3000",
            access_classes: &map,
            ..test_cfg()
        };
        let mut desired = single_container();
        desired.access_class = "ghost".to_string();
        desired.routes[0].access = Some(AccessRequirement::None);
        let built = build_container(&desired, &cfg);
        let labels = built.config.labels.as_ref().unwrap();
        assert!(
            !labels.keys().any(|key| key.starts_with("traefik.")),
            "unknown access class must suppress every router before applying public overrides"
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

    #[test]
    fn with_health_check_emits_healthcheck_labels() {
        // A health_check with path `/healthz` and period 5s →
        // healthcheck.path=/healthz, interval=5s (timeout falls back to default).
        let cfg = test_cfg();
        let mut desired = single_container();
        desired.health_path = Some("/healthz".to_string());
        desired.health_check_interval_secs = Some(5);
        desired.health_check_timeout_secs = None;
        let built = build_container(&desired, &cfg);
        let labels = built.config.labels.as_ref().unwrap();
        let svc = group_service_base("myapp", "default", "app");
        assert_eq!(
            labels
                .get(&format!(
                    "traefik.http.services.{svc}.loadbalancer.healthcheck.path"
                ))
                .map(String::as_str),
            Some("/healthz")
        );
        assert_eq!(
            labels
                .get(&format!(
                    "traefik.http.services.{svc}.loadbalancer.healthcheck.interval"
                ))
                .map(String::as_str),
            Some("5s")
        );
        assert_eq!(
            labels
                .get(&format!(
                    "traefik.http.services.{svc}.loadbalancer.healthcheck.timeout"
                ))
                .map(String::as_str),
            Some("5s"),
            "timeout falls back to the default Go duration (matching K8s) when unset"
        );
        assert_eq!(
            labels
                .get(&format!(
                    "traefik.http.services.{svc}.loadbalancer.healthcheck.scheme"
                ))
                .map(String::as_str),
            Some("http")
        );
    }

    #[test]
    fn without_health_check_emits_no_healthcheck_labels() {
        // NO health_check (ready-when-running) → no healthcheck labels: probing is
        // opt-in, so a container with no effective health path gets no Traefik
        // health check.
        let cfg = test_cfg();
        let mut desired = single_container();
        desired.health_path = None;
        desired.health_check_interval_secs = None;
        desired.health_check_timeout_secs = None;
        let built = build_container(&desired, &cfg);
        let labels = built.config.labels.as_ref().unwrap();
        assert!(
            !labels.keys().any(|k| k.contains("healthcheck")),
            "ready-when-running container must not emit healthcheck labels"
        );
    }

    #[test]
    fn healthcheck_labels_change_route_hash() {
        // The health-check labels are part of the rendered Traefik label set, so
        // toggling their presence (via the effective health path) changes the
        // route-hash and forces a recreate automatically.
        let cfg = test_cfg();
        let mut with_hc = single_container();
        with_hc.health_path = Some("/healthz".to_string());
        let mut without_hc = single_container();
        without_hc.health_path = None;
        assert_ne!(
            route_hash_for(&with_hc, &cfg),
            route_hash_for(&without_hc, &cfg),
            "emitting healthcheck labels must change the route hash"
        );
    }
}
