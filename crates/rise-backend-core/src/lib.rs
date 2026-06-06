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
pub mod models;
pub mod providers;
pub mod quantity;
pub mod state_machine;

pub use backend::{DeploymentBackend, DeploymentUrls};
pub use providers::{
    EncryptionProvider, ImageTagType, RegistryAuthMethod, RegistryCredentials, RegistryProvider,
};
