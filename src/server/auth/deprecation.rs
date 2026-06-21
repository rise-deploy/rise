//! Deprecation signals for the legacy raw-external-token auth path.
//!
//! While `auth.allow_raw_external_tokens` is `true`, a CI service account may
//! present its raw external OIDC token directly to project-scoped endpoints
//! instead of pre-exchanging it at `POST /api/v1/auth/token`. That path is
//! deprecated and its default flips to `false` in 0.25.0. To let operators see
//! *who* still relies on it before that flip, every raw-token request is
//! counted per `(issuer, sub)` and surfaced as a metric-shaped `tracing` event
//! (Rise has no metrics endpoint — logs are the metric transport).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

/// How often the background rollup task emits the aggregate counts. Not an
/// operator setting (avoids config-schema churn) — a deprecation signal does
/// not need a tunable cadence.
pub const DEPRECATION_ROLLUP_INTERVAL: Duration = Duration::from_secs(3600);

/// Process-wide counters for deprecated auth paths.
///
/// Cumulative-since-startup and per-replica: sufficient for a deprecation
/// signal (operators take the max/last `count` per key across replicas). The
/// raw-token path already does a DB round-trip + JWKS verify, so the brief lock
/// taken here is negligible — and the path is being removed, not optimized.
#[derive(Default)]
pub struct DeprecationCounters {
    /// `(issuer, sub) -> count` of accepted raw external tokens.
    raw_external_tokens: Mutex<HashMap<(String, String), u64>>,
}

impl DeprecationCounters {
    /// Record one accepted raw external token and return the new cumulative
    /// total for that `(issuer, sub)`.
    pub fn record_raw_external_token(&self, issuer: &str, sub: &str) -> u64 {
        let mut map = self
            .raw_external_tokens
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let counter = map
            .entry((issuer.to_string(), sub.to_string()))
            .or_insert(0);
        *counter += 1;
        *counter
    }

    /// Snapshot the counters as `(issuer, sub, count)` for the rollup task.
    pub fn snapshot_raw_external_tokens(&self) -> Vec<(String, String, u64)> {
        let map = self
            .raw_external_tokens
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        map.iter()
            .map(|((issuer, sub), count)| (issuer.clone(), sub.clone(), *count))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_increments_per_issuer_sub() {
        let counters = DeprecationCounters::default();
        assert_eq!(counters.record_raw_external_token("iss-a", "sub-1"), 1);
        assert_eq!(counters.record_raw_external_token("iss-a", "sub-1"), 2);
        // Different sub under the same issuer is tracked independently.
        assert_eq!(counters.record_raw_external_token("iss-a", "sub-2"), 1);
        // Different issuer is tracked independently.
        assert_eq!(counters.record_raw_external_token("iss-b", "sub-1"), 1);
    }

    #[test]
    fn snapshot_reports_all_keys() {
        let counters = DeprecationCounters::default();
        counters.record_raw_external_token("iss-a", "sub-1");
        counters.record_raw_external_token("iss-a", "sub-1");
        counters.record_raw_external_token("iss-b", "<none>");

        let mut snapshot = counters.snapshot_raw_external_tokens();
        snapshot.sort();
        assert_eq!(
            snapshot,
            vec![
                ("iss-a".to_string(), "sub-1".to_string(), 2),
                ("iss-b".to_string(), "<none>".to_string(), 1),
            ]
        );
    }

    #[test]
    fn snapshot_empty_when_no_records() {
        let counters = DeprecationCounters::default();
        assert!(counters.snapshot_raw_external_tokens().is_empty());
    }
}
