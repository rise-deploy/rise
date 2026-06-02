//! Rise bookkeeping labels + Traefik (Docker provider) label generation.
//!
//! Two label families end up on every Rise-managed container:
//!
//! 1. **Bookkeeping** — `{ns}/managed-by=rise`, `/controller-class`,
//!    `/project`, `/deployment-group`, `/deployment-id`, `/deployment-uuid`,
//!    `/container`, `/environment`, `/env-hash`, `/image`. Used by the
//!    reconciler to find Rise containers, detect drift (image / env hash), and
//!    GC orphans.
//! 2. **Traefik** — `traefik.enable`, the per-router `Host(...)` rule,
//!    entrypoint, service port and optional TLS certresolver. Only routable
//!    containers (those with a port + at least one host) get these.

use std::collections::HashMap;

/// Bookkeeping label suffixes (joined to the configured `label_namespace`).
pub const SUFFIX_MANAGED_BY: &str = "managed-by";
pub const SUFFIX_CONTROLLER_CLASS: &str = "controller-class";
pub const SUFFIX_PROJECT: &str = "project";
pub const SUFFIX_DEPLOYMENT_GROUP: &str = "deployment-group";
pub const SUFFIX_DEPLOYMENT_ID: &str = "deployment-id";
pub const SUFFIX_DEPLOYMENT_UUID: &str = "deployment-uuid";
pub const SUFFIX_CONTAINER: &str = "container";
pub const SUFFIX_ENVIRONMENT: &str = "environment";
pub const SUFFIX_ENV_HASH: &str = "env-hash";
pub const SUFFIX_IMAGE: &str = "image";
/// sha256 of the fully-rendered Traefik label set (empty string when the
/// container is not routable). Lets the diff detect routing transitions —
/// active↔inactive — that Docker can't apply to a running container in place.
pub const SUFFIX_ROUTE_HASH: &str = "route-hash";

/// Build a namespaced bookkeeping label key, e.g. `rise.dev/project`.
pub fn ns_key(label_namespace: &str, suffix: &str) -> String {
    format!("{label_namespace}/{suffix}")
}

/// Inputs needed to compute the bookkeeping labels for a container.
pub struct BookkeepingLabels<'a> {
    pub label_namespace: &'a str,
    pub controller_class: &'a str,
    pub project: &'a str,
    pub deployment_group: &'a str,
    pub deployment_id: &'a str,
    pub deployment_uuid: &'a str,
    pub container: &'a str,
    pub environment: Option<&'a str>,
    pub env_hash: &'a str,
    pub image: &'a str,
    /// sha256 of the fully-rendered Traefik label set (empty string for a
    /// non-routable container). Stamped as `{ns}/route-hash` so the reconciler
    /// can detect when routability/routing changed and recreate the container.
    pub route_hash: &'a str,
}

impl BookkeepingLabels<'_> {
    /// Render the bookkeeping label map.
    pub fn render(&self) -> HashMap<String, String> {
        let ns = self.label_namespace;
        let mut labels = HashMap::new();
        labels.insert(ns_key(ns, SUFFIX_MANAGED_BY), "rise".to_string());
        labels.insert(
            ns_key(ns, SUFFIX_CONTROLLER_CLASS),
            self.controller_class.to_string(),
        );
        labels.insert(ns_key(ns, SUFFIX_PROJECT), self.project.to_string());
        labels.insert(
            ns_key(ns, SUFFIX_DEPLOYMENT_GROUP),
            self.deployment_group.to_string(),
        );
        labels.insert(
            ns_key(ns, SUFFIX_DEPLOYMENT_ID),
            self.deployment_id.to_string(),
        );
        labels.insert(
            ns_key(ns, SUFFIX_DEPLOYMENT_UUID),
            self.deployment_uuid.to_string(),
        );
        labels.insert(ns_key(ns, SUFFIX_CONTAINER), self.container.to_string());
        if let Some(env) = self.environment {
            labels.insert(ns_key(ns, SUFFIX_ENVIRONMENT), env.to_string());
        }
        labels.insert(ns_key(ns, SUFFIX_ENV_HASH), self.env_hash.to_string());
        labels.insert(ns_key(ns, SUFFIX_IMAGE), self.image.to_string());
        labels.insert(ns_key(ns, SUFFIX_ROUTE_HASH), self.route_hash.to_string());
        labels
    }
}

/// Stable sha256 of a rendered Traefik label map, used as the `route-hash`
/// drift label. Hashes the map over a deterministically key-sorted copy with
/// length-prefixed key/value framing so reordering can't change the digest
/// while any add/edit/remove of a routing label does. An empty map (a
/// non-routable container) yields the digest of the empty input, which is
/// stable and distinct from any non-empty routing set.
pub fn hash_traefik_labels(labels: &HashMap<String, String>) -> String {
    use sha2::{Digest, Sha256};
    let mut entries: Vec<(&String, &String)> = labels.iter().collect();
    entries.sort();
    let mut hasher = Sha256::new();
    for (k, v) in entries {
        hasher.update((k.len() as u64).to_le_bytes());
        hasher.update(k.as_bytes());
        hasher.update((v.len() as u64).to_le_bytes());
        hasher.update(v.as_bytes());
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Configuration needed to render Traefik labels for one routable container.
pub struct TraefikRoute<'a> {
    /// Sanitized, unique router/service name (see [`sanitize_router_name`]).
    pub router_name: &'a str,
    /// Hosts the router should match, in priority order.
    pub hosts: &'a [String],
    /// Optional path prefix (multi-container routes). `None` → host-only rule.
    pub path_prefix: Option<&'a str>,
    /// Container port Traefik load-balances to.
    pub port: u16,
    /// Traefik entrypoint name (e.g. `web`).
    pub entrypoint: &'a str,
    /// Docker network shared with Traefik.
    pub network: &'a str,
    /// Optional certresolver — when set the router gets TLS labels.
    pub certresolver: Option<&'a str>,
}

/// Build a Traefik `Host(...)` (+ optional `PathPrefix(...)`) rule string.
///
/// Multiple hosts are OR-ed: ``Host(`a`) || Host(`b`)``. When `path_prefix` is
/// set the whole host alternation is grouped and AND-ed with the prefix:
/// ``(Host(`a`) || Host(`b`)) && PathPrefix(`/p`)``.
pub fn build_rule(hosts: &[String], path_prefix: Option<&str>) -> String {
    let host_rule = hosts
        .iter()
        .map(|h| format!("Host(`{h}`)"))
        .collect::<Vec<_>>()
        .join(" || ");
    match path_prefix {
        Some(prefix) if !prefix.is_empty() && prefix != "/" => {
            if hosts.len() > 1 {
                format!("({host_rule}) && PathPrefix(`{prefix}`)")
            } else {
                format!("{host_rule} && PathPrefix(`{prefix}`)")
            }
        }
        _ => host_rule,
    }
}

/// Sanitize a string into a Traefik router/service name: lowercase, with any
/// character outside `[a-z0-9-]` collapsed to `-`, trimmed of leading/trailing
/// `-`. Deterministic so the reconciler can match an existing container's
/// router back to its desired form.
pub fn sanitize_router_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_dash = false;
    for ch in raw.chars() {
        let mapped = if ch.is_ascii_alphanumeric() {
            ch.to_ascii_lowercase()
        } else {
            '-'
        };
        if mapped == '-' {
            if last_dash {
                continue;
            }
            last_dash = true;
        } else {
            last_dash = false;
        }
        out.push(mapped);
    }
    out.trim_matches('-').to_string()
}

/// Render the Traefik labels for one routable container. Returns an empty map
/// when there are no hosts to route (no router can be formed).
pub fn render_traefik_labels(route: &TraefikRoute<'_>) -> HashMap<String, String> {
    let mut labels = HashMap::new();
    if route.hosts.is_empty() {
        return labels;
    }
    let r = route.router_name;
    labels.insert("traefik.enable".to_string(), "true".to_string());
    labels.insert(
        "traefik.docker.network".to_string(),
        route.network.to_string(),
    );
    labels.insert(
        format!("traefik.http.routers.{r}.rule"),
        build_rule(route.hosts, route.path_prefix),
    );
    labels.insert(
        format!("traefik.http.routers.{r}.entrypoints"),
        route.entrypoint.to_string(),
    );
    labels.insert(
        format!("traefik.http.services.{r}.loadbalancer.server.port"),
        route.port.to_string(),
    );
    if let Some(certresolver) = route.certresolver {
        labels.insert(format!("traefik.http.routers.{r}.tls"), "true".to_string());
        labels.insert(
            format!("traefik.http.routers.{r}.tls.certresolver"),
            certresolver.to_string(),
        );
    }
    labels
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_router_name_lowercases_and_collapses() {
        assert_eq!(sanitize_router_name("My-App"), "my-app");
        assert_eq!(sanitize_router_name("proj_default_app"), "proj-default-app");
        assert_eq!(sanitize_router_name("a//b..c"), "a-b-c");
        assert_eq!(
            sanitize_router_name("--leading--trailing--"),
            "leading-trailing"
        );
        assert_eq!(sanitize_router_name("mr/123"), "mr-123");
    }

    #[test]
    fn build_rule_single_host() {
        assert_eq!(
            build_rule(&["app.rise.dev".to_string()], None),
            "Host(`app.rise.dev`)"
        );
    }

    #[test]
    fn build_rule_multi_host_joins_with_or() {
        assert_eq!(
            build_rule(&["a.rise.dev".to_string(), "b.rise.dev".to_string()], None),
            "Host(`a.rise.dev`) || Host(`b.rise.dev`)"
        );
    }

    #[test]
    fn build_rule_with_path_prefix() {
        assert_eq!(
            build_rule(&["a.rise.dev".to_string()], Some("/api")),
            "Host(`a.rise.dev`) && PathPrefix(`/api`)"
        );
        // Multi-host gets grouped before AND-ing the prefix.
        assert_eq!(
            build_rule(
                &["a.rise.dev".to_string(), "b.rise.dev".to_string()],
                Some("/api")
            ),
            "(Host(`a.rise.dev`) || Host(`b.rise.dev`)) && PathPrefix(`/api`)"
        );
    }

    #[test]
    fn build_rule_root_prefix_is_host_only() {
        // "/" and "" are treated as "no prefix" — the whole host serves it.
        assert_eq!(
            build_rule(&["a.rise.dev".to_string()], Some("/")),
            "Host(`a.rise.dev`)"
        );
        assert_eq!(
            build_rule(&["a.rise.dev".to_string()], Some("")),
            "Host(`a.rise.dev`)"
        );
    }

    #[test]
    fn traefik_labels_without_certresolver_have_no_tls() {
        let labels = render_traefik_labels(&TraefikRoute {
            router_name: "app",
            hosts: &["app.rise.dev".to_string()],
            path_prefix: None,
            port: 8080,
            entrypoint: "web",
            network: "rise_default",
            certresolver: None,
        });
        assert_eq!(
            labels.get("traefik.enable").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            labels.get("traefik.docker.network").map(String::as_str),
            Some("rise_default")
        );
        assert_eq!(
            labels
                .get("traefik.http.routers.app.rule")
                .map(String::as_str),
            Some("Host(`app.rise.dev`)")
        );
        assert_eq!(
            labels
                .get("traefik.http.services.app.loadbalancer.server.port")
                .map(String::as_str),
            Some("8080")
        );
        assert!(!labels.contains_key("traefik.http.routers.app.tls"));
        assert!(!labels.contains_key("traefik.http.routers.app.tls.certresolver"));
    }

    #[test]
    fn traefik_labels_with_certresolver_gate_tls() {
        let labels = render_traefik_labels(&TraefikRoute {
            router_name: "app",
            hosts: &["app.rise.dev".to_string()],
            path_prefix: None,
            port: 8080,
            entrypoint: "websecure",
            network: "rise_default",
            certresolver: Some("le"),
        });
        assert_eq!(
            labels
                .get("traefik.http.routers.app.tls")
                .map(String::as_str),
            Some("true")
        );
        assert_eq!(
            labels
                .get("traefik.http.routers.app.tls.certresolver")
                .map(String::as_str),
            Some("le")
        );
    }

    #[test]
    fn traefik_labels_empty_when_no_hosts() {
        let labels = render_traefik_labels(&TraefikRoute {
            router_name: "app",
            hosts: &[],
            path_prefix: None,
            port: 8080,
            entrypoint: "web",
            network: "rise_default",
            certresolver: None,
        });
        assert!(labels.is_empty());
    }

    #[test]
    fn hash_traefik_labels_is_order_independent_and_distinguishes_content() {
        let mut a = HashMap::new();
        a.insert("traefik.enable".to_string(), "true".to_string());
        a.insert(
            "traefik.http.routers.x.rule".to_string(),
            "Host(`a`)".to_string(),
        );
        // Same entries, inserted in a different order → same hash.
        let mut b = HashMap::new();
        b.insert(
            "traefik.http.routers.x.rule".to_string(),
            "Host(`a`)".to_string(),
        );
        b.insert("traefik.enable".to_string(), "true".to_string());
        assert_eq!(hash_traefik_labels(&a), hash_traefik_labels(&b));

        // A changed value (different Host) yields a different hash.
        let mut c = a.clone();
        c.insert(
            "traefik.http.routers.x.rule".to_string(),
            "Host(`b`)".to_string(),
        );
        assert_ne!(hash_traefik_labels(&a), hash_traefik_labels(&c));

        // The empty (non-routable) set is stable and distinct from any non-empty
        // set, so active↔inactive transitions always register as drift.
        let empty = HashMap::new();
        assert_eq!(
            hash_traefik_labels(&empty),
            hash_traefik_labels(&HashMap::new())
        );
        assert_ne!(hash_traefik_labels(&empty), hash_traefik_labels(&a));
    }
}
