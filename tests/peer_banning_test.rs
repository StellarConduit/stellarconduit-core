//! Unit tests for peer banning and signature failure tracking.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use ed25519_dalek::SigningKey;
use stellarconduit_core::discovery::peer_list::PeerList;
use stellarconduit_core::gossip::protocol::process_transaction_envelope;
use stellarconduit_core::gossip::strike_tracker::StrikeTracker;
use stellarconduit_core::message::signing::sign_envelope;
use stellarconduit_core::message::types::TransactionEnvelope;
use stellarconduit_core::peer::identity::PeerIdentity;
use stellarconduit_core::transport::unified::{TransportManager, TransportPreference};

fn create_valid_envelope(keypair: &SigningKey, tx_xdr: &str) -> TransactionEnvelope {
    let mut envelope = TransactionEnvelope {
        message_id: [0xAA; 32],
        origin_pubkey: keypair.verifying_key().to_bytes(),
        tx_xdr: tx_xdr.to_string(),
        ttl_hops: 10,
        timestamp: 1672531200,
        signature: [0u8; 64],
    };
    sign_envelope(keypair, &mut envelope).unwrap();
    envelope
}

fn create_invalid_envelope(keypair: &SigningKey, tx_xdr: &str) -> TransactionEnvelope {
    let mut envelope = create_valid_envelope(keypair, tx_xdr);
    // Corrupt the signature
    envelope.signature[0] ^= 0xFF;
    envelope
}

#[tokio::test]
async fn test_strike_tracking_banning() {
    let mut strike_tracker = StrikeTracker::new();
    let peer_list = Arc::new(Mutex::new(PeerList::new(300)));
    let transport_manager = Arc::new(Mutex::new(TransportManager::new(
        TransportPreference::BleOnly,
    )));

    // Create a keypair for the peer
    let keypair = SigningKey::from_bytes(&[0x42; 32]);

    // Create invalid envelopes
    let invalid_env1 = create_invalid_envelope(&keypair, "invalid1");
    let invalid_env2 = create_invalid_envelope(&keypair, "invalid2");
    let invalid_env3 = create_invalid_envelope(&keypair, "invalid3");
    let invalid_env4 = create_invalid_envelope(&keypair, "invalid4");

    let peer_identity = PeerIdentity::new(keypair.verifying_key().to_bytes());

    // First 3 failures should not ban
    let result1 = process_transaction_envelope(
        &invalid_env1,
        &mut strike_tracker,
        peer_list.clone(),
        transport_manager.clone(),
    )
    .await;
    assert!(result1.is_ok());
    assert_eq!(strike_tracker.get_strike_count(&peer_identity), 1);

    let result2 = process_transaction_envelope(
        &invalid_env2,
        &mut strike_tracker,
        peer_list.clone(),
        transport_manager.clone(),
    )
    .await;
    assert!(result2.is_ok());
    assert_eq!(strike_tracker.get_strike_count(&peer_identity), 2);

    let result3 = process_transaction_envelope(
        &invalid_env3,
        &mut strike_tracker,
        peer_list.clone(),
        transport_manager.clone(),
    )
    .await;
    assert!(result3.is_ok());
    assert_eq!(strike_tracker.get_strike_count(&peer_identity), 3);

    // 4th failure should ban
    let result4 = process_transaction_envelope(
        &invalid_env4,
        &mut strike_tracker,
        peer_list.clone(),
        transport_manager.clone(),
    )
    .await;
    assert!(result4.is_err()); // Should return Err with peer identity
    assert_eq!(strike_tracker.get_strike_count(&peer_identity), 4);

    // Verify peer is banned
    let peer_list_guard = peer_list.lock().await;
    assert!(peer_list_guard.is_peer_banned(&peer_identity.pubkey));
    drop(peer_list_guard);
}

#[tokio::test]
async fn test_valid_signature_clears_strikes() {
    let mut strike_tracker = StrikeTracker::new();
    let peer_list = Arc::new(Mutex::new(PeerList::new(300)));
    let transport_manager = Arc::new(Mutex::new(TransportManager::new(
        TransportPreference::BleOnly,
    )));

    let keypair = SigningKey::from_bytes(&[0x43; 32]);
    let peer_identity = PeerIdentity::new(keypair.verifying_key().to_bytes());

    // Record 2 failures
    let invalid_env1 = create_invalid_envelope(&keypair, "invalid1");
    let invalid_env2 = create_invalid_envelope(&keypair, "invalid2");

    process_transaction_envelope(
        &invalid_env1,
        &mut strike_tracker,
        peer_list.clone(),
        transport_manager.clone(),
    )
    .await
    .unwrap();
    process_transaction_envelope(
        &invalid_env2,
        &mut strike_tracker,
        peer_list.clone(),
        transport_manager.clone(),
    )
    .await
    .unwrap();

    assert_eq!(strike_tracker.get_strike_count(&peer_identity), 2);

    // Valid signature should clear strikes
    let valid_env = create_valid_envelope(&keypair, "valid");
    process_transaction_envelope(
        &valid_env,
        &mut strike_tracker,
        peer_list.clone(),
        transport_manager.clone(),
    )
    .await
    .unwrap();

    assert_eq!(strike_tracker.get_strike_count(&peer_identity), 0);
}

#[tokio::test]
async fn test_ban_expiration() {
    let mut peer_list = PeerList::new(300);
    let keypair = SigningKey::from_bytes(&[0x44; 32]);
    let peer_identity = PeerIdentity::new(keypair.verifying_key().to_bytes());

    // Add peer to list
    peer_list.insert_or_update(peer_identity.pubkey, 100);

    // Ban for 1 second
    peer_list.ban_peer(&peer_identity.pubkey, 1);
    assert!(peer_list.is_peer_banned(&peer_identity.pubkey));

    // Wait for ban to expire
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Check expiration
    let unbanned = peer_list.check_ban_expirations();
    assert_eq!(unbanned.len(), 1);
    assert_eq!(unbanned[0].pubkey, peer_identity.pubkey);
    assert!(!peer_list.is_peer_banned(&peer_identity.pubkey));
}

#[tokio::test]
async fn test_multiple_peers_separate_strikes() {
    let mut strike_tracker = StrikeTracker::new();
    let peer_list = Arc::new(Mutex::new(PeerList::new(300)));
    let transport_manager = Arc::new(Mutex::new(TransportManager::new(
        TransportPreference::BleOnly,
    )));

    let keypair1 = SigningKey::from_bytes(&[0x01; 32]);
    let keypair2 = SigningKey::from_bytes(&[0x02; 32]);
    let peer1 = PeerIdentity::new(keypair1.verifying_key().to_bytes());
    let peer2 = PeerIdentity::new(keypair2.verifying_key().to_bytes());

    // Peer1 gets 4 failures (should be banned)
    for _ in 0..4 {
        let invalid = create_invalid_envelope(&keypair1, "invalid");
        process_transaction_envelope(
            &invalid,
            &mut strike_tracker,
            peer_list.clone(),
            transport_manager.clone(),
        )
        .await
        .ok();
    }

    // Peer2 gets 2 failures (should not be banned)
    for _ in 0..2 {
        let invalid = create_invalid_envelope(&keypair2, "invalid");
        process_transaction_envelope(
            &invalid,
            &mut strike_tracker,
            peer_list.clone(),
            transport_manager.clone(),
        )
        .await
        .ok();
    }

    assert_eq!(strike_tracker.get_strike_count(&peer1), 4);
    assert_eq!(strike_tracker.get_strike_count(&peer2), 2);

    let peer_list_guard = peer_list.lock().await;
    assert!(peer_list_guard.is_peer_banned(&peer1.pubkey));
    assert!(!peer_list_guard.is_peer_banned(&peer2.pubkey));
}

#[test]
fn test_strike_tracker_window() {
    let mut tracker = StrikeTracker::new();
    let peer = PeerIdentity::new([0xAA; 32]);

    // Record 3 failures
    assert!(!tracker.record_failure(&peer));
    assert!(!tracker.record_failure(&peer));
    assert!(!tracker.record_failure(&peer));
    assert_eq!(tracker.get_strike_count(&peer), 3);

    // 4th failure should trigger ban
    assert!(tracker.record_failure(&peer));
    assert_eq!(tracker.get_strike_count(&peer), 4);
}

#[test]
fn test_peer_ban_methods() {
    let mut peer = stellarconduit_core::peer::peer_node::Peer::new([0xBB; 32]);

    // Initially not banned
    assert!(!peer.is_banned);
    assert_eq!(peer.ban_expires_at_unix_sec, 0);

    // Ban for 3600 seconds (1 hour)
    peer.ban(3600);
    assert!(peer.is_banned);
    assert!(peer.ban_expires_at_unix_sec > 0);

    // Check expiration immediately should not unban
    assert!(!peer.check_ban_expiration());
    assert!(peer.is_banned);
}

#[tokio::test]
async fn test_peer_list_ban_methods() {
    let mut peer_list = PeerList::new(300);
    let pubkey = [0xCC; 32];

    // Add peer
    peer_list.insert_or_update(pubkey, 100);
    assert!(!peer_list.is_peer_banned(&pubkey));

    // Ban peer
    assert!(peer_list.ban_peer(&pubkey, 3600));
    assert!(peer_list.is_peer_banned(&pubkey));

    // Unban peer
    assert!(peer_list.unban_peer(&pubkey));
    assert!(!peer_list.is_peer_banned(&pubkey));
}
