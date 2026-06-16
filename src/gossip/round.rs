//! Gossip round scheduling logic.

use std::time::{Duration, Instant};

pub const ACTIVE_ROUND_INTERVAL_MS: u64 = 500;
pub const IDLE_ROUND_INTERVAL_MS: u64 = 5_000;
pub const IDLE_TIMEOUT_SEC: u64 = 30;

pub struct GossipScheduler {
    last_round_time: Instant,
    pub last_active_msg_time: Instant,
}

impl GossipScheduler {
    /// Creates a new GossipScheduler initialized to the current time.
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            last_round_time: now,
            last_active_msg_time: now,
        }
    }

    pub fn record_activity(&mut self) {
        self.last_active_msg_time = Instant::now();
    }

    pub fn is_time_for_round(&self) -> bool {
        let interval = if self.is_idle() {
            Duration::from_millis(IDLE_ROUND_INTERVAL_MS)
        } else {
            Duration::from_millis(ACTIVE_ROUND_INTERVAL_MS)
        };
        self.last_round_time.elapsed() >= interval
    }

    pub fn round_executed(&mut self) {
        self.last_round_time = Instant::now();
    }

    pub fn is_idle(&self) -> bool {
        self.last_active_msg_time.elapsed() >= Duration::from_secs(IDLE_TIMEOUT_SEC)
    }

    pub fn current_interval(&self) -> Duration {
        if self.is_idle() {
            Duration::from_millis(IDLE_ROUND_INTERVAL_MS)
        } else {
            Duration::from_millis(ACTIVE_ROUND_INTERVAL_MS)
        }
    }
}

impl Default for GossipScheduler {
    fn default() -> Self {
        Self::new()
    }
}

/// Public-facing scheduler alias used by integration tests.
/// Wraps `GossipScheduler` and exposes `get_interval` / `advance_time`.
pub struct RoundScheduler {
    inner: GossipScheduler,
    /// Accumulated virtual time offset for testing.
    time_offset: Duration,
}

impl RoundScheduler {
    pub fn new() -> Self {
        Self {
            inner: GossipScheduler::new(),
            time_offset: Duration::ZERO,
        }
    }

    pub fn record_activity(&mut self) {
        self.inner.record_activity();
    }

    /// Returns the current gossip round interval.
    pub fn get_interval(&self) -> Duration {
        // If a virtual time offset has pushed us past the idle threshold, report idle interval.
        if self.time_offset >= Duration::from_secs(IDLE_TIMEOUT_SEC) {
            Duration::from_millis(IDLE_ROUND_INTERVAL_MS)
        } else {
            self.inner.current_interval()
        }
    }

    /// Advance the virtual clock by `delta` (used in tests to simulate time passing).
    pub fn advance_time(&mut self, delta: Duration) {
        self.time_offset += delta;
    }
}

impl Default for RoundScheduler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scheduler_with_last_round_ago(ago: Duration) -> GossipScheduler {
        let mut s = GossipScheduler::new();
        s.last_round_time = Instant::now() - ago;
        s
    }

    fn scheduler_idle_for(idle_duration: Duration) -> GossipScheduler {
        let mut s = GossipScheduler::new();
        s.last_active_msg_time = Instant::now() - idle_duration;
        s
    }

    #[test]
    fn test_new_scheduler_is_not_idle() {
        let s = GossipScheduler::new();
        assert!(!s.is_idle());
    }

    #[test]
    fn test_becomes_idle_after_timeout() {
        let s = scheduler_idle_for(Duration::from_secs(IDLE_TIMEOUT_SEC + 1));
        assert!(s.is_idle());
    }

    #[test]
    fn test_not_idle_just_before_timeout() {
        let s = scheduler_idle_for(Duration::from_secs(IDLE_TIMEOUT_SEC - 1));
        assert!(!s.is_idle());
    }

    #[test]
    fn test_record_activity_resets_idle_timer() {
        let mut s = scheduler_idle_for(Duration::from_secs(IDLE_TIMEOUT_SEC + 5));
        assert!(s.is_idle());
        s.record_activity();
        assert!(!s.is_idle());
    }

    #[test]
    fn test_active_round_triggers_after_active_interval() {
        let s = scheduler_with_last_round_ago(Duration::from_millis(ACTIVE_ROUND_INTERVAL_MS + 10));
        assert!(s.is_time_for_round());
    }

    #[test]
    fn test_active_round_does_not_trigger_too_early() {
        let s = scheduler_with_last_round_ago(Duration::from_millis(100));
        assert!(!s.is_time_for_round());
    }

    #[test]
    fn test_idle_round_triggers_after_idle_interval() {
        let mut s = scheduler_idle_for(Duration::from_secs(IDLE_TIMEOUT_SEC + 1));
        s.last_round_time = Instant::now() - Duration::from_millis(IDLE_ROUND_INTERVAL_MS + 10);
        assert!(s.is_time_for_round());
    }

    #[test]
    fn test_idle_round_does_not_trigger_too_early() {
        let mut s = scheduler_idle_for(Duration::from_secs(IDLE_TIMEOUT_SEC + 1));
        s.last_round_time = Instant::now() - Duration::from_secs(1);
        assert!(!s.is_time_for_round());
    }

    #[test]
    fn test_round_executed_resets_round_timer() {
        let mut s =
            scheduler_with_last_round_ago(Duration::from_millis(ACTIVE_ROUND_INTERVAL_MS + 50));
        assert!(s.is_time_for_round());
        s.round_executed();
        assert!(!s.is_time_for_round());
    }

    #[test]
    fn test_current_interval_active() {
        let s = GossipScheduler::new();
        assert_eq!(
            s.current_interval(),
            Duration::from_millis(ACTIVE_ROUND_INTERVAL_MS)
        );
    }

    #[test]
    fn test_current_interval_idle() {
        let s = scheduler_idle_for(Duration::from_secs(IDLE_TIMEOUT_SEC + 1));
        assert_eq!(
            s.current_interval(),
            Duration::from_millis(IDLE_ROUND_INTERVAL_MS)
        );
    }

    #[test]
    fn test_adaptive_transition_active_to_idle_to_active() {
        let mut s = GossipScheduler::new();
        assert!(!s.is_idle());
        s.last_active_msg_time = Instant::now() - Duration::from_secs(IDLE_TIMEOUT_SEC + 1);
        assert!(s.is_idle());
        s.record_activity();
        assert!(!s.is_idle());
    }
}
