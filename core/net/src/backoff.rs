// SPDX-License-Identifier: Apache-2.0

use std::time::Duration;

/// Exponential backoff with jitter for reconnects and retries. The default is spec v0 section 7.3 step 5: from one
/// second, doubling up to 60 seconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Backoff {
    pub initial: Duration,
    pub max: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(60),
        }
    }
}

impl Backoff {
    /// The wait before retry number `attempt` (0 for the first): a random point in the upper half of
    /// `min(max, initial * 2^attempt)`, so clients that lost the same server spread out but never retry at once.
    pub fn delay(&self, attempt: u32) -> Duration {
        let ceiling = self
            .initial
            .saturating_mul(1 << attempt.min(20))
            .min(self.max);
        let half = ceiling / 2;
        half + Duration::from_nanos(fastrand::u64(0..=(ceiling - half).as_nanos() as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_double_from_one_second_up_to_a_minute_with_jitter() {
        let backoff = Backoff::default();
        for _ in 0..200 {
            for (attempt, ceiling) in [
                (0, 1),
                (1, 2),
                (2, 4),
                (5, 32),
                (6, 60),
                (30, 60),
                (u32::MAX, 60),
            ] {
                let delay = backoff.delay(attempt);
                let ceiling = Duration::from_secs(ceiling);
                assert!(
                    delay >= ceiling / 2 && delay <= ceiling,
                    "attempt {attempt}: {delay:?}"
                );
            }
        }
    }

    #[test]
    fn a_zero_backoff_never_waits() {
        let backoff = Backoff {
            initial: Duration::ZERO,
            max: Duration::ZERO,
        };
        assert_eq!(backoff.delay(3), Duration::ZERO);
    }
}
