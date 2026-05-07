pub mod docker; // Note: File still named docker.rs but contains OciClientAuthProvider

#[cfg(feature = "backend")]
pub mod ecr;

#[cfg(feature = "backend")]
pub mod gitlab;

#[cfg(feature = "backend")]
pub mod jfrog;

pub use docker::OciClientAuthProvider;

#[cfg(feature = "backend")]
pub use ecr::EcrProvider;

#[cfg(feature = "backend")]
pub use gitlab::GitLabRegistryProvider;

#[cfg(feature = "backend")]
pub use jfrog::JfrogProvider;
