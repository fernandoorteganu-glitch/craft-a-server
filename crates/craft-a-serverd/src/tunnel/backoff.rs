//! Restart delays: exponential, capped, jittered, reset after a stable run.

use std::time::Duration;

/// Tracks consecutive failures and says how long to wait before the next try.
#[derive(Debug, Clone)]
pub struct Backoff {
    base: Duration,
    cap: Duration,
    failures: u32,
}

impl Backoff {
    pub fn new(base: Duration, cap: Duration) -> Backoff {
        Backoff {
            base,
            cap,
            failures: 0,
        }
    }

    /// Consecutive failures since the last reset.
    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// Records a failure and returns the delay before the next attempt.
    /// `jitter` in `0.0..=1.0` picks a point between half of the exponential
    /// delay and all of it ("equal jitter"), so many installs do not retry in
    /// lockstep.
    pub fn on_failure(&mut self, jitter: f64) -> Duration {
        self.failures = self.failures.saturating_add(1);
        delay_for(self.failures, self.base, self.cap, jitter)
    }

    /// The agent ran long enough to be considered healthy.
    pub fn reset(&mut self) {
        self.failures = 0;
    }
}

/// `base * 2^(failures-1)`, capped, then jittered between 50% and 100%.
pub fn delay_for(failures: u32, base: Duration, cap: Duration, jitter: f64) -> Duration {
    let exponent = failures.saturating_sub(1).min(20);
    let raw = base.checked_mul(1_u32 << exponent).unwrap_or(cap).min(cap);
    let jitter = if jitter.is_finite() {
        jitter.clamp(0.0, 1.0)
    } else {
        1.0
    };
    raw.mul_f64(0.5 + 0.5 * jitter)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: Duration = Duration::from_secs(5);
    const CAP: Duration = Duration::from_secs(300);

    #[test]
    fn grows_exponentially_then_stops_at_the_cap() {
        let secs = |n| delay_for(n, BASE, CAP, 1.0).as_secs();
        assert_eq!(
            (1..=8).map(secs).collect::<Vec<_>>(),
            [5, 10, 20, 40, 80, 160, 300, 300]
        );
        assert_eq!(secs(1000), 300);
        assert_eq!(secs(u32::MAX), 300);
    }

    #[test]
    fn jitter_stays_between_half_and_full() {
        assert_eq!(delay_for(3, BASE, CAP, 0.0), Duration::from_secs(10));
        assert_eq!(delay_for(3, BASE, CAP, 1.0), Duration::from_secs(20));
        assert_eq!(delay_for(3, BASE, CAP, f64::NAN), Duration::from_secs(20));
        assert_eq!(delay_for(3, BASE, CAP, -5.0), Duration::from_secs(10));
        assert_eq!(delay_for(3, BASE, CAP, 9.0), Duration::from_secs(20));
    }

    #[test]
    fn counts_failures_and_resets() {
        let mut b = Backoff::new(BASE, CAP);
        assert_eq!(b.on_failure(1.0), Duration::from_secs(5));
        assert_eq!(b.on_failure(1.0), Duration::from_secs(10));
        assert_eq!(b.failures(), 2);
        b.reset();
        assert_eq!(b.failures(), 0);
        assert_eq!(b.on_failure(1.0), Duration::from_secs(5));
    }

    #[test]
    fn zero_base_never_panics() {
        assert_eq!(delay_for(5, Duration::ZERO, CAP, 1.0), Duration::ZERO);
    }
}
