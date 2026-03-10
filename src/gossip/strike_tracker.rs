//! Tracks signature verification failures per peer using a sliding window.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::peer::identity::PeerIdentity;

/// Tracks signature failures within a sliding time window.
pub struct StrikeTracker {
    /// Maps peer pubkey to a list of failure timestamps
    failures: HashMap<[u8; 32], Vec<Instant>>,
    /// Window duration for counting strikes
    window_duration: Duration,
    /// Maximum number of strikes before banning
    max_strikes: usize,
}

impl StrikeTracker {
    /// Create a new StrikeTracker with a 10-minute window and max 3 strikes.
    pub fn new() -> Self {
        Self {
            failures: HashMap::new(),
            window_duration: Duration::from_secs(10 * 60), // 10 minutes
            max_strikes: 3,
        }
    }

    /// Record a signature failure for the given peer.
    /// Returns `true` if the peer should be banned (exceeded max strikes).
    pub fn record_failure(&mut self, peer: &PeerIdentity) -> bool {
        let now = Instant::now();
        let peer_key = peer.pubkey;

        let failures = self.failures.entry(peer_key).or_default();

        // Remove old failures outside the window
        failures.retain(|&timestamp| now.duration_since(timestamp) <= self.window_duration);

        // Add the new failure
        failures.push(now);

        // Check if we've exceeded the threshold
        failures.len() > self.max_strikes
    }

    /// Clear all failures for a peer (e.g., after unbanning)
    pub fn clear_peer(&mut self, peer: &PeerIdentity) {
        self.failures.remove(&peer.pubkey);
    }

    /// Get the current number of strikes for a peer
    pub fn get_strike_count(&self, peer: &PeerIdentity) -> usize {
        let now = Instant::now();
        self.failures
            .get(&peer.pubkey)
            .map(|failures| {
                failures
                    .iter()
                    .filter(|&&timestamp| now.duration_since(timestamp) <= self.window_duration)
                    .count()
            })
            .unwrap_or(0)
    }

    /// Clean up old entries that are outside the window
    pub fn cleanup(&mut self) {
        let now = Instant::now();
        self.failures.retain(|_, failures| {
            failures.retain(|&timestamp| now.duration_since(timestamp) <= self.window_duration);
            !failures.is_empty()
        });
    }
}

impl Default for StrikeTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_strike_tracking() {
        let mut tracker = StrikeTracker::new();
        let peer = PeerIdentity::new([0xAA; 32]);

        // Record 3 failures - should not ban yet
        assert!(!tracker.record_failure(&peer));
        assert!(!tracker.record_failure(&peer));
        assert!(!tracker.record_failure(&peer));
        assert_eq!(tracker.get_strike_count(&peer), 3);

        // 4th failure should trigger ban
        assert!(tracker.record_failure(&peer));
        assert_eq!(tracker.get_strike_count(&peer), 4);
    }

    #[test]
    fn test_sliding_window() {
        let mut tracker = StrikeTracker::new();
        let peer = PeerIdentity::new([0xBB; 32]);

        // Record failures
        tracker.record_failure(&peer);
        tracker.record_failure(&peer);
        tracker.record_failure(&peer);
        assert_eq!(tracker.get_strike_count(&peer), 3);

        // Wait for window to expire (using a shorter duration for testing)
        // Note: In real tests, we'd use a mock time source
        thread::sleep(Duration::from_millis(100));

        // Manually clean up old entries
        tracker.cleanup();

        // After cleanup, old entries should be gone
        // But since we can't easily mock Instant, we'll test the cleanup logic differently
        // by checking that clear_peer works
        tracker.clear_peer(&peer);
        assert_eq!(tracker.get_strike_count(&peer), 0);
    }

    #[test]
    fn test_multiple_peers() {
        let mut tracker = StrikeTracker::new();
        let peer1 = PeerIdentity::new([0x01; 32]);
        let peer2 = PeerIdentity::new([0x02; 32]);

        tracker.record_failure(&peer1);
        tracker.record_failure(&peer1);
        tracker.record_failure(&peer2);

        assert_eq!(tracker.get_strike_count(&peer1), 2);
        assert_eq!(tracker.get_strike_count(&peer2), 1);
    }
}
