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

/// Effective HTTP health-probe path for a container, honoring its public
/// `health_check` spec (`rise.toml [containers.X.health_check]`):
///
/// - `health_check` **absent** (`None`) → `None`: HTTP probing is OPT-IN, so a
///   container with no `health_check` declared is treated as ready as soon as
///   it is *running* — matching Kubernetes, where a Pod with no readiness probe
///   is Ready once the container is up.
/// - `disabled = true` → `None`: the probe is explicitly turned off (the caller
///   treats a *running* container as ready), matching the Kubernetes behavior
///   where `health_check = false` removes the readiness/liveness probes.
/// - `path` set → that path, normalized to a leading `/`.
/// - set but no `path` (or an empty `path`) → the controller's default
///   `health_path`.
///
/// Note: the reconcile loop probes once per tick, so the spec's fine-grained
/// timing/threshold knobs (`period_seconds`, `failure_threshold`, …) and the
/// separate `liveness_enabled`/`readiness_enabled` toggles are not applied on
/// Docker — only `path` and `disabled` are. See the deployment-backends feature
/// matrix.
pub(crate) fn effective_health_path(
    spec: &crate::server::deployment::models::ContainerSpec,
    default_path: &str,
) -> Option<String> {
    // Probing is opt-in: no `health_check` declared → no probe (ready when
    // running).
    let hc = spec.health_check.as_ref()?;
    if hc.disabled {
        return None;
    }
    match hc.path.as_deref().filter(|p| !p.is_empty()) {
        Some(p) if p.starts_with('/') => Some(p.to_string()),
        Some(p) => Some(format!("/{p}")),
        None => Some(default_path.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_health_path_honors_spec() {
        use crate::server::deployment::models::{ContainerSpec, HealthCheckSpec};
        let base = ContainerSpec {
            name: "app".to_string(),
            image: None,
            port: Some(8080),
            replicas: None,
            cpu: None,
            memory: None,
            env_overrides: vec![],
            health_check: None,
        };
        // No spec → None: probing is opt-in, so an absent `health_check` means
        // no HTTP probe (the container is ready when running).
        assert_eq!(
            effective_health_path(&base, "/"),
            None,
            "absent health_check → no probe"
        );
        // `health_check` set but no path → controller default.
        let default_path = ContainerSpec {
            health_check: Some(HealthCheckSpec::default()),
            ..base.clone()
        };
        assert_eq!(
            effective_health_path(&default_path, "/livez").as_deref(),
            Some("/livez"),
            "set-but-no-path → controller default"
        );
        // disabled → None (probe turned off).
        let disabled = ContainerSpec {
            health_check: Some(HealthCheckSpec {
                disabled: true,
                ..Default::default()
            }),
            ..base.clone()
        };
        assert_eq!(effective_health_path(&disabled, "/"), None, "disabled");
        // explicit path used verbatim.
        let with_path = ContainerSpec {
            health_check: Some(HealthCheckSpec {
                path: Some("/healthz".to_string()),
                ..Default::default()
            }),
            ..base.clone()
        };
        assert_eq!(
            effective_health_path(&with_path, "/").as_deref(),
            Some("/healthz")
        );
        // path missing a leading slash is normalized.
        let no_slash = ContainerSpec {
            health_check: Some(HealthCheckSpec {
                path: Some("healthz".to_string()),
                ..Default::default()
            }),
            ..base.clone()
        };
        assert_eq!(
            effective_health_path(&no_slash, "/").as_deref(),
            Some("/healthz")
        );
        // empty path falls back to the default.
        let empty = ContainerSpec {
            health_check: Some(HealthCheckSpec {
                path: Some(String::new()),
                ..Default::default()
            }),
            ..base.clone()
        };
        assert_eq!(
            effective_health_path(&empty, "/livez").as_deref(),
            Some("/livez")
        );
    }
}
