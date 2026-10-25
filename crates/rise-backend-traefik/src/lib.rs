//! Traefik routing machinery, shared by Rise's Traefik-fronted deployment
//! backends.
//!
//! Docker and ECS both put workloads behind Traefik and configure it the same
//! way: native-provider labels publish deployment-scoped server pools, the HTTP
//! provider owns stable public routers, and the API reports readiness and the
//! applied router target. Only the native provider differs (`@docker` vs `@ecs`)
//! and, for Docker, a `traefik.docker.network` hint.
//!
//! This is deliberately **not** in `rise-backend-core`. Core is the contract
//! seam every backend shares; Traefik is a routing choice two of them happen to
//! make. Kubernetes routes with nginx ingress annotations and never touches any
//! of this, so putting it in core would have meant every backend depending on
//! one backend-group's proxy.
//!
//! What lives where:
//!
//! - [`labels`] — the Traefik label vocabulary and the hashes that make a label-set
//!   change detectable as drift.
//! - [`render`] — turning a `DesiredContainer` into a native service label set, including
//!   the fail-closed [`render::routes_withheld`] predicate that refuses to
//!   advertise a router Rise cannot authenticate.
//! - [`dynamic`] — stable public HTTP routers, middleware and TLS configuration.
//! - [`naming`] — Traefik router/service names, whose collision-resistance is a
//!   multi-tenant security property.
//! - [`api`] — the Traefik API client for `serverStatus` and applied router targets.
//! - [`readiness`] — the readiness and in-rotation verdicts that signal is used
//!   to reach.

pub mod api;
pub mod dynamic;
pub mod labels;
pub mod naming;
pub mod readiness;
pub mod render;

pub use api::TraefikApiClient;
pub use labels::{
    build_rule, hash_recreate_signature, hash_traefik_labels, normalize_certresolver,
    render_traefik_labels, sanitize_router_name, ForwardAuth, TraefikRoute,
};
pub use naming::{
    deployment_service_base, group_service_base, group_service_name, MAX_SERVICE_BASE_LEN,
};
pub use readiness::{
    replica_ready, rolling_rotation_decision, service_names_for_spec, ReadyVerdict,
    RotationDecision,
};
pub use render::{render_traefik_labels_for, route_hash_for, routes_withheld, TraefikRenderConfig};
