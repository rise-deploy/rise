//! Shared deployment-backend contracts for Rise.
//!
//! This crate is the dependency seam between `rise-deploy` and the deployment
//! runtimes (Kubernetes, Docker, ECS). It holds the data models the backends
//! operate on, the `DeploymentBackend` trait, the provider traits
//! (registry/encryption), the pure `quantity` and `state_machine` helpers, the
//! `DeploymentStore` trait — the database boundary implemented in `rise-deploy`
//! over SQLX — and the runtime-agnostic reconcile machinery every backend
//! shares: desired-state shaping, env merging/hashing, deterministic naming,
//! bookkeeping labels, drift diffing, rollout gating and pod-status reporting.
//!
//! Routing is deliberately NOT here. Traefik's label machinery lives in
//! `rise-backend-traefik`, shared by the backends that front workloads with
//! Traefik (Docker, ECS); Kubernetes routes with nginx annotations and depends
//! on none of it.
//!
//! It never opens a database connection or runs a query; `sqlx` is a dependency
//! only for the `FromRow`/`Type` derives on the moved models.

pub mod backend;
pub mod custom_domain;
pub mod desired;
pub mod diff;
pub mod env;
pub mod events;
pub mod group;
pub mod health_path;
pub mod identity;
pub mod labels;
pub mod lifecycle;
pub mod logs;
pub mod models;
pub mod naming;
pub mod observation;
pub mod organization;
pub mod providers;
pub mod quantity;
pub mod rolling;
pub mod runtime;
pub mod state_machine;
pub mod store;
pub mod system_env;
pub mod token_ttl;
pub mod url_builder;

pub mod test_helpers;

pub use backend::{
    normalize_runtime_arch, AccessClass, BackendCapabilities, DeploymentBackend, DeploymentUrls,
};
pub use custom_domain::validate_custom_domain;
pub use desired::{DesiredContainer, DesiredRoute};
pub use diff::{
    diff_desired_vs_actual, identity_key, spec_key, ActualContainer, InspectedContainer,
    ReconcileAction,
};
pub use env::{hash_env, merge_container_env, pin_system_env, redact_secrets_for_hash, upsert_env};
pub use group::{normalize_deployment_group, DEFAULT_DEPLOYMENT_GROUP};
pub use health_path::effective_health_path;
pub use lifecycle::{
    complete_termination, handle_deployment_became_healthy, perform_status_transition,
    SupersededHook,
};
pub use naming::{container_name, group_app_name, sanitize_ecs_name, stable_identity_name};
pub use observation::{derive_events, ContainerObservation, DerivedEvent, ObservedState};
pub use organization::{
    controller_class_matches, resolve_namespace_prefix, resolve_namespace_prefix_fallback,
    OrganizationView, NAMESPACE_PREFIX_ANNOTATION,
};
pub use providers::{
    EncryptionProvider, ImageTagType, RegistryAuthMethod, RegistryCredentials, RegistryProvider,
};
pub use rise_deployment_spec::AccessRequirement;
pub use rolling::filter_rolling_actions;
pub use runtime::{
    effective_access_requirement, resolve_deployment_env_vars, resolve_runtime_containers,
    secret_fingerprint, should_have_infrastructure, ResolvedDeploymentEnvVars,
    DEPLOYING_TIMEOUT_MINUTES, PRE_PUSHED_TIMEOUT_MINUTES,
};
pub use store::{DeploymentStore, SupersessionOutcome};
pub use system_env::rise_system_env_vars;
pub use token_ttl::{refresh_due_after_secs, remint_after_secs};
pub use url_builder::{DeploymentUrlBuilder, IngressUrl};
