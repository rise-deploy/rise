//! Rise bookkeeping labels: the vocabulary every backend uses to recognise its
//! own workloads and detect drift.
//!
//! `{ns}/managed-by=rise`, `/controller-class`, `/project`, `/deployment-group`,
//! `/deployment-id`, `/deployment-uuid`, `/container`, `/environment`,
//! `/env-hash`, `/image`, `/route-hash`, `/generation`, `/replica`.
//!
//! Backend-agnostic on purpose: Docker stamps these as container labels and ECS
//! as resource tags, but the keys and their meanings are ONE vocabulary, so
//! operator docs and debugging habits transfer between backends.
//!
//! The Traefik label machinery that used to sit alongside this now lives in
//! `rise-backend-traefik`: it belongs to the backends that front workloads with
//! Traefik, not to the seam every backend shares (Kubernetes routes with nginx
//! annotations and never touches it).

use std::collections::HashMap;

/// Bookkeeping label suffixes (joined to the configured `label_namespace`).
pub const SUFFIX_MANAGED_BY: &str = "managed-by";
pub const SUFFIX_CONTROLLER_CLASS: &str = "controller-class";
pub const SUFFIX_PROJECT: &str = "project";
/// The project's immutable identity. `project` (the name) is mutable and is
/// not a safe key for recognising a workload after a rename — the reconciler
/// prefers this tag/label when present, falling back to `project` only for
/// workloads created before it existed. See `secret_fingerprint`-style
/// leniency note on `ServiceTags::parse`: this suffix must stay OPTIONAL to
/// read (a missing value, never a required one) so an upgrade doesn't
/// suddenly find every pre-existing workload unattributable.
pub const SUFFIX_PROJECT_UUID: &str = "project-uuid";
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

/// Inputs needed to compute the bookkeeping labels for a container.
pub struct BookkeepingLabels<'a> {
    pub label_namespace: &'a str,
    pub controller_class: &'a str,
    pub project: &'a str,
    /// The project's immutable identity — see [`SUFFIX_PROJECT_UUID`].
    pub project_uuid: &'a str,
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
            ns_key(ns, SUFFIX_PROJECT_UUID),
            self.project_uuid.to_string(),
        );
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
