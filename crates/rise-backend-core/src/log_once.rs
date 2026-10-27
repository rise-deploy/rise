//! Report a persistent configuration state once rather than on every evaluation.
//!
//! Host resolution runs in hot loops — every reconcile tick of the Docker and
//! ECS controllers, and every Traefik poll of the HTTP-provider snapshot — so a
//! misconfiguration it notices (a suppressed deployment-group host, an ignored
//! custom domain) would otherwise be logged every few seconds for as long as it
//! lasts. [`first_report`] lets the caller log it at `warn` the first time and
//! at `debug` afterwards.

use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};

/// How many distinct states are remembered. Past this the set is cleared, so
/// memory stays bounded and a long-lived state is at worst reported again.
const CAPACITY: usize = 10_000;

static REPORTED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);

/// Whether this process is reporting `key` for the first time.
///
/// `key` must identify the state, not just its kind: include the project,
/// group, host or domain it concerns, so each distinct state is reported once.
pub fn first_report(key: String) -> bool {
    let mut reported = REPORTED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if reported.contains(&key) {
        return false;
    }
    if reported.len() >= CAPACITY {
        reported.clear();
    }
    reported.insert(key)
}

/// Log at `warn` the first time `key` is reported in this process, and at
/// `debug` on every later evaluation of the same state.
macro_rules! warn_once {
    ($key:expr, $($arg:tt)+) => {
        if $crate::log_once::first_report($key) {
            tracing::warn!($($arg)+);
        } else {
            tracing::debug!($($arg)+);
        }
    };
}
pub(crate) use warn_once;

#[cfg(test)]
mod tests {
    use super::first_report;

    #[test]
    fn reports_each_key_once() {
        assert!(first_report("log_once-test:a".to_string()));
        assert!(!first_report("log_once-test:a".to_string()));
        assert!(first_report("log_once-test:b".to_string()));
    }
}
