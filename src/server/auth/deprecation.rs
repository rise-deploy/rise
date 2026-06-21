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

/// Upper bound on distinct `(issuer, sub)` keys retained. Although `sub` now
/// comes from a *validated* token (so it is bounded by what trusted issuers
/// actually mint), this cap is defense-in-depth: a high-cardinality issuer
/// cannot grow the map without bound. Once reached, further new keys fold into
/// a single overflow bucket rather than allocating.
const MAX_TRACKED_KEYS: usize = 10_000;

/// Longest `sub` retained in a key; longer values are truncated (on a char
/// boundary) as defense-in-depth against oversized claims.
const MAX_SUB_LEN: usize = 256;

/// Sentinel key used once `MAX_TRACKED_KEYS` distinct keys are tracked, so the
/// total count of accepted raw tokens stays accurate while memory is bounded.
const OVERFLOW_KEY: (&str, &str) = ("<overflow>", "<overflow>");

/// Truncate `sub` to at most `MAX_SUB_LEN` bytes on a UTF-8 char boundary.
fn truncate_sub(sub: &str) -> String {
    if sub.len() <= MAX_SUB_LEN {
        return sub.to_string();
    }
    let mut end = MAX_SUB_LEN;
    while !sub.is_char_boundary(end) {
        end -= 1;
    }
    sub[..end].to_string()
}

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
    /// total for that `(issuer, sub)`. Callers must pass claims from a
    /// *validated* token; once `MAX_TRACKED_KEYS` distinct keys are tracked,
    /// new keys fold into a single overflow bucket so the map stays bounded.
    pub fn record_raw_external_token(&self, issuer: &str, sub: &str) -> u64 {
        let key = (issuer.to_string(), truncate_sub(sub));
        let mut map = self
            .raw_external_tokens
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let key = if map.contains_key(&key) || map.len() < MAX_TRACKED_KEYS {
            key
        } else {
            (OVERFLOW_KEY.0.to_string(), OVERFLOW_KEY.1.to_string())
        };
        let counter = map.entry(key).or_insert(0);
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

    #[test]
    fn new_keys_fold_into_overflow_bucket_at_capacity() {
        let counters = DeprecationCounters::default();
        // Fill to capacity with distinct keys.
        for i in 0..MAX_TRACKED_KEYS {
            counters.record_raw_external_token("iss", &format!("sub-{i}"));
        }
        // Already-tracked keys keep incrementing past capacity.
        assert_eq!(counters.record_raw_external_token("iss", "sub-0"), 2);
        // A brand-new key folds into the overflow bucket instead of growing.
        counters.record_raw_external_token("iss", "brand-new");
        counters.record_raw_external_token("other", "also-new");

        let snapshot = counters.snapshot_raw_external_tokens();
        // capacity distinct keys + exactly one overflow bucket.
        assert_eq!(snapshot.len(), MAX_TRACKED_KEYS + 1);
        let overflow = snapshot
            .iter()
            .find(|(i, s, _)| i == OVERFLOW_KEY.0 && s == OVERFLOW_KEY.1)
            .expect("overflow bucket present");
        assert_eq!(overflow.2, 2);
    }

    #[test]
    fn oversized_sub_is_truncated() {
        let counters = DeprecationCounters::default();
        let long_sub = "x".repeat(MAX_SUB_LEN + 50);
        counters.record_raw_external_token("iss", &long_sub);
        let snapshot = counters.snapshot_raw_external_tokens();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].1.len(), MAX_SUB_LEN);
    }

    #[test]
    fn truncate_sub_respects_char_boundaries() {
        // A multi-byte char straddling the limit must not panic or split.
        let s = format!("{}é", "a".repeat(MAX_SUB_LEN - 1));
        let truncated = truncate_sub(&s);
        assert!(truncated.len() <= MAX_SUB_LEN);
        assert!(s.starts_with(&truncated));
    }
}
