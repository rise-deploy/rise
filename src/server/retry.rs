//! Backoff between replays of a transaction that lost a concurrent-update race.

use std::time::Duration;

const RETRY_INITIAL_MS: u64 = 20;
const RETRY_MAX_MS: u64 = 200;

/// The pause before replaying a transaction whose `attempt`-th try lost a
/// serialization or uniqueness race.
///
/// The ceiling doubles per attempt up to [`RETRY_MAX_MS`]. Equal jitter keeps a
/// real pause while spreading contenders apart, so two transactions that
/// aborted each other do not replay in lockstep and lose again.
pub fn conflict_retry_delay(attempt: u32) -> Duration {
    use rand::RngExt;

    let ceiling_ms = RETRY_INITIAL_MS
        .saturating_mul(2u64.saturating_pow(attempt.saturating_sub(1)))
        .min(RETRY_MAX_MS);
    Duration::from_millis(rand::rng().random_range(ceiling_ms / 2..=ceiling_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_stays_within_the_exponential_cap() {
        for (attempt, ceiling) in [20, 40, 80, 160, 200, 200, 200].into_iter().enumerate() {
            for _ in 0..32 {
                let delay = conflict_retry_delay(attempt as u32 + 1);
                assert!(
                    (Duration::from_millis(ceiling / 2)..=Duration::from_millis(ceiling))
                        .contains(&delay)
                );
            }
        }
    }
}
