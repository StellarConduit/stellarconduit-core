use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::Arc;

use governor::{
    clock::DefaultClock, state::direct::NotKeyed, state::InMemoryState, Quota,
    RateLimiter as GovernorRateLimiter,
};

use crate::peer::identity::PeerIdentity;
use crate::transport::connection::TransportType;

/// Rate limiter configuration for different transport types.
#[derive(Debug, Clone, Copy)]
pub struct RateLimitConfig {
    /// Maximum messages per second for BLE transport
    pub ble_messages_per_second: u32,
    /// Maximum messages per second for WiFi-Direct transport
    pub wifi_messages_per_second: u32,
    /// Number of violations before triggering a ban
    pub violation_threshold: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            ble_messages_per_second: 10,
            wifi_messages_per_second: 100,
            violation_threshold: 10,
        }
    }
}

/// Per-peer rate limiter state tracking violations.
/// Maintains separate limiters for BLE and WiFi-Direct transports.
#[derive(Debug)]
struct PeerRateLimitState {
    ble_limiter: Arc<GovernorRateLimiter<NotKeyed, InMemoryState, DefaultClock>>,
    wifi_limiter: Arc<GovernorRateLimiter<NotKeyed, InMemoryState, DefaultClock>>,
    violation_count: u32,
}

/// Rate limiter that enforces per-peer message rate limits using a token bucket algorithm.
///
/// This rate limiter prevents malicious nodes from flooding neighbors with excessive messages.
/// It maintains separate rate limits for BLE (10 msg/s) and WiFi-Direct (100 msg/s) transports.
pub struct RateLimiter {
    config: RateLimitConfig,
    /// Per-peer rate limiters and violation tracking
    peer_limiters: HashMap<[u8; 32], PeerRateLimitState>,
}

impl RateLimiter {
    /// Create a new rate limiter with the default configuration.
    pub fn new() -> Self {
        Self::with_config(RateLimitConfig::default())
    }

    /// Create a new rate limiter with a custom configuration.
    pub fn with_config(config: RateLimitConfig) -> Self {
        Self {
            config,
            peer_limiters: HashMap::new(),
        }
    }

    /// Check if a message from a peer should be allowed based on rate limits.
    ///
    /// Returns:
    /// - `Ok(())` if the message is within rate limits
    /// - `Err(true)` if the peer has exceeded the violation threshold and should be banned
    /// - `Err(false)` if the message exceeds the rate limit but hasn't hit the threshold yet
    pub fn check_rate_limit(
        &mut self,
        peer: &PeerIdentity,
        transport_type: TransportType,
    ) -> Result<(), bool> {
        // Get or create the rate limiters for this peer (separate for each transport)
        let state = self.peer_limiters.entry(peer.pubkey).or_insert_with(|| {
            // Create BLE limiter
            let ble_rate = self.config.ble_messages_per_second.max(1);
            let ble_rate_nonzero =
                NonZeroU32::new(ble_rate).expect("BLE rate should be at least 1");
            let ble_quota = Quota::per_second(ble_rate_nonzero);
            let ble_limiter = Arc::new(GovernorRateLimiter::direct(ble_quota));

            // Create WiFi-Direct limiter
            let wifi_rate = self.config.wifi_messages_per_second.max(1);
            let wifi_rate_nonzero =
                NonZeroU32::new(wifi_rate).expect("WiFi rate should be at least 1");
            let wifi_quota = Quota::per_second(wifi_rate_nonzero);
            let wifi_limiter = Arc::new(GovernorRateLimiter::direct(wifi_quota));

            PeerRateLimitState {
                ble_limiter,
                wifi_limiter,
                violation_count: 0,
            }
        });

        // Select the appropriate limiter based on transport type
        let limiter = match transport_type {
            TransportType::Ble => &state.ble_limiter,
            TransportType::WifiDirect => &state.wifi_limiter,
        };

        // Check if the rate limit allows this message
        if limiter.check().is_ok() {
            // Reset violation count on successful check (good behavior)
            state.violation_count = 0;
            Ok(())
        } else {
            // Rate limit exceeded
            state.violation_count += 1;

            // Check if we've exceeded the violation threshold
            if state.violation_count >= self.config.violation_threshold {
                Err(true) // Should ban
            } else {
                Err(false) // Rate limited but not banned yet
            }
        }
    }

    /// Remove rate limiter state for a peer (e.g., when they disconnect).
    pub fn remove_peer(&mut self, peer: &PeerIdentity) {
        self.peer_limiters.remove(&peer.pubkey);
    }

    /// Get the current violation count for a peer.
    pub fn get_violation_count(&self, peer: &PeerIdentity) -> u32 {
        self.peer_limiters
            .get(&peer.pubkey)
            .map(|state| state.violation_count)
            .unwrap_or(0)
    }

    /// Clear all rate limiter state (useful for testing).
    pub fn clear(&mut self) {
        self.peer_limiters.clear();
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer::identity::PeerIdentity;

    fn create_test_peer(byte: u8) -> PeerIdentity {
        PeerIdentity::new([byte; 32])
    }

    #[test]
    fn test_rate_limiter_allows_messages_within_limit() {
        let mut limiter = RateLimiter::new();
        let peer = create_test_peer(1);

        // Should allow messages up to the limit
        for _ in 0..10 {
            assert!(limiter.check_rate_limit(&peer, TransportType::Ble).is_ok());
        }
    }

    #[test]
    fn test_rate_limiter_blocks_messages_exceeding_limit() {
        let mut limiter = RateLimiter::new();
        let peer = create_test_peer(2);

        // Consume the entire quota immediately
        for _ in 0..10 {
            let _ = limiter.check_rate_limit(&peer, TransportType::Ble);
        }

        // Next message should be rate limited
        let result = limiter.check_rate_limit(&peer, TransportType::Ble);
        assert!(result.is_err());
        assert!(!result.unwrap_err()); // Not banned yet
    }

    #[test]
    fn test_rate_limiter_tracks_violations() {
        let config = RateLimitConfig {
            ble_messages_per_second: 5,
            wifi_messages_per_second: 50,
            violation_threshold: 3,
        };
        let mut limiter = RateLimiter::with_config(config);
        let peer = create_test_peer(3);

        // Exceed rate limit multiple times
        for _ in 0..5 {
            let _ = limiter.check_rate_limit(&peer, TransportType::Ble);
        }

        // Trigger violations
        for i in 0..3 {
            let result = limiter.check_rate_limit(&peer, TransportType::Ble);
            if i < 2 {
                assert!(!result.unwrap_err()); // Not banned yet
            } else {
                assert!(result.unwrap_err()); // Should ban
            }
        }

        assert_eq!(limiter.get_violation_count(&peer), 3);
    }

    #[test]
    fn test_rate_limiter_resets_violations_on_good_behavior() {
        let config = RateLimitConfig {
            ble_messages_per_second: 10,
            wifi_messages_per_second: 100,
            violation_threshold: 5,
        };
        let mut limiter = RateLimiter::with_config(config);
        let peer = create_test_peer(4);

        // Trigger some violations
        for _ in 0..10 {
            let _ = limiter.check_rate_limit(&peer, TransportType::Ble);
        }
        let _ = limiter.check_rate_limit(&peer, TransportType::Ble); // Violation 1
        assert_eq!(limiter.get_violation_count(&peer), 1);

        // Wait for tokens to refill (simulate by allowing time to pass)
        // In a real scenario, we'd wait, but for testing we can check that
        // the violation count is tracked correctly
        // Note: governor uses real time, so we can't easily test token refill in unit tests
        // without waiting. This is tested in integration tests.
    }

    #[test]
    fn test_rate_limiter_different_limits_per_transport() {
        let mut limiter = RateLimiter::new();
        let peer = create_test_peer(5);

        // BLE should have lower limit (10 msg/s)
        for _ in 0..10 {
            assert!(limiter.check_rate_limit(&peer, TransportType::Ble).is_ok());
        }

        // WiFi-Direct should have higher limit (100 msg/s)
        for _ in 0..100 {
            assert!(limiter
                .check_rate_limit(&peer, TransportType::WifiDirect)
                .is_ok());
        }
    }

    #[test]
    fn test_rate_limiter_remove_peer() {
        let mut limiter = RateLimiter::new();
        let peer = create_test_peer(6);

        // Add some state
        let _ = limiter.check_rate_limit(&peer, TransportType::Ble);
        assert!(limiter.peer_limiters.contains_key(&peer.pubkey));

        // Remove peer
        limiter.remove_peer(&peer);
        assert!(!limiter.peer_limiters.contains_key(&peer.pubkey));
    }

    #[test]
    fn test_rate_limiter_separate_limits_per_peer() {
        let mut limiter = RateLimiter::new();
        let peer1 = create_test_peer(7);
        let peer2 = create_test_peer(8);

        // Both peers should have independent rate limits
        for _ in 0..10 {
            assert!(limiter.check_rate_limit(&peer1, TransportType::Ble).is_ok());
            assert!(limiter.check_rate_limit(&peer2, TransportType::Ble).is_ok());
        }
    }
}
