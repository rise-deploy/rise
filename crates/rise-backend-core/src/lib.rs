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

pub mod access;
pub mod backend;
pub mod group;
pub mod models;
pub mod providers;
pub mod quantity;
pub mod runtime;
pub mod state_machine;
pub mod store;
pub mod system_env;

pub use access::AccessRequirement;
pub use backend::{DeploymentBackend, DeploymentUrls};
pub use group::{normalize_deployment_group, DEFAULT_DEPLOYMENT_GROUP};
pub use providers::{
    EncryptionProvider, ImageTagType, RegistryAuthMethod, RegistryCredentials, RegistryProvider,
};
pub use runtime::{
    resolve_deployment_env_vars, resolve_runtime_containers, should_have_infrastructure,
    ResolvedDeploymentEnvVars, DEPLOYING_TIMEOUT_MINUTES, PRE_PUSHED_TIMEOUT_MINUTES,
};
pub use store::DeploymentStore;
pub use system_env::rise_system_env_vars;
