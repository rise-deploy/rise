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

/// Maximum length of an ECS resource name (service, task-definition family).
pub const MAX_ECS_NAME_LEN: usize = 255;

/// Sanitize a name for ECS, which accepts only `[a-zA-Z0-9_-]` and at most 255
/// characters.
///
/// Separate from [`sanitize_and_cap`] because the charsets and caps genuinely
/// differ: Docker permits `.` and caps container names far shorter. Sharing one
/// function would mean either rejecting names ECS accepts or emitting names ECS
/// rejects.
///
/// Over-long names keep a readable prefix and gain a stable 10-hex-character
/// suffix derived from the full sanitized string, so truncation stays
/// deterministic and two different long names cannot collapse onto one.
pub fn sanitize_ecs_name(raw: &str) -> String {
    let sanitized: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    // A leading hyphen is legal but ugly and complicates CLI use; trim both ends.
    let sanitized = sanitized.trim_matches('-').to_string();
    if sanitized.len() <= MAX_ECS_NAME_LEN {
        return sanitized;
    }
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(sanitized.as_bytes());
    let suffix: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    let suffix = &suffix[..10];
    let keep = MAX_ECS_NAME_LEN - suffix.len() - 1;
    format!("{}-{}", &sanitized[..keep], suffix)
}

#[cfg(test)]
mod ecs_name_tests {
    use super::*;

    #[test]
    fn sanitize_ecs_name_keeps_the_legal_charset() {
        assert_eq!(
            sanitize_ecs_name("rise-myapp_default-app"),
            "rise-myapp_default-app"
        );
        // Dots are legal in Docker names but not in ECS ones.
        assert_eq!(sanitize_ecs_name("rise-my.app-app"), "rise-my-app-app");
    }

    #[test]
    fn sanitize_ecs_name_caps_and_stays_injective() {
        // A project name is user-controlled, so two long names must not truncate
        // onto one ECS resource — that would pool two projects' workloads.
        let a = sanitize_ecs_name(&format!("rise-{}-a", "x".repeat(300)));
        let b = sanitize_ecs_name(&format!("rise-{}-b", "x".repeat(300)));
        assert!(a.len() <= MAX_ECS_NAME_LEN);
        assert!(b.len() <= MAX_ECS_NAME_LEN);
        assert_ne!(a, b, "long names must not collide after truncation");
        assert_eq!(a, sanitize_ecs_name(&format!("rise-{}-a", "x".repeat(300))));
    }
}
