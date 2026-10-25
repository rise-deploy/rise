//! Pure health-probe helpers: the effective HTTP probe path for a container
//! spec and the human-readable detail for a probe transport error. Both are
//! `&self`-free and unit-testable without a daemon.

/// Concise, human-readable detail for a probe `reqwest::Error`: prefer the
/// underlying transport source (e.g. "connection refused", "timed out") over
/// reqwest's verbose wrapper text, so the surfaced reason is actionable.
pub(crate) fn probe_error_detail(e: &reqwest::Error) -> String {
    use std::error::Error;
    if let Some(src) = e.source() {
        src.to_string()
    } else if e.is_timeout() {
        "timed out".to_string()
    } else if e.is_connect() {
        "connection error".to_string()
    } else {
        e.to_string()
    }
}

/// Re-exported from `rise-backend-core`: the probe policy is backend-agnostic
/// and shared with the ECS backend.
pub(crate) use rise_backend_core::health_path::effective_health_path;
