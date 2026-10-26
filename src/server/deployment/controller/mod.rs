/// The Kubernetes deployment backend lives in the `rise-backend-kubernetes`
/// crate. Re-exported under this module path so existing
/// `crate::server::deployment::controller::kubernetes::*` references keep
/// working unchanged.
#[cfg(feature = "backend")]
pub mod kubernetes {
    pub use rise_backend_kubernetes::*;
}

#[cfg(feature = "backend")]
pub use kubernetes::KubernetesBackend;

/// The Docker deployment backend lives in the `rise-backend-docker` crate.
/// Re-exported under this module path so existing
/// `crate::server::deployment::controller::docker::*` references (the backend,
/// the reconciler, the `labels` helpers, …) keep working unchanged.
#[cfg(feature = "backend")]
pub mod docker {
    pub use rise_backend_docker::*;
}

#[cfg(feature = "backend")]
pub use docker::DockerBackend;

/// The `DeploymentBackend` contract and `DeploymentUrls` live in
/// `rise-backend-core`; re-exported here so existing
/// `crate::server::deployment::controller::{DeploymentBackend, DeploymentUrls}`
/// references keep working.
#[allow(unused_imports)]
pub use rise_backend_core::{DeploymentBackend, DeploymentUrls};
