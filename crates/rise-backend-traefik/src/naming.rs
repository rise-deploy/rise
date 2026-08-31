//! Traefik router and service names.
//!
//! Separate from `rise_backend_core::naming` (which derives *workload* names)
//! because these name Traefik objects, and their collision-resistance is a
//! multi-tenant security property rather than a cosmetic one: `sanitize_router_name`
//! is lossy, so without the structural hash suffix two different projects could
//! collapse onto one Traefik service — and one project's labels could then
//! suppress the other's forwardAuth.

use rise_backend_core::desired::DesiredContainer;

use crate::labels;

/// Maximum length of the human-readable (sanitized) portion of a Traefik
/// router/service base name, before the `-{hex16}` injective suffix is appended.
/// Comfortably inside Traefik/Docker name limits even with the suffix and any
/// per-route `-{idx}` / `-auth` decorations [`group_service_name`] /
/// [`render_traefik_labels_for`] add on top.
pub const MAX_SERVICE_BASE_LEN: usize = 48;

/// Group-scoped Traefik router/service BASE name for a (project, group,
/// container), deployment-id-FREE so ALL routers/services/middlewares of a
/// (project, group, container) are named IDENTICALLY across every deployment of
/// the group. This lets an old and a new deployment share one Traefik service
/// (their replica containers register as servers of the same load balancer),
/// setting up health-driven rolling overlap. Mirrors the K8s group
/// Service/Ingress naming, which is likewise deployment-id-free.
///
/// **Injective.** [`sanitize_router_name`] is lossy — it lowercases and
/// collapses every run of non-`[a-z0-9]` to a single `-`, so distinct tuples
/// that differ only in separators or case (e.g. `("a.b", …)` vs `("a-b", …)`,
/// or `Foo` vs `foo`) would otherwise collapse to the SAME router/service name.
/// Two projects pooled behind one Traefik load balancer is a multi-tenant
/// boundary break (one project's labels could suppress the other's forwardAuth).
/// To guarantee distinct tuples get distinct names, we append a short stable
/// hash of the STRUCTURED `(project, group, container)` tuple — the first 8 bytes
/// (16 hex chars) of a SHA-256 over a NUL-separated (collision-free, since NUL can't
/// appear in any field) encoding of the three fields. The human-readable base
/// is length-capped first so the total stays well within name limits.
///
/// Shared by [`render_traefik_labels_for`] (which stamps the labels) and the
/// reconciler's `serverStatus` lookup (which queries `{service}@docker`) so the
/// two can't drift on how the service is named.
pub fn group_service_base(project: &str, deployment_group: &str, container: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut sanitized =
        labels::sanitize_router_name(&format!("{project}-{deployment_group}-{container}"));
    sanitized.truncate(MAX_SERVICE_BASE_LEN);
    // Trim any trailing `-` left by truncation so the base reads cleanly before
    // the suffix join (and never yields a `--` run).
    let base = sanitized.trim_end_matches('-');

    // Hash the STRUCTURED tuple (not the lossy sanitized string) with NUL field
    // separators, so two tuples collide here only if they are byte-identical.
    // 64 bits of suffix (16 hex chars): this name is a multi-tenant boundary and
    // an attacker controls their own project/group/container names, so a shorter
    // suffix would be grindable into a deliberate collision with a victim's
    // service; 64 bits makes that infeasible.
    let mut hasher = Sha256::new();
    hasher.update(project.as_bytes());
    hasher.update([0u8]);
    hasher.update(deployment_group.as_bytes());
    hasher.update([0u8]);
    hasher.update(container.as_bytes());
    let digest = hasher.finalize();
    let suffix: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();

    if base.is_empty() {
        suffix
    } else {
        format!("{base}-{suffix}")
    }
}

/// Per-route Traefik service/router name for route `route_idx` of a container
/// that emits `route_count` routes. A single-route container uses the bare
/// [`group_service_base`]; a multi-route container gets per-route services
/// `{base}-{idx}` (so multiple path prefixes don't collide on one router/service
/// name). The reconciler queries these SAME names for `serverStatus`, so the
/// derivation lives in ONE place. Routes are sorted longest-path-prefix-first by
/// [`render_traefik_labels_for`] before indexing, so callers that need the names
/// in label order must sort the same way.
pub fn group_service_name(base: &str, route_idx: usize, route_count: usize) -> String {
    if route_count > 1 {
        format!("{base}-{route_idx}")
    } else {
        base.to_string()
    }
}

/// The full set of Traefik service names a container's routable routes emit, in
/// the same order [`render_traefik_labels_for`] stamps them (longest path-prefix
/// first). Empty when the container is not routable / portless / has no host.
///
/// The reconciler reads `serverStatus` for the same service(s) via the runtime
/// [`DesiredContainer`]-free path
/// ([`super::reconciler::service_names_for_spec`]); this `DesiredContainer`-based
/// form is the reference used to assert the two derivations agree.
pub fn group_service_names(desired: &DesiredContainer) -> Vec<String> {
    if desired.port.filter(|_| desired.routable).is_none() {
        return Vec::new();
    }
    let base = group_service_base(
        &desired.project,
        &desired.deployment_group,
        &desired.container,
    );
    let mut routes = desired.routes.clone();
    routes.sort_by(|a, b| {
        let al = a.path_prefix.as_deref().unwrap_or("/").len();
        let bl = b.path_prefix.as_deref().unwrap_or("/").len();
        bl.cmp(&al)
    });
    // Enumerate over the FULL sorted list (matching `render_traefik_labels_for`,
    // where the `{base}-{idx}` index is the position in the full list and the
    // multi-route test is `routes.len() > 1`), but only emit a name for routes
    // that actually produce a router — those with at least one host.
    let route_count = routes.len();
    routes
        .iter()
        .enumerate()
        .filter(|(_, r)| !r.hosts.is_empty())
        .map(|(idx, _)| group_service_name(&base, idx, route_count))
        .collect()
}
