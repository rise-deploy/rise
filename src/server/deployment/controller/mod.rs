#[cfg(feature = "backend")]
mod kubernetes;

#[cfg(feature = "backend")]
pub use kubernetes::KubernetesBackend;

#[cfg(feature = "backend")]
pub mod docker;

#[cfg(feature = "backend")]
pub use docker::DockerBackend;

/// The `DeploymentBackend` contract and `DeploymentUrls` live in
/// `rise-backend-core`; re-exported here so existing
/// `crate::server::deployment::controller::{DeploymentBackend, DeploymentUrls}`
/// references keep working.
pub use rise_backend_core::{DeploymentBackend, DeploymentUrls};
