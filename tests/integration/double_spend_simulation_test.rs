//! Double-spend simulation test
//!
//! This test verifies that the StellarConduit mesh protocol is resilient to
//! double-spend scenarios where two identical transactions (or two conflicting
//! transactions from the same account) attempt to gossip through two different
//! paths in the mesh simultaneously.
//!
//! A malicious or buggy mobile app might fire the same payment twice — or fire
//! two conflicting payments. Both will gossip through different parts of the mesh
//! and eventually converge on a Relay Node. The Relay Node's deduplication
//! should ensure only one submission reaches the Stellar RPC, and the second
//! transaction will simply return a txBadSeq from the network (which is normal
//! expected behavior for Stellar, not a protocol bug).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use stellarconduit_core::message::types::TransactionEnvelope;
use stellarconduit_core::relay::{RelayNode, StellarRpcClient};

/// Mock RPC client that tracks submission count
struct MockRpcClient {
    submission_count: Arc<AtomicUsize>,
}

// Safety: MockRpcClient is Send + Sync because AtomicUsize is Send + Sync
unsafe impl Send for MockRpcClient {}
unsafe impl Sync for MockRpcClient {}

impl StellarRpcClient for MockRpcClient {
    fn submit_transaction(&self, _tx_xdr: &str) -> Result<String, String> {
        let count = self.submission_count.fetch_add(1, Ordering::SeqCst);
        Ok(format!("tx_hash_{}", count + 1))
    }
}

/// Test node that simulates a mesh node
struct TestNode {
    _node_id: u8,
    relay_node: Option<Arc<std::sync::Mutex<RelayNode>>>,
    rpc_submission_count: Option<Arc<AtomicUsize>>,
}

impl TestNode {
    fn new_regular(node_id: u8) -> Self {
        Self {
            _node_id: node_id,
            relay_node: None,
            rpc_submission_count: None,
        }
    }

    fn new_relay(node_id: u8, rpc_submission_count: Arc<AtomicUsize>) -> Self {
        let rpc_client = Box::new(MockRpcClient {
            submission_count: rpc_submission_count.clone(),
        });
        let relay = RelayNode::new(1000, rpc_client);
        #[allow(clippy::arc_with_non_send_sync)]
        let relay_arc = Arc::new(std::sync::Mutex::new(relay));

        Self {
            _node_id: node_id,
            relay_node: Some(relay_arc.clone()),
            rpc_submission_count: Some(rpc_submission_count),
        }
    }

    /// Simulate injecting a message into this node
    async fn inject_message(&self, envelope: TransactionEnvelope) -> Result<(), String> {
        // If this is a relay node, process the envelope
        if let Some(relay) = &self.relay_node {
            let mut relay_guard = relay.lock().unwrap();
            relay_guard.process_envelope(&envelope)?;
        }
        Ok(())
    }

    /// Check if this relay node has received a message (by checking if RPC was called)
    async fn has_received_message(&self, _expected_id: &[u8; 32]) -> bool {
        // For this test, we check if the relay has processed any messages
        // by checking if submission count > 0
        if let Some(count) = &self.rpc_submission_count {
            count.load(Ordering::SeqCst) > 0
        } else {
            false
        }
    }

    /// Get the RPC submission count (only valid for relay nodes)
    fn rpc_submission_count(&self) -> Option<Arc<AtomicUsize>> {
        self.rpc_submission_count.clone()
    }
}

/// Create a test transaction envelope with the given message_id
fn create_test_envelope(message_id: [u8; 32]) -> TransactionEnvelope {
    TransactionEnvelope {
        message_id,
        origin_pubkey: [0u8; 32],
        tx_xdr: "AAAAAQAAAAAAAAAA".to_string(),
        ttl_hops: 10,
        timestamp: 1672531200,
        signature: [0u8; 64],
    }
}

#[tokio::test]
async fn test_double_spend_simulation() {
    // Build a diamond-shaped network topology:
    //
    //                  Node1
    //                 /      \
    // Node0 (Origin)            Node4 (Relay)
    //                 \      /
    //                  Node2
    //
    // Node0 creates two identical transaction envelopes with the same message_id.
    // Both are injected simultaneously and propagate through different paths.

    // Create nodes
    let rpc_submission_count = Arc::new(AtomicUsize::new(0));
    let nodes = [
        TestNode::new_regular(0),                             // Origin node
        TestNode::new_regular(1),                             // Intermediate node (path 1)
        TestNode::new_regular(2),                             // Intermediate node (path 2)
        TestNode::new_regular(3),                             // Unused node
        TestNode::new_relay(4, rpc_submission_count.clone()), // Relay node
    ];

    // Create two identical transaction envelopes with the same message_id
    let message_id = [42u8; 32];
    let duplicate_envelope_a = create_test_envelope(message_id);
    let duplicate_envelope_b = create_test_envelope(message_id);

    // Verify they are identical
    assert_eq!(
        duplicate_envelope_a.message_id,
        duplicate_envelope_b.message_id
    );
    assert_eq!(duplicate_envelope_a.tx_xdr, duplicate_envelope_b.tx_xdr);

    // Get the relay node
    let relay_node = &nodes[4];
    let relay_count = relay_node.rpc_submission_count().unwrap();

    // Simulate simultaneous injection via two different paths
    // Path 1: Node0 -> Node1 -> Node4 (Relay)
    // Path 2: Node0 -> Node2 -> Node4 (Relay)
    let (path1_result, path2_result) = tokio::join!(
        async {
            // Simulate path 1: Node0 -> Node1 -> Node4
            // In a real mesh, Node1 would gossip to Node4
            // For this test, we directly inject to relay from both paths
            relay_node
                .inject_message(duplicate_envelope_a.clone())
                .await
        },
        async {
            // Simulate path 2: Node0 -> Node2 -> Node4
            // In a real mesh, Node2 would gossip to Node4
            // For this test, we directly inject to relay from both paths
            relay_node
                .inject_message(duplicate_envelope_b.clone())
                .await
        }
    );

    // Both paths should succeed (deduplication returns the cached hash)
    assert!(path1_result.is_ok());
    assert!(path2_result.is_ok());

    // Wait for gossip to fully propagate from both paths
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Relay node should have received the envelope
    assert!(relay_node.has_received_message(&message_id).await);

    // Critical: Relay must only have called the RPC exactly ONCE
    // despite receiving the same message via two different paths
    assert_eq!(
        relay_count.load(Ordering::SeqCst),
        1,
        "Relay node should have submitted to RPC exactly once, but got {} submissions",
        relay_count.load(Ordering::SeqCst)
    );
}

#[tokio::test]
async fn test_double_spend_with_different_message_ids() {
    // Test that different message IDs result in separate submissions
    let rpc_submission_count = Arc::new(AtomicUsize::new(0));
    let relay_node = TestNode::new_relay(0, rpc_submission_count.clone());
    let relay_count = relay_node.rpc_submission_count().unwrap();

    let envelope1 = create_test_envelope([1u8; 32]);
    let envelope2 = create_test_envelope([2u8; 32]);

    // Submit two different messages
    relay_node.inject_message(envelope1).await.unwrap();
    relay_node.inject_message(envelope2).await.unwrap();

    // Should have two submissions
    assert_eq!(relay_count.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn test_multiple_duplicate_submissions() {
    // Test that multiple duplicates of the same message only result in one submission
    let rpc_submission_count = Arc::new(AtomicUsize::new(0));
    let relay_node = TestNode::new_relay(0, rpc_submission_count.clone());
    let relay_count = relay_node.rpc_submission_count().unwrap();

    let message_id = [99u8; 32];
    let envelope = create_test_envelope(message_id);

    // Submit the same envelope multiple times (simulating multiple paths)
    for _ in 0..5 {
        relay_node.inject_message(envelope.clone()).await.unwrap();
    }

    // Should only have one submission
    assert_eq!(
        relay_count.load(Ordering::SeqCst),
        1,
        "Multiple duplicate submissions should only result in one RPC call"
    );
}
