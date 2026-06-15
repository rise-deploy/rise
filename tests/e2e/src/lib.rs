//! Cross-backend end-to-end test harness for Rise.
//!
//! Scenarios are written once against the [`backend::Backend`] driver seam and
//! run against either backend, so a parity gap shows up as a *declared skip*
//! (printed with a reason) rather than silent drift.
//!
//! The harness is gated on the `RISE_E2E_BACKEND` env var (`docker` | `minikube`).
//! When it is unset, every test returns immediately, so the crate never tries to
//! stand up a backend it wasn't asked to.

pub mod backend;
pub mod cli;
pub mod dex;
pub mod http;
pub mod report;
pub mod scenario;
pub mod token;

/// Which backend the current process targets. CI runs one backend per job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    Docker,
    Minikube,
}

impl BackendKind {
    /// Read the selected backend from `RISE_E2E_BACKEND`, or `None` when unset
    /// (the signal to skip the harness).
    pub fn from_env() -> Option<Self> {
        match std::env::var("RISE_E2E_BACKEND").ok().as_deref() {
            Some("docker") => Some(Self::Docker),
            Some("minikube") => Some(Self::Minikube),
            Some(other) if !other.is_empty() => {
                panic!("RISE_E2E_BACKEND={other:?} is not a known backend (docker|minikube)")
            }
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::Minikube => "minikube",
        }
    }
}
