//! Pure `Deployment` → bollard create-spec mapping.
//!
//! Everything here is deterministic and daemon-free so it can be unit-tested:
//! the reconciler resolves the dynamic inputs (env vars, image, URLs) and calls
//! [`build_container`] to produce the [`bollard::container::Config`] plus the
//! container name and labels.

use std::collections::HashMap;

use bollard::container::Config;
use bollard::secret::{
    EndpointSettings, HealthConfig, HostConfig, RestartPolicy, RestartPolicyNameEnum,
};

use super::labels::{self, BookkeepingLabels, TraefikRoute};
use crate::server::deployment::quantity::{parse_cpu_millicores, parse_memory_bytes};

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
    /// sha256 of the secret env material; drift here forces recreation.
    pub env_secret_hash: String,
    /// Routes for this container (empty for workers / unrouted containers).
    pub routes: Vec<DesiredRoute>,
    /// Health-probe path used for the optional Docker HEALTHCHECK.
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
}

/// Result of building one container: the deterministic name and the bollard
/// create config (which already carries the network attachment).
pub struct BuiltContainer {
    pub name: String,
    pub config: Config<String>,
}

/// Maximum length of a Docker container name segment we emit before hashing.
const MAX_NAME_LEN: usize = 63;

/// Compute the deterministic container name:
/// `<prefix>_<project>_<group>_<deploymentid>_<container>`, sanitized to
/// `[a-zA-Z0-9_.-]`, hash-suffixed when longer than [`MAX_NAME_LEN`] so it
/// stays unique and the diff can still match it.
pub fn container_name(
    prefix: &str,
    project: &str,
    deployment_group: &str,
    deployment_id: &str,
    container: &str,
) -> String {
    let raw = format!("{prefix}_{project}_{deployment_group}_{deployment_id}_{container}");
    let sanitized = sanitize_name(&raw);
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
    );

    // ── Labels: bookkeeping + Traefik (routable only) ──────────────────
    let mut all_labels = BookkeepingLabels {
        label_namespace: cfg.label_namespace,
        controller_class: cfg.controller_class,
        project: &desired.project,
        deployment_group: &desired.deployment_group,
        deployment_id: &desired.deployment_id,
        deployment_uuid: &desired.deployment_uuid,
        container: &desired.container,
        environment: desired.environment.as_deref(),
        env_secret_hash: &desired.env_secret_hash,
        image: &desired.image,
    }
    .render();

    if let Some(port) = desired.port {
        // One router per (host-set × route). Single-container apps have a single
        // `/` route, so this yields exactly one router. Longest path-prefix
        // first matches the nginx semantics used by the K8s path.
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
            // Include the deployment id so a superseded (Terminating) container
            // and its replacement carry distinct Traefik router/service names
            // during a rollout — otherwise both would expose identical router
            // labels for up to one reconcile interval and collide. The Host
            // rule stays the same so traffic keeps resolving to the project.
            let base = labels::sanitize_router_name(&format!(
                "{}-{}-{}-{}",
                desired.project, desired.deployment_group, desired.deployment_id, desired.container
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
            });
            all_labels.extend(traefik);
        }
    }

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

    // ── Health check (best-effort Docker HEALTHCHECK) ──────────────────
    // The reconciler's HTTP probe is the source of truth; this is a bonus so
    // `docker ps` shows health. We use wget/curl-free shell-less form when we
    // only have a path: prefer a simple TCP-ish check via the shell.
    let healthcheck = match (desired.port, desired.health_path.as_deref()) {
        (Some(port), Some(path)) => Some(HealthConfig {
            test: Some(vec![
                "CMD-SHELL".to_string(),
                format!("wget -q -O /dev/null http://127.0.0.1:{port}{path} || exit 1"),
            ]),
            interval: Some(10_000_000_000), // 10s
            timeout: Some(5_000_000_000),   // 5s
            retries: Some(3),
            start_period: Some(10_000_000_000), // 10s
            start_interval: None,
        }),
        _ => None,
    };

    // ── Network attachment ─────────────────────────────────────────────
    let mut endpoints = HashMap::new();
    endpoints.insert(cfg.traefik_network.to_string(), EndpointSettings::default());

    let host_config = HostConfig {
        nano_cpus,
        memory,
        restart_policy: Some(RestartPolicy {
            name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
            maximum_retry_count: None,
        }),
        network_mode: Some(cfg.traefik_network.to_string()),
        ..Default::default()
    };

    let config = Config {
        image: Some(desired.image.clone()),
        env: if env.is_empty() { None } else { Some(env) },
        labels: Some(all_labels),
        healthcheck,
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

    fn test_cfg() -> BuilderConfig<'static> {
        BuilderConfig {
            label_namespace: "rise.dev",
            controller_class: "default",
            container_prefix: "rise",
            traefik_network: "rise_default",
            traefik_entrypoint: "web",
            traefik_certresolver: None,
        }
    }

    fn single_container() -> DesiredContainer {
        DesiredContainer {
            project: "myapp".to_string(),
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
            env_secret_hash: "abc123".to_string(),
            routes: vec![DesiredRoute {
                hosts: vec!["myapp.rise.dev".to_string()],
                path_prefix: None,
            }],
            health_path: Some("/".to_string()),
        }
    }

    #[test]
    fn deterministic_name() {
        let n1 = container_name("rise", "myapp", "default", "20260101-120000", "app");
        let n2 = container_name("rise", "myapp", "default", "20260101-120000", "app");
        assert_eq!(n1, n2);
        assert_eq!(n1, "rise_myapp_default_20260101-120000_app");
    }

    #[test]
    fn long_name_is_hashed_but_stable() {
        let long_project = "a".repeat(100);
        let n1 = container_name("rise", &long_project, "default", "20260101-120000", "app");
        let n2 = container_name("rise", &long_project, "default", "20260101-120000", "app");
        assert_eq!(n1, n2);
        assert!(n1.len() <= MAX_NAME_LEN);
    }

    #[test]
    fn single_container_maps_labels_env_resources() {
        let desired = single_container();
        let built = build_container(&desired, &test_cfg());

        assert_eq!(built.name, "rise_myapp_default_20260101-120000_app");

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
            labels.get("rise.dev/env-secret-hash").map(String::as_str),
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
                .get("traefik.http.routers.myapp-default-20260101-120000-app.rule")
                .map(String::as_str),
            Some("Host(`myapp.rise.dev`)")
        );
        assert_eq!(
            labels
                .get(
                    "traefik.http.services.myapp-default-20260101-120000-app.loadbalancer.server.port"
                )
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
                .get("traefik.http.routers.myapp-default-20260101-120000-api-0.rule")
                .map(String::as_str),
            Some("Host(`myapp.rise.dev`) && PathPrefix(`/api/v1`)")
        );
        assert_eq!(
            labels
                .get("traefik.http.routers.myapp-default-20260101-120000-api-1.rule")
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
                .get("traefik.http.routers.myapp-default-20260101-120000-app.tls")
                .map(String::as_str),
            Some("true")
        );
        assert_eq!(
            labels
                .get("traefik.http.routers.myapp-default-20260101-120000-app.tls.certresolver")
                .map(String::as_str),
            Some("le")
        );
    }
}
