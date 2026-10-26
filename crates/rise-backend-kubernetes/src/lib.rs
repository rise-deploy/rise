//! The Kubernetes deployment backend for Rise.
//!
//! Reconciliation runs through Metacontroller rather than an in-process loop:
//! each project has a `RiseProject` custom resource, and Metacontroller calls
//! this crate's sync webhook to ask what that project's children should be.
//! [`ResourceBuilder`] answers with Deployment/Service/Ingress/NetworkPolicy
//! specs; Metacontroller applies the diff and garbage-collects what is no
//! longer returned.
//!
//! The pieces `rise-deploy` needs are [`KubernetesBackend`] (the
//! `DeploymentBackend` implementation), [`routes::metacontroller_router`] (the
//! webhook listener's routes) and [`webhook::WebhookContext`] (their state).

pub mod backend;
pub mod config;
pub mod crd;
pub mod identity_refresh;
pub mod ip_validator;
pub mod logs;
pub mod pods;
pub mod resource_builder;
pub mod routes;
pub mod webhook;

pub use backend::KubernetesBackend;
pub use logs::KubernetesLogBackend;
pub use resource_builder::ResourceBuilder;
pub use routes::metacontroller_router;
pub use webhook::WebhookContext;

/// Install the process-wide rustls crypto provider the kube client needs.
///
/// Idempotent: a second call is a no-op rather than an error, so a process that
/// already installed one (or constructs two clients) is fine.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
