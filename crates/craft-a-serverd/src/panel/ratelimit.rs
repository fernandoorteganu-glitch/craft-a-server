//! In-memory brute-force protection for pairing.
//!
//! Two layers, both driven by an explicit `now` (unix seconds) so tests need no
//! real clock:
//!
//! * per peer IP: at most [`IP_MAX_ATTEMPTS`] attempts per [`IP_WINDOW_SECS`];
//!   going over starts a lockout of 2 minutes that doubles with every repeat
//!   (capped at an hour) and is forgotten after an hour of good behaviour;
//! * global: [`GLOBAL_MAX_FAILURES`] wrong codes within
//!   [`GLOBAL_WINDOW_SECS`], from any mix of IPs, trip a circuit breaker. The
//!   caller then wipes every outstanding pairing code and pairing stays locked
//!   for [`GLOBAL_LOCKOUT_SECS`]. This bounds a distributed guesser, which
//!   per-IP limits cannot.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;

pub const IP_MAX_ATTEMPTS: usize = 5;
pub const IP_WINDOW_SECS: i64 = 60;
pub const IP_BASE_LOCKOUT_SECS: i64 = 120;
pub const IP_MAX_LOCKOUT_SECS: i64 = 3600;
pub const GLOBAL_MAX_FAILURES: usize = 20;
pub const GLOBAL_WINDOW_SECS: i64 = 600;
pub const GLOBAL_LOCKOUT_SECS: i64 = 600;
/// Most distinct IPs tracked at once; beyond it new IPs are refused (fail closed).
pub const MAX_TRACKED_IPS: usize = 4096;
const FORGET_AFTER_SECS: i64 = 3600;

/// Why an attempt was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refused {
    pub retry_after_secs: u64,
}

/// What a recorded failure caused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureEffect {
    None,
    /// The global breaker tripped: wipe all pairing codes now.
    GlobalLockout,
}

#[derive(Debug, Default)]
struct IpState {
    attempts: VecDeque<i64>,
    strikes: u32,
    locked_until: i64,
    last_seen: i64,
}

#[derive(Debug, Default)]
pub struct PairLimiter {
    ips: HashMap<IpAddr, IpState>,
    failures: VecDeque<i64>,
    global_locked_until: i64,
}

fn secs_until(now: i64, until: i64) -> u64 {
    u64::try_from((until - now).max(1)).unwrap_or(1)
}

impl PairLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Call before processing an attempt; counts it against the IP.
    pub fn check(&mut self, ip: IpAddr, now: i64) -> Result<(), Refused> {
        if now < self.global_locked_until {
            return Err(Refused {
                retry_after_secs: secs_until(now, self.global_locked_until),
            });
        }
        if self.ips.len() >= MAX_TRACKED_IPS / 2 {
            self.ips
                .retain(|_, s| now - s.last_seen.max(s.locked_until) < FORGET_AFTER_SECS);
        }
        if self.ips.len() >= MAX_TRACKED_IPS && !self.ips.contains_key(&ip) {
            return Err(Refused {
                retry_after_secs: 60,
            });
        }
        let st = self.ips.entry(ip).or_default();
        if now < st.locked_until {
            return Err(Refused {
                retry_after_secs: secs_until(now, st.locked_until),
            });
        }
        if now - st.last_seen.max(st.locked_until) >= FORGET_AFTER_SECS {
            st.strikes = 0;
        }
        st.last_seen = now;
        while st
            .attempts
            .front()
            .is_some_and(|t| now - *t >= IP_WINDOW_SECS)
        {
            st.attempts.pop_front();
        }
        if st.attempts.len() >= IP_MAX_ATTEMPTS {
            let lock = (IP_BASE_LOCKOUT_SECS << st.strikes.min(6)).min(IP_MAX_LOCKOUT_SECS);
            st.strikes = st.strikes.saturating_add(1);
            st.locked_until = now + lock;
            st.attempts.clear();
            return Err(Refused {
                retry_after_secs: secs_until(now, st.locked_until),
            });
        }
        st.attempts.push_back(now);
        Ok(())
    }

    /// Records a wrong code. May trip the global breaker.
    pub fn record_failure(&mut self, now: i64) -> FailureEffect {
        while self
            .failures
            .front()
            .is_some_and(|t| now - *t >= GLOBAL_WINDOW_SECS)
        {
            self.failures.pop_front();
        }
        self.failures.push_back(now);
        if self.failures.len() >= GLOBAL_MAX_FAILURES {
            self.failures.clear();
            self.global_locked_until = now + GLOBAL_LOCKOUT_SECS;
            return FailureEffect::GlobalLockout;
        }
        FailureEffect::None
    }

    /// A correct code resets that IP's strikes (but not the global window).
    pub fn record_success(&mut self, ip: IpAddr) {
        if let Some(st) = self.ips.get_mut(&ip) {
            st.strikes = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(n: u8) -> IpAddr {
        IpAddr::from([192, 168, 1, n])
    }

    #[test]
    fn sixth_attempt_in_a_minute_is_refused_then_locked_out() {
        let mut l = PairLimiter::new();
        for i in 0..5 {
            assert_eq!(l.check(ip(1), 1000 + i), Ok(()), "attempt {i}");
        }
        let r = l.check(ip(1), 1006);
        assert_eq!(
            r,
            Err(Refused {
                retry_after_secs: 120
            })
        );
        // Still locked a minute later, free after the lockout.
        assert!(l.check(ip(1), 1066).is_err());
        assert_eq!(l.check(ip(1), 1006 + 120), Ok(()));
        // Other IPs are unaffected.
        assert_eq!(l.check(ip(2), 1007), Ok(()));
    }

    #[test]
    fn lockout_doubles_and_is_capped() {
        let mut l = PairLimiter::new();
        let mut now = 0;
        let mut seen = Vec::new();
        for _ in 0..8 {
            for _ in 0..5 {
                let _ = l.check(ip(1), now);
            }
            match l.check(ip(1), now) {
                Err(r) => {
                    seen.push(r.retry_after_secs);
                    now += i64::try_from(r.retry_after_secs).unwrap_or(0);
                }
                Ok(()) => seen.push(0),
            }
        }
        assert_eq!(seen, [120, 240, 480, 960, 1920, 3600, 3600, 3600]);
    }

    #[test]
    fn window_slides() {
        let mut l = PairLimiter::new();
        for i in 0..5 {
            assert_eq!(l.check(ip(1), i * 15), Ok(()));
        }
        // t=60: the attempt from t=0 left the window.
        assert_eq!(l.check(ip(1), 60), Ok(()));
        assert!(l.check(ip(1), 61).is_err());
    }

    #[test]
    fn strikes_are_forgotten_after_an_hour() {
        let mut l = PairLimiter::new();
        for _ in 0..6 {
            let _ = l.check(ip(1), 0);
        }
        let later = 10_000;
        for _ in 0..5 {
            assert_eq!(l.check(ip(1), later), Ok(()));
        }
        assert_eq!(
            l.check(ip(1), later),
            Err(Refused {
                retry_after_secs: 120
            })
        );
    }

    #[test]
    fn global_breaker_trips_across_ips_and_expires() {
        let mut l = PairLimiter::new();
        for i in 0..19 {
            assert_eq!(l.record_failure(100 + i), FailureEffect::None);
        }
        assert_eq!(l.record_failure(120), FailureEffect::GlobalLockout);
        let r = l.check(ip(9), 121);
        assert_eq!(
            r,
            Err(Refused {
                retry_after_secs: 599
            })
        );
        assert_eq!(l.check(ip(9), 120 + 600), Ok(()));
    }

    #[test]
    fn old_failures_do_not_count_toward_the_breaker() {
        let mut l = PairLimiter::new();
        for i in 0..19 {
            assert_eq!(l.record_failure(i), FailureEffect::None);
        }
        assert_eq!(l.record_failure(1000), FailureEffect::None);
    }

    #[test]
    fn tracking_table_is_bounded() {
        let mut l = PairLimiter::new();
        for n in 0..MAX_TRACKED_IPS {
            let a = IpAddr::from([10, 0, u8::try_from(n / 256).unwrap_or(0), (n % 256) as u8]);
            assert_eq!(l.check(a, 5), Ok(()));
        }
        assert!(l.check(IpAddr::from([172, 16, 0, 1]), 5).is_err());
        // Known IPs keep working, and idle entries are reclaimed later.
        assert_eq!(l.check(IpAddr::from([10, 0, 0, 0]), 6), Ok(()));
        assert_eq!(l.check(IpAddr::from([172, 16, 0, 1]), 5 + 4000), Ok(()));
    }
}
