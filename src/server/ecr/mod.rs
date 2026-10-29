mod controller;
mod manager;

pub use controller::EcrController;
pub use manager::{repository_access, EcrRepoManager};

/// Finalizer name for ECR repositories
/// Added when an ECR repo is created for a project, removed when cleanup is complete.
pub const ECR_FINALIZER: &str = "ecr.rise.dev/repository";
