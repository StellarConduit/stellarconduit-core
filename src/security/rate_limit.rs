//! Per-peer message rate limiter with observability counters.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::metrics::Metrics;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

struct PeerBucket {
    count: usize,
    window_start_ms: u64,
}

/// Sliding-window rate limiter keyed by peer pubkey.
///
/// Each peer may send at most `capacity` messages per `window_ms` milliseconds.
/// Excess messages increment `messages_rate_limited` and `peer_violations_recorded`
/// on the shared `Metrics` instance.
pub struct RateLimiter {
    capacity: usize,
    window_ms: u64,
    buckets: HashMap<[u8; 32], PeerBucket>,
    metrics: Arc<Metrics>,
}

impl RateLimiter {
    /// `capacity` – max allowed messages per peer per window.
    /// `window_ms` – sliding window length in milliseconds.
    pub fn new(capacity: usize, window_ms: u64, metrics: Arc<Metrics>) -> Self {
        Self {
            capacity,
            window_ms,
            buckets: HashMap::new(),
            metrics,
        }
    }

    /// Returns `true` if the message is allowed, `false` if it is rate-limited.
    /// Increments `messages_rate_limited` and `peer_violations_recorded` on rejection.
    pub fn check_and_record(&mut self, peer: &[u8; 32]) -> bool {
        let now = now_ms();
        let bucket = self.buckets.entry(*peer).or_insert(PeerBucket {
            count: 0,
            window_start_ms: now,
        });

        // Reset window if it has expired.
        if now.saturating_sub(bucket.window_start_ms) >= self.window_ms {
            bucket.count = 0;
            bucket.window_start_ms = now;
        }

        if bucket.count >= self.capacity {
            self.metrics
                .messages_rate_limited
                .fetch_add(1, Ordering::Relaxed);
            self.metrics
                .peer_violations_recorded
                .fetch_add(1, Ordering::Relaxed);
            false
        } else {
            bucket.count += 1;
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn peer(b: u8) -> [u8; 32] {
        [b; 32]
    }

    #[test]
    fn test_within_capacity_is_allowed() {
        let metrics = Metrics::new();
        let mut limiter = RateLimiter::new(5, 60_000, metrics.clone());
        for _ in 0..5 {
            assert!(limiter.check_and_record(&peer(0x01)));
        }
        assert_eq!(metrics.messages_rate_limited.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_metrics_rate_limit_counter() {
        let metrics = Metrics::new();
        let mut limiter = RateLimiter::new(2, 60_000, metrics.clone());
        let p = peer(0x02);
        assert!(limiter.check_and_record(&p)); // 1st: allowed
        assert!(limiter.check_and_record(&p)); // 2nd: allowed (at capacity)
        assert!(!limiter.check_and_record(&p)); // 3rd: rate limited
        assert!(!limiter.check_and_record(&p)); // 4th: rate limited
        assert!(!limiter.check_and_record(&p)); // 5th: rate limited
        assert_eq!(metrics.messages_rate_limited.load(Ordering::Relaxed), 3);
        assert_eq!(metrics.peer_violations_recorded.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn test_different_peers_have_independent_buckets() {
        let metrics = Metrics::new();
        let mut limiter = RateLimiter::new(1, 60_000, metrics.clone());
        assert!(limiter.check_and_record(&peer(0xAA)));
        assert!(limiter.check_and_record(&peer(0xBB))); // different peer, fresh bucket
        assert_eq!(metrics.messages_rate_limited.load(Ordering::Relaxed), 0);

        // Now both peers exceed capacity
        assert!(!limiter.check_and_record(&peer(0xAA)));
        assert!(!limiter.check_and_record(&peer(0xBB)));
        assert_eq!(metrics.messages_rate_limited.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn test_zero_capacity_limits_all() {
        let metrics = Metrics::new();
        let mut limiter = RateLimiter::new(0, 60_000, metrics.clone());
        for _ in 0..3 {
            assert!(!limiter.check_and_record(&peer(0x01)));
        }
        assert_eq!(metrics.messages_rate_limited.load(Ordering::Relaxed), 3);
    }
}
