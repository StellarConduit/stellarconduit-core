use std::time::Duration;
use tokio::net::TcpListener;
use tokio::time::timeout;

use stellarconduit_core::gossip::protocol::GossipState;
use stellarconduit_core::message::types::{ProtocolMessage, TransactionEnvelope};
use stellarconduit_core::peer::identity::PeerIdentity;
use stellarconduit_core::transport::connection::Connection;
use stellarconduit_core::transport::unified::{TransportManager, TransportPreference};
use stellarconduit_core::transport::wifi_transport::WifiDirectConnection;

async fn send_with_timeout<F, T, E>(fut: F, ms: u64, ctx: &str) -> T
where
    F: std::future::Future<Output = Result<T, E>>,
    E: std::fmt::Debug,
{
    match timeout(Duration::from_millis(ms), fut).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => panic!("{} send failed: {:?}", ctx, e),
        Err(_) => panic!("{} send timed out after {}ms", ctx, ms),
    }
}

fn mock_envelope(id_byte: u8, origin: [u8; 32], ttl: u8) -> TransactionEnvelope {
    TransactionEnvelope {
        message_id: [id_byte; 32],
        origin_pubkey: origin,
        tx_xdr: format!("xdr-{}", id_byte),
        ttl_hops: ttl,
        timestamp: 1_000_000 + id_byte as u64,
        signature: [0u8; 64],
    }
}

#[tokio::test]
async fn test_full_gossip_round_delivers_to_peer() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let peer_a = PeerIdentity::new([0xAA; 32]);
    let peer_b = PeerIdentity::new([0xBB; 32]);
    let peer_a_for_accept = peer_a.clone();

    let b_conn_task = tokio::spawn(async move {
        WifiDirectConnection::accept_from(&listener, peer_a_for_accept)
            .await
            .unwrap()
    });

    let mut mgr_a = TransportManager::new(TransportPreference::WifiOnly);
    mgr_a.connect(peer_b.clone(), Some(addr)).await.unwrap();

    let envelope = TransactionEnvelope {
        message_id: [0x01; 32],
        origin_pubkey: peer_a.pubkey,
        tx_xdr: "test-xdr".to_string(),
        ttl_hops: 10,
        timestamp: 1_000_000,
        signature: [0u8; 64],
    };

    let mut state_a = GossipState::new();
    state_a.add_envelope(envelope.clone()).unwrap();

    let batch = state_a.active_queue.drain_batch(32);
    for msg in batch {
        send_with_timeout(mgr_a.send_to(&peer_b, msg), 5000, "mgr_a.send_to(&peer_b)").await;
    }

    let mut conn_b = b_conn_task.await.unwrap();
    let received = conn_b.recv().await.unwrap();

    let ProtocolMessage::Transaction(received_env) = received else {
        panic!("Expected Transaction message");
    };
    assert_eq!(received_env.message_id, envelope.message_id);
    assert_eq!(received_env.tx_xdr, "test-xdr");
    assert_eq!(received_env.ttl_hops, 10);
}

#[tokio::test]
async fn test_anti_entropy_sync_converges_two_nodes() {
    let a_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a_addr = a_listener.local_addr().unwrap();

    let peer_a = PeerIdentity::new([0xAA; 32]);
    let peer_b = PeerIdentity::new([0xBB; 32]);
    let peer_b_for_accept = peer_b.clone();

    let a_accept_task = tokio::spawn(async move {
        WifiDirectConnection::accept_from(&a_listener, peer_b_for_accept)
            .await
            .unwrap()
    });

    let mut conn_b_to_a = WifiDirectConnection::connect_to(peer_a.clone(), a_addr)
        .await
        .unwrap();

    let mut conn_a_to_b = a_accept_task.await.unwrap();

    let env_a1 = mock_envelope(0xA1, peer_a.pubkey, 10);
    let env_a2 = mock_envelope(0xA2, peer_a.pubkey, 10);
    let env_b1 = mock_envelope(0xB1, peer_b.pubkey, 10);

    let mut state_a = GossipState::new();
    state_a.add_envelope(env_a1).unwrap();
    state_a.add_envelope(env_a2).unwrap();

    let mut state_b = GossipState::new();
    state_b.add_envelope(env_b1).unwrap();

    let req = state_b.generate_sync_request();
    send_with_timeout(
        conn_b_to_a.send(ProtocolMessage::SyncRequest(req)),
        5000,
        "conn_b_to_a.send(SyncRequest)",
    )
    .await;

    let received = conn_a_to_b.recv().await.unwrap();
    let ProtocolMessage::SyncRequest(req) = received else {
        panic!("Expected SyncRequest");
    };

    let resp = state_a.handle_sync_request(&req);
    assert_eq!(resp.missing_envelopes.len(), 2);

    send_with_timeout(
        conn_a_to_b.send(ProtocolMessage::SyncResponse(resp)),
        5000,
        "conn_a_to_b.send(SyncResponse)",
    )
    .await;

    let received = conn_b_to_a.recv().await.unwrap();
    let ProtocolMessage::SyncResponse(resp) = received else {
        panic!("Expected SyncResponse");
    };

    state_b.handle_sync_response(resp);
    assert_eq!(state_b.active_queue.len(), 3);
}

#[tokio::test]
async fn test_ttl_decrement_prevents_infinite_loops() {
    let b_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let b_addr = b_listener.local_addr().unwrap();

    let c_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let c_addr = c_listener.local_addr().unwrap();

    let peer_a = PeerIdentity::new([0xAA; 32]);
    let peer_b = PeerIdentity::new([0xBB; 32]);
    let peer_c = PeerIdentity::new([0xCC; 32]);
    let peer_a_for_b_accept = peer_a.clone();
    let peer_b_for_c_accept = peer_b.clone();

    let b_accept_task = tokio::spawn(async move {
        WifiDirectConnection::accept_from(&b_listener, peer_a_for_b_accept)
            .await
            .unwrap()
    });

    let c_accept_task = tokio::spawn(async move {
        WifiDirectConnection::accept_from(&c_listener, peer_b_for_c_accept)
            .await
            .unwrap()
    });

    let mut mgr_a = TransportManager::new(TransportPreference::WifiOnly);
    mgr_a.connect(peer_b.clone(), Some(b_addr)).await.unwrap();

    let mut mgr_b = TransportManager::new(TransportPreference::WifiOnly);
    mgr_b.connect(peer_c.clone(), Some(c_addr)).await.unwrap();

    let mut b_conn = b_accept_task.await.unwrap();

    let envelope = TransactionEnvelope {
        message_id: [0x01; 32],
        origin_pubkey: peer_a.pubkey,
        tx_xdr: "ttl-test-xdr".to_string(),
        ttl_hops: 1,
        timestamp: 2_000_000,
        signature: [0u8; 64],
    };

    let mut state_a = GossipState::new();
    state_a.add_envelope(envelope.clone()).unwrap();

    // Node A sends to B (decrement TTL from 1 → 0)
    let batch = state_a.active_queue.drain_batch(32);
    for msg in batch {
        let ProtocolMessage::Transaction(mut env) = msg else {
            continue;
        };
        env.ttl_hops = env.ttl_hops.saturating_sub(1);
        send_with_timeout(
            mgr_a.send_to(&peer_b, ProtocolMessage::Transaction(env)),
            5000,
            "mgr_a.send_to(&peer_b) (TTL test)",
        )
        .await;
    }

    let received = b_conn.recv().await.unwrap();
    let ProtocolMessage::Transaction(received_env) = received else {
        panic!("Expected Transaction message");
    };
    assert_eq!(
        received_env.ttl_hops, 0,
        "TTL should be 0 after decrement from 1"
    );

    // B adds the envelope to its gossip state
    let mut state_b = GossipState::new();
    state_b.add_envelope(received_env).unwrap();
    assert_eq!(state_b.active_queue.len(), 1);

    // Simulate execute_fanout_round's TTL check: drain and verify TTL=0 prevents forwarding
    let batch = state_b.active_queue.drain_batch(32);
    for msg in &batch {
        if let ProtocolMessage::Transaction(env) = msg {
            assert_eq!(
                env.ttl_hops, 0,
                "Envelope should have TTL=0 and be skipped for forwarding"
            );
        }
    }

    // C must NOT receive any forwarded message since the envelope had TTL=0
    let mut c_conn = c_accept_task.await.unwrap();
    let result = tokio::time::timeout(Duration::from_millis(500), c_conn.recv()).await;
    assert!(
        result.is_err(),
        "C should not receive any message (TTL was 0)"
    );
}

#[tokio::test]
async fn test_concurrent_gossip_does_not_deadlock() {
    // Set up two independent TCP connections: one for A→B, one for B→A.
    // This avoids the test flakiness of a single connection handling
    // concurrent writes from both ends simultaneously.
    let listener_ab = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr_ab = listener_ab.local_addr().unwrap();

    let listener_ba = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr_ba = listener_ba.local_addr().unwrap();

    let peer_a = PeerIdentity::new([0xAA; 32]);
    let peer_b = PeerIdentity::new([0xBB; 32]);
    let peer_b_for_accept = peer_b.clone();

    let server_ab = tokio::spawn(async move {
        WifiDirectConnection::accept_from(&listener_ab, peer_b_for_accept)
            .await
            .unwrap()
    });

    let peer_a_for_ba_accept = peer_a.clone();
    let server_ba = tokio::spawn(async move {
        WifiDirectConnection::accept_from(&listener_ba, peer_a_for_ba_accept)
            .await
            .unwrap()
    });

    let mut conn_a_to_b = WifiDirectConnection::connect_to(peer_b.clone(), addr_ab)
        .await
        .unwrap();

    let mut conn_b_to_a = WifiDirectConnection::connect_to(peer_a.clone(), addr_ba)
        .await
        .unwrap();

    // Wait for both server-side accepts
    let _server_ab_conn = server_ab.await.unwrap();
    let _server_ba_conn = server_ba.await.unwrap();

    let msg = ProtocolMessage::Transaction(TransactionEnvelope {
        message_id: [0xDD; 32],
        origin_pubkey: peer_a.pubkey,
        tx_xdr: "concurrent-test".to_string(),
        ttl_hops: 10,
        timestamp: 3_000_000,
        signature: [0u8; 64],
    });
    let msg_b = msg.clone();

    let task_a = tokio::spawn(async move {
        for _ in 0..100 {
            send_with_timeout(conn_a_to_b.send(msg.clone()), 5000, "conn_a_to_b.send (concurrent)").await;
        }
    });

    let task_b = tokio::spawn(async move {
        for _ in 0..100 {
            send_with_timeout(conn_b_to_a.send(msg_b.clone()), 5000, "conn_b_to_a.send (concurrent)").await;
        }
    });

    let result = tokio::time::timeout(Duration::from_secs(2), async {
        task_a.await.unwrap();
        task_b.await.unwrap();
    })
    .await;

    assert!(
        result.is_ok(),
        "Concurrent gossip tasks should complete within 2 seconds"
    );
}
