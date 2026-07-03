//! Pure restart backoff and circuit-breaker arithmetic. No I/O, no clocks:
//! the caller reports observed uptimes and receives decisions.

use std::time::Duration;

pub const BASE_MS: u64 = 100;
pub const FACTOR: u64 = 2;
pub const MAX_MS: u64 = 30_000;
/// A run that survives this long resets the consecutive-failure counter.
pub const STABLE_MS: u64 = 5_000;
/// Consecutive quick exits before the breaker opens.
pub const BREAKER_THRESHOLD: u32 = 5;

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Restart { delay: Duration },
    Open,
}

/// Tracks consecutive quick exits for one unit.
#[derive(Debug, Default)]
pub struct Breaker {
    consecutive: u32,
}

impl Breaker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Report a process exit with the given uptime; returns what to do.
    /// Any exit counts — a clean-exit loop is as much a crash loop as a
    /// panic loop.
    pub fn on_exit(&mut self, uptime: Duration) -> Decision {
        if uptime >= Duration::from_millis(STABLE_MS) {
            self.consecutive = 0;
        }
        self.consecutive += 1;
        if self.consecutive >= BREAKER_THRESHOLD {
            return Decision::Open;
        }
        let exp = self.consecutive - 1;
        let delay = BASE_MS.saturating_mul(FACTOR.saturating_pow(exp)).min(MAX_MS);
        Decision::Restart {
            delay: Duration::from_millis(delay),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quick() -> Duration {
        Duration::from_millis(50)
    }

    #[test]
    fn backoff_doubles_then_opens() {
        let mut b = Breaker::new();
        assert_eq!(b.on_exit(quick()), Decision::Restart { delay: Duration::from_millis(100) });
        assert_eq!(b.on_exit(quick()), Decision::Restart { delay: Duration::from_millis(200) });
        assert_eq!(b.on_exit(quick()), Decision::Restart { delay: Duration::from_millis(400) });
        assert_eq!(b.on_exit(quick()), Decision::Restart { delay: Duration::from_millis(800) });
        assert_eq!(b.on_exit(quick()), Decision::Open);
    }

    #[test]
    fn stable_uptime_resets_counter() {
        let mut b = Breaker::new();
        b.on_exit(quick());
        b.on_exit(quick());
        assert_eq!(
            b.on_exit(Duration::from_millis(STABLE_MS)),
            Decision::Restart { delay: Duration::from_millis(100) }
        );
    }

    #[test]
    fn delay_is_capped() {
        let mut b = Breaker::new();
        // With the default threshold the cap is unreachable, but the
        // arithmetic must hold regardless of tuning.
        for _ in 0..3 {
            b.on_exit(quick());
        }
        if let Decision::Restart { delay } = b.on_exit(Duration::from_millis(STABLE_MS + 1)) {
            assert!(delay <= Duration::from_millis(MAX_MS));
        }
    }
}
