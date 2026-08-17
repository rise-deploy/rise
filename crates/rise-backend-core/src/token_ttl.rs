//! Workload-identity token refresh scheduling.
//!
//! Pure TTL math shared by both deployment backends to decide *when* a
//! pre-minted workload token should be re-minted and (on Kubernetes) when the
//! identity-refresh controller should schedule the project's resync. Kept
//! separate from the signing logic in `rise-backend-auth` — the signer takes a
//! TTL, it does not schedule.

/// A pre-minted workload-identity token is re-minted once it is older than half
/// its TTL — both the Kubernetes sync webhook (reuse-guard) and the Docker
/// reconciler gate re-minting on this, leaving ~half the lifetime as margin for
/// delivery to the running workload. Floored at 1s so a degenerate `ttl <= 1`
/// can't trigger a re-mint on every reconcile.
pub fn remint_after_secs(ttl_secs: u64) -> u64 {
    (ttl_secs / 2).max(1)
}

/// How long after a mint the Kubernetes identity-refresh controller schedules the
/// project's resync — strictly *later* than [`remint_after_secs`] so the
/// triggered sync actually re-mints: 2/3 of the TTL, leaving ~1/3 of the lifetime
/// for the resync round-trip + kubelet Secret-volume propagation before the old
/// token expires.
pub fn refresh_due_after_secs(ttl_secs: u64) -> u64 {
    ttl_secs * 2 / 3
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remint_is_half_floored_at_one() {
        assert_eq!(remint_after_secs(3600), 1800);
        assert_eq!(remint_after_secs(1), 1);
        assert_eq!(remint_after_secs(0), 1);
    }

    #[test]
    fn refresh_is_two_thirds_and_strictly_after_remint() {
        assert_eq!(refresh_due_after_secs(3600), 2400);
        // Strictly later than the re-mint threshold so the resync re-mints.
        for ttl in [60u64, 300, 3600, 86_400] {
            assert!(refresh_due_after_secs(ttl) > remint_after_secs(ttl));
        }
    }
}
