//! Deterministic name derivation shared by every deployment backend.
//!
//! Two families live here:
//!
//! 1. **Workload names** — `container_name` / `stable_identity_name` /
//!    `group_app_name`, all funnelled through `sanitize_and_cap` so a
//!    user-controlled project name can never produce an invalid or
//!    over-long runtime identifier.
//! 2. **Router/service names** — `group_service_base` / `group_service_name`,
//!    which carry a structural hash suffix because the sanitizer is lossy and a
//!    collision would pool two projects behind one load balancer.

use crate::desired::DesiredContainer;
use crate::labels;

/// Maximum length of a Docker container name segment we emit before hashing.
pub const MAX_NAME_LEN: usize = 63;

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
