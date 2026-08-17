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
/// Monotonic generation counter for a container slot's identity tuple
/// (project, group, deployment-id, container, replica). Starts at 1; bumped on
/// every recreate so the new container's NAME (`..._g{n}`) is visibly newer than
/// the one it replaced. NOT part of any matching key or hash — purely cosmetic
/// plus the source for computing the next generation.
pub const SUFFIX_GENERATION: &str = "generation";
/// Zero-based replica index for a container slot. Part of the stable identity
/// tuple so each replica of a spec is matched/recreated independently, but
/// deliberately NOT part of the network alias / `RISE_CONTAINER_HOST__` discovery
/// host (all replicas share one replica-free alias so Docker DNS round-robins)
/// and NOT part of the Traefik labels (all replicas share one router+service so
/// Traefik load-balances across them).
pub const SUFFIX_REPLICA: &str = "replica";

/// Build a namespaced bookkeeping label key, e.g. `rise.dev/project`.
pub fn ns_key(label_namespace: &str, suffix: &str) -> String {
    format!("{label_namespace}/{suffix}")
}

/// Normalize a configured Traefik certresolver into the effective value the
/// controller should use. An empty or whitespace-only string (e.g. the
/// `${RISE_CERTRESOLVER:-}` default in the local HTTP config) means "no TLS"
/// and collapses to `None`, so no broken `tls.certresolver=` label is stamped.
/// A non-blank value is trimmed and kept.
pub fn normalize_certresolver(certresolver: Option<String>) -> Option<String> {
    certresolver
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
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
    /// Monotonic generation of this container; rendered as `{ns}/generation`.
    /// Drives the `..._g{n}` name suffix. NOT fed into any hash or matching key.
    pub generation: u32,
    /// Zero-based replica index of this container within its spec; rendered as
    /// `{ns}/replica`. Part of the stable identity tuple (so each replica is
    /// reconciled independently) and folded into the `..._r{n}` name segment, but
    /// NOT fed into any hash, the network alias, or the Traefik labels.
    pub replica: u32,
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
        // Cosmetic bookkeeping only — never read by `hash_traefik_labels` /
        // `hash_recreate_signature`, so the generation never affects the
        // recreate signature (no per-generation recreate loop).
        labels.insert(ns_key(ns, SUFFIX_GENERATION), self.generation.to_string());
        // Replica index — part of the identity tuple, read back in
        // `list_actual_containers`. Like the generation it is never fed into any
        // routing/recreate hash.
        labels.insert(ns_key(ns, SUFFIX_REPLICA), self.replica.to_string());
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

/// Recreate-signature hash stamped as the `route-hash` bookkeeping label.
///
/// Extends [`hash_traefik_labels`] with create-time-only `HostConfig` properties
/// the diff must detect but Docker can't change on a running container in place —
/// currently whether the app port is published to a loopback host port
/// (`publish_app_ports`). Folding it into the same hash the diff already compares
/// means toggling port publishing (or a container created before the setting was
/// enabled) registers as drift and forces a recreate, exactly like a Traefik
/// label change — with no extra field to compare.
pub fn hash_recreate_signature(labels: &HashMap<String, String>, publish_port: bool) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    // Compose over the label hash + a domain-separated publish bit, so the
    // result still changes if either the labels or the publish intent change.
    hasher.update(hash_traefik_labels(labels).as_bytes());
    hasher.update(b"|publish_port:");
    hasher.update([publish_port as u8]);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// forwardAuth middleware spec for a routable container. Mirrors the
/// Kubernetes nginx `auth-url`/`auth-response-headers` annotations: Traefik
/// issues a subrequest to `address` before proxying, and copies the named
/// response headers (`auth_response_headers`) from a 2xx auth response onto the
/// forwarded request.
pub struct ForwardAuth<'a> {
    /// URL Traefik calls for the auth subrequest (Rise's
    /// `/api/v1/auth/ingress`). Includes the `signin_redirect=1` query so the
    /// handler 302-redirects unauthenticated browsers to the login page.
    pub address: &'a str,
    /// Comma-separated response headers copied from the auth response onto the
    /// proxied request (e.g. `X-Auth-Request-Email,X-Auth-Request-User`).
    pub auth_response_headers: &'a str,
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
    /// Optional forwardAuth middleware — when set the router gets a
    /// `{router}-auth` middleware enforcing ingress authentication.
    pub forward_auth: Option<ForwardAuth<'a>>,
}

/// Build a Traefik `Host(...)` (+ optional segment-bounded path-prefix) rule.
///
/// Multiple hosts are OR-ed: ``Host(`a`) || Host(`b`)``. A non-root route
/// matches the path itself and descendants on a slash boundary — `/api` and
/// `/api/...`, but not `/apiculture`. This mirrors Kubernetes `Prefix` path
/// semantics and prevents a less-restrictive route from widening its access
/// policy to neighboring paths.
pub fn build_rule(hosts: &[String], path_prefix: Option<&str>) -> String {
    let host_rule = hosts
        .iter()
        .map(|h| format!("Host(`{h}`)"))
        .collect::<Vec<_>>()
        .join(" || ");
    match path_prefix {
        Some(prefix) if !prefix.is_empty() && prefix != "/" => {
            // Kubernetes Prefix semantics ignore a trailing slash, so normalize
            // `/api/` to `/api`. Preserve all-slash inputs such as `//` instead
            // of collapsing them to an empty base and accidentally widening the
            // rule to host-wide `PathPrefix(`/`)`.
            let trimmed = prefix.trim_end_matches('/');
            let segment = if trimmed.is_empty() { prefix } else { trimmed };
            let descendant_prefix = if trimmed.is_empty() {
                segment.to_string()
            } else {
                format!("{segment}/")
            };
            let path_rule = format!("(Path(`{segment}`) || PathPrefix(`{descendant_prefix}`))");
            if hosts.len() > 1 {
                format!("({host_rule}) && {path_rule}")
            } else {
                format!("{host_rule} && {path_rule}")
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
    // Bind the router to its own service explicitly. A container serving more
    // than one route (multiple `[routes]` → the same container) defines several
    // services, and Traefik refuses to auto-link a router when its container has
    // multiple services ("cannot be linked automatically with multiple
    // Services"). Naming the service the same as the router keeps this
    // unambiguous.
    labels.insert(format!("traefik.http.routers.{r}.service"), r.to_string());
    if let Some(certresolver) = route.certresolver {
        labels.insert(format!("traefik.http.routers.{r}.tls"), "true".to_string());
        labels.insert(
            format!("traefik.http.routers.{r}.tls.certresolver"),
            certresolver.to_string(),
        );
    }
    if let Some(fa) = &route.forward_auth {
        labels.insert(
            format!("traefik.http.middlewares.{r}-auth.forwardauth.address"),
            fa.address.to_string(),
        );
        labels.insert(
            format!("traefik.http.middlewares.{r}-auth.forwardauth.authResponseHeaders"),
            fa.auth_response_headers.to_string(),
        );
        labels.insert(
            format!("traefik.http.routers.{r}.middlewares"),
            format!("{r}-auth@docker"),
        );
    }
    labels
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_certresolver_blank_is_none() {
        // None stays None; empty / whitespace-only collapse to None so the
        // `${RISE_CERTRESOLVER:-}` local default means "no TLS".
        assert_eq!(normalize_certresolver(None), None);
        assert_eq!(normalize_certresolver(Some(String::new())), None);
        assert_eq!(normalize_certresolver(Some("   ".to_string())), None);
        assert_eq!(normalize_certresolver(Some("\t\n".to_string())), None);
        // A real value is trimmed and kept.
        assert_eq!(
            normalize_certresolver(Some("le".to_string())),
            Some("le".to_string())
        );
        assert_eq!(
            normalize_certresolver(Some("  le  ".to_string())),
            Some("le".to_string())
        );
    }

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
            "Host(`a.rise.dev`) && (Path(`/api`) || PathPrefix(`/api/`))"
        );
        // Multi-host gets grouped before AND-ing the prefix.
        assert_eq!(
            build_rule(
                &["a.rise.dev".to_string(), "b.rise.dev".to_string()],
                Some("/api")
            ),
            "(Host(`a.rise.dev`) || Host(`b.rise.dev`)) && (Path(`/api`) || PathPrefix(`/api/`))"
        );
    }

    #[test]
    fn build_rule_path_prefix_is_segment_bounded() {
        let rule = build_rule(&["a.rise.dev".to_string()], Some("/health/"));
        assert_eq!(
            rule,
            "Host(`a.rise.dev`) && (Path(`/health`) || PathPrefix(`/health/`))"
        );
        assert!(rule.contains("Path(`/health`)"));
        assert!(!rule.contains("PathPrefix(`/health`)"));
    }

    #[test]
    fn build_rule_preserves_trailing_slashes_without_widening_to_root() {
        assert_eq!(
            build_rule(&["a.rise.dev".to_string()], Some("//")),
            "Host(`a.rise.dev`) && (Path(`//`) || PathPrefix(`//`))"
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
            forward_auth: None,
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
            forward_auth: None,
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
            forward_auth: None,
        });
        assert!(labels.is_empty());
    }

    #[test]
    fn traefik_labels_without_forward_auth_emit_no_middleware() {
        let labels = render_traefik_labels(&TraefikRoute {
            router_name: "app",
            hosts: &["app.rise.dev".to_string()],
            path_prefix: None,
            port: 8080,
            entrypoint: "web",
            network: "rise_default",
            certresolver: None,
            forward_auth: None,
        });
        assert!(!labels
            .keys()
            .any(|k| k.contains("forwardauth") || k.ends_with(".middlewares")));
    }

    #[test]
    fn traefik_labels_bind_router_to_its_own_service() {
        // Each router must explicitly name its service so a container serving
        // more than one route (several services) doesn't hit Traefik's
        // "cannot be linked automatically with multiple Services".
        let labels = render_traefik_labels(&TraefikRoute {
            router_name: "app-1",
            hosts: &["app.rise.dev".to_string()],
            path_prefix: Some("/api"),
            port: 8080,
            entrypoint: "web",
            network: "rise_default",
            certresolver: None,
            forward_auth: None,
        });
        assert_eq!(
            labels
                .get("traefik.http.routers.app-1.service")
                .map(String::as_str),
            Some("app-1")
        );
        assert!(labels.contains_key("traefik.http.services.app-1.loadbalancer.server.port"));
    }

    #[test]
    fn traefik_labels_with_forward_auth_emit_middleware() {
        let labels = render_traefik_labels(&TraefikRoute {
            router_name: "app",
            hosts: &["app.rise.dev".to_string()],
            path_prefix: None,
            port: 8080,
            entrypoint: "web",
            network: "rise_default",
            certresolver: None,
            forward_auth: Some(ForwardAuth {
                address: "http://rise:3000/api/v1/auth/ingress?project=app&signin_redirect=1",
                auth_response_headers: "X-Auth-Request-Email,X-Auth-Request-User",
            }),
        });
        assert_eq!(
            labels
                .get("traefik.http.middlewares.app-auth.forwardauth.address")
                .map(String::as_str),
            Some("http://rise:3000/api/v1/auth/ingress?project=app&signin_redirect=1")
        );
        assert_eq!(
            labels
                .get("traefik.http.middlewares.app-auth.forwardauth.authResponseHeaders")
                .map(String::as_str),
            Some("X-Auth-Request-Email,X-Auth-Request-User")
        );
        assert_eq!(
            labels
                .get("traefik.http.routers.app.middlewares")
                .map(String::as_str),
            Some("app-auth@docker")
        );
    }

    #[test]
    fn route_hash_differs_with_and_without_forward_auth() {
        let base = TraefikRoute {
            router_name: "app",
            hosts: &["app.rise.dev".to_string()],
            path_prefix: None,
            port: 8080,
            entrypoint: "web",
            network: "rise_default",
            certresolver: None,
            forward_auth: None,
        };
        let without = render_traefik_labels(&base);
        let with = render_traefik_labels(&TraefikRoute {
            forward_auth: Some(ForwardAuth {
                address: "http://rise:3000/api/v1/auth/ingress?project=app&signin_redirect=1",
                auth_response_headers: "X-Auth-Request-Email,X-Auth-Request-User",
            }),
            ..base
        });
        assert_ne!(
            hash_traefik_labels(&without),
            hash_traefik_labels(&with),
            "forward_auth must change the route hash so access-class changes trigger recreate"
        );
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

    #[test]
    fn hash_recreate_signature_folds_in_publish_bit() {
        let mut labels = HashMap::new();
        labels.insert("traefik.enable".to_string(), "true".to_string());

        let off = hash_recreate_signature(&labels, false);
        let on = hash_recreate_signature(&labels, true);
        // The publish bit changes the signature (drives the recreate at toggle).
        assert_ne!(off, on);
        // Deterministic for the same inputs.
        assert_eq!(off, hash_recreate_signature(&labels, false));
        // Distinct from the bare label hash, so containers stamped with the old
        // label-only hash recreate once to converge onto the new signature.
        assert_ne!(off, hash_traefik_labels(&labels));
    }
}
