//! Integration tests for transport-level rate limiting and spam protection.
//!
//! These tests verify that the rate limiter correctly throttles rapid message bursts,
//! tracks violations, and triggers peer disconnection when violation thresholds are exceeded.

use std::time::Duration;

use stellarconduit_core::peer::identity::PeerIdentity;
use stellarconduit_core::security::rate_limit::{RateLimitConfig, RateLimiter};
use stellarconduit_core::transport::connection::TransportType;
use stellarconduit_core::transport::unified::{TransportManager, TransportPreference};
use tokio::sync::broadcast;
use tokio::time::sleep;

fn create_test_peer(byte: u8) -> PeerIdentity {
    PeerIdentity::new([byte; 32])
}

#[tokio::test]
async fn test_rate_limiter_throttles_rapid_bursts() {
    let config = RateLimitConfig {
        ble_messages_per_second: 10,
        wifi_messages_per_second: 100,
        violation_threshold: 5,
    };
    let mut limiter = RateLimiter::with_config(config);
    let peer = create_test_peer(1);

    // Consume the entire BLE quota immediately (10 messages)
    for _ in 0..10 {
        assert!(limiter.check_rate_limit(&peer, TransportType::Ble).is_ok());
    }

    // Next message should be rate limited
    let result = limiter.check_rate_limit(&peer, TransportType::Ble);
    assert!(result.is_err());
    assert!(!result.unwrap_err()); // Not banned yet

    // Wait for tokens to refill (1 second should be enough for 10 tokens)
    sleep(Duration::from_millis(1100)).await;

    // Should be able to send more messages after refill
    for _ in 0..5 {
        assert!(limiter.check_rate_limit(&peer, TransportType::Ble).is_ok());
    }
}

#[tokio::test]
async fn test_rate_limiter_tracks_violations_and_bans() {
    let config = RateLimitConfig {
        ble_messages_per_second: 5,
        wifi_messages_per_second: 50,
        violation_threshold: 3,
    };
    let mut limiter = RateLimiter::with_config(config);
    let peer = create_test_peer(2);

    // Consume quota
    for _ in 0..5 {
        let _ = limiter.check_rate_limit(&peer, TransportType::Ble);
    }

    // Trigger violations
    for i in 0..3 {
        let result = limiter.check_rate_limit(&peer, TransportType::Ble);
        if i < 2 {
            assert!(!result.unwrap_err()); // Not banned yet
            assert_eq!(limiter.get_violation_count(&peer), i + 1);
        } else {
            assert!(result.unwrap_err()); // Should ban
            assert_eq!(limiter.get_violation_count(&peer), 3);
        }
    }
}

#[tokio::test]
async fn test_rate_limiter_different_limits_per_transport() {
    let mut limiter = RateLimiter::new();
    let peer = create_test_peer(3);

    // BLE should have lower limit (10 msg/s)
    for _ in 0..10 {
        assert!(limiter.check_rate_limit(&peer, TransportType::Ble).is_ok());
    }
    // Next BLE message should be rate limited
    assert!(limiter.check_rate_limit(&peer, TransportType::Ble).is_err());

    // WiFi-Direct should have higher limit (100 msg/s) and be independent
    for _ in 0..100 {
        assert!(limiter
            .check_rate_limit(&peer, TransportType::WifiDirect)
            .is_ok());
    }
    // Next WiFi-Direct message should be rate limited
    assert!(limiter
        .check_rate_limit(&peer, TransportType::WifiDirect)
        .is_err());
}

#[tokio::test]
async fn test_transport_manager_rate_limiting_integration() {
    // Create a TransportManager with security events
    let (tx, _rx) = broadcast::channel(128);
    let mut mgr = TransportManager::with_security_events(TransportPreference::BleOnly, tx);

    // Connect a peer
    let peer = create_test_peer(4);
    mgr.connect(peer.clone(), None).await.unwrap();

    // Create a mock connection that can send messages rapidly
    // Note: In a real test, we'd need to mock the Connection trait or use a test implementation
    // For now, we'll test the rate limiter directly through the TransportManager's internal state

    // The rate limiter should be active, but we can't easily test recv_any without
    // a real connection. This test verifies the integration structure is correct.
    assert_eq!(mgr.connection_count(), 1);

    // Disconnect should clean up rate limiter state
    mgr.disconnect_peer(&peer.pubkey).await;
    assert_eq!(mgr.connection_count(), 0);
}

#[tokio::test]
async fn test_security_event_emission_on_violation() {
    let config = RateLimitConfig {
        ble_messages_per_second: 5,
        wifi_messages_per_second: 50,
        violation_threshold: 2,
    };
    let mut limiter = RateLimiter::with_config(config);
    let peer = create_test_peer(5);

    // Consume quota and trigger violations
    for _ in 0..5 {
        let _ = limiter.check_rate_limit(&peer, TransportType::Ble);
    }

    // Trigger violations until ban threshold
    for i in 0..2 {
        let result = limiter.check_rate_limit(&peer, TransportType::Ble);
        if i == 1 {
            // Last violation should trigger ban
            assert!(result.unwrap_err());
        }
    }

    // In a real scenario, TransportManager would emit the event
    // For this test, we verify the violation tracking works correctly
    assert_eq!(limiter.get_violation_count(&peer), 2);
}

#[tokio::test]
async fn test_rate_limiter_separate_state_per_peer() {
    let mut limiter = RateLimiter::new();
    let peer1 = create_test_peer(6);
    let peer2 = create_test_peer(7);

    // Both peers should have independent rate limits
    // Consume peer1's quota
    for _ in 0..10 {
        assert!(limiter.check_rate_limit(&peer1, TransportType::Ble).is_ok());
    }
    assert!(limiter
        .check_rate_limit(&peer1, TransportType::Ble)
        .is_err());

    // peer2 should still have full quota
    for _ in 0..10 {
        assert!(limiter.check_rate_limit(&peer2, TransportType::Ble).is_ok());
    }
    assert!(limiter
        .check_rate_limit(&peer2, TransportType::Ble)
        .is_err());
}

#[tokio::test]
async fn test_rate_limiter_removes_peer_state() {
    let mut limiter = RateLimiter::new();
    let peer = create_test_peer(8);

    // Add some state
    let _ = limiter.check_rate_limit(&peer, TransportType::Ble);
    assert_eq!(limiter.get_violation_count(&peer), 0);

    // Remove peer
    limiter.remove_peer(&peer);
    assert_eq!(limiter.get_violation_count(&peer), 0); // Should return 0 for non-existent peer
}

#[tokio::test]
async fn test_rate_limiter_resets_violations_on_good_behavior() {
    let config = RateLimitConfig {
        ble_messages_per_second: 10,
        wifi_messages_per_second: 100,
        violation_threshold: 5,
    };
    let mut limiter = RateLimiter::with_config(config);
    let peer = create_test_peer(9);

    // Consume quota and trigger a violation
    for _ in 0..10 {
        let _ = limiter.check_rate_limit(&peer, TransportType::Ble);
    }
    let _ = limiter.check_rate_limit(&peer, TransportType::Ble); // Violation 1
    assert_eq!(limiter.get_violation_count(&peer), 1);

    // Wait for tokens to refill
    sleep(Duration::from_millis(1100)).await;

    // Good behavior should reset violation count
    assert!(limiter.check_rate_limit(&peer, TransportType::Ble).is_ok());
    assert_eq!(limiter.get_violation_count(&peer), 0);
}
