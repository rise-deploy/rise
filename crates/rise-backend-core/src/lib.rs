//! Shared deployment-backend contracts for Rise.
//!
//! This crate is the dependency seam between `rise-deploy` and the deployment
//! runtimes (Kubernetes, Docker). It holds the data models the backends operate
//! on, the `DeploymentBackend` trait, the provider traits (registry/encryption),
//! the pure `quantity` and `state_machine` helpers, and the `DeploymentStore`
//! trait — the database boundary implemented in `rise-deploy` over SQLX.
//!
//! It never opens a database connection or runs a query; `sqlx` is a dependency
//! only for the `FromRow`/`Type` derives on the moved models.

pub mod backend;
pub mod custom_domain;
pub mod group;
pub mod identity;
pub mod models;
pub mod providers;
pub mod quantity;
pub mod runtime;
pub mod state_machine;
pub mod store;
pub mod system_env;
pub mod token_ttl;
pub mod url_builder;

pub use backend::{DeploymentBackend, DeploymentUrls};
pub use custom_domain::validate_custom_domain;
pub use group::{normalize_deployment_group, DEFAULT_DEPLOYMENT_GROUP};
pub use providers::{
    EncryptionProvider, ImageTagType, RegistryAuthMethod, RegistryCredentials, RegistryProvider,
};
pub use rise_deployment_spec::AccessRequirement;
pub use runtime::{
    effective_access_requirement, resolve_deployment_env_vars, resolve_runtime_containers,
    should_have_infrastructure, ResolvedDeploymentEnvVars, DEPLOYING_TIMEOUT_MINUTES,
    PRE_PUSHED_TIMEOUT_MINUTES,
};
pub use store::DeploymentStore;
pub use system_env::rise_system_env_vars;
pub use token_ttl::{refresh_due_after_secs, remint_after_secs};
pub use url_builder::{DeploymentUrlBuilder, IngressUrl};
