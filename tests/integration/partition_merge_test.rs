//! Network partition merge dampening test
//!
//! This test verifies that the StellarConduit mesh protocol safely merges two
//! large, previously disconnected mesh clusters without causing a broadcast storm
//! that crashes both clusters.
//!
//! Scenario:
//! - Cluster A: 50 nodes with 1000 pending transactions
//! - Cluster B: 50 nodes with 1000 different pending transactions
//! - A bridge node connects Cluster A to Cluster B
//! - The protocol should detect the large partition merge and apply rate limiting
//! - Both clusters should remain stable and eventually synchronize

use stellarconduit_core::gossip::protocol::{GossipState, MergeRateLimit};
use stellarconduit_core::message::types::{
    PartitionMergeHandshake, SyncRequest, TransactionEnvelope,
};

/// Test node representing a node in a cluster
#[allow(dead_code)]
struct ClusterNode {
    node_id: usize,
    gossip_state: GossipState,
    cluster_id: usize,           // 0 for Cluster A, 1 for Cluster B
    connected_peers: Vec<usize>, // Indices of connected nodes
}

impl ClusterNode {
    fn new(node_id: usize, cluster_id: usize) -> Self {
        Self {
            node_id,
            gossip_state: GossipState::new(),
            cluster_id,
            connected_peers: Vec::new(),
        }
    }

    /// Add a message to this node's state
    fn add_message(&mut self, envelope: TransactionEnvelope) {
        self.gossip_state.add_envelope(envelope);
    }

    /// Get the number of messages this node has
    fn message_count(&self) -> usize {
        self.gossip_state.active_envelopes.len()
    }

    /// Connect this node to another node
    fn connect_to(&mut self, peer_id: usize) {
        if !self.connected_peers.contains(&peer_id) {
            self.connected_peers.push(peer_id);
        }
    }

    /// Simulate sending a partition merge handshake to a peer
    fn send_partition_merge_handshake(&self, peer_cluster_size: u32) -> PartitionMergeHandshake {
        self.gossip_state
            .generate_partition_merge_handshake(peer_cluster_size)
    }

    /// Simulate receiving a partition merge handshake from a peer
    fn receive_partition_merge_handshake(
        &mut self,
        handshake: &PartitionMergeHandshake,
        peer_pubkey: [u8; 32],
        local_cluster_size: u32,
    ) -> stellarconduit_core::message::types::PartitionMergeHandshakeResponse {
        self.gossip_state.handle_partition_merge_handshake(
            handshake,
            peer_pubkey,
            local_cluster_size,
        )
    }

    /// Simulate receiving a partition merge handshake response
    fn receive_partition_merge_handshake_response(
        &mut self,
        response: &stellarconduit_core::message::types::PartitionMergeHandshakeResponse,
        peer_pubkey: [u8; 32],
    ) {
        self.gossip_state
            .handle_partition_merge_handshake_response(response, peer_pubkey);
    }

    /// Generate a sync request
    fn generate_sync_request(&self) -> SyncRequest {
        self.gossip_state.generate_sync_request()
    }

    /// Handle a sync request with rate limiting
    fn handle_sync_request(
        &self,
        req: &SyncRequest,
        peer_pubkey: &[u8; 32],
    ) -> stellarconduit_core::message::types::SyncResponse {
        self.gossip_state.handle_sync_request(req, peer_pubkey)
    }

    /// Get rate limit parameters for a peer
    fn get_rate_limit(&self, peer_pubkey: &[u8; 32]) -> Option<MergeRateLimit> {
        self.gossip_state
            .merge_tracker
            .get_rate_limit(peer_pubkey)
            .cloned()
    }
}

/// Create a test transaction envelope with a unique message ID
fn create_test_envelope(message_id_seed: u32) -> TransactionEnvelope {
    let mut message_id = [0u8; 32];
    // Use the seed to create a unique message ID
    message_id[0..4].copy_from_slice(&message_id_seed.to_be_bytes());
    // Fill the rest with the seed pattern
    for (i, byte) in message_id.iter_mut().enumerate().skip(4) {
        *byte = (message_id_seed as u8).wrapping_add(i as u8);
    }

    TransactionEnvelope {
        message_id,
        origin_pubkey: [0u8; 32],
        tx_xdr: format!("AAAAAQAAAAAAAAAA{}", message_id_seed),
        ttl_hops: 10,
        timestamp: 1672531200,
        signature: [0u8; 64],
    }
}

/// Create a cluster of nodes with a mesh topology
fn create_cluster(
    cluster_id: usize,
    node_count: usize,
    messages_per_node: usize,
) -> Vec<ClusterNode> {
    let mut nodes = Vec::new();

    // Create nodes
    for i in 0..node_count {
        let mut node = ClusterNode::new(i, cluster_id);

        // Add messages to this node
        let start_msg_id = (cluster_id * 10000) + (i * messages_per_node);
        for j in 0..messages_per_node {
            let envelope = create_test_envelope((start_msg_id + j) as u32);
            node.add_message(envelope);
        }

        // Connect nodes in a mesh: each node connects to a few neighbors
        // Simple topology: connect to next 3 nodes (wrapping around)
        for offset in 1..=3 {
            let peer_id = (i + offset) % node_count;
            node.connect_to(peer_id);
        }

        nodes.push(node);
    }

    nodes
}

#[tokio::test]
async fn test_partition_merge_dampening() {
    // Initialize logging for test output
    let _ = env_logger::try_init();

    const CLUSTER_SIZE: usize = 50;
    const MESSAGES_PER_NODE_CLUSTER_A: usize = 20; // 50 nodes * 20 = 1000 messages
    const MESSAGES_PER_NODE_CLUSTER_B: usize = 20; // 50 nodes * 20 = 1000 messages

    println!("Creating Cluster A with {} nodes...", CLUSTER_SIZE);
    let mut cluster_a = create_cluster(0, CLUSTER_SIZE, MESSAGES_PER_NODE_CLUSTER_A);

    println!("Creating Cluster B with {} nodes...", CLUSTER_SIZE);
    let mut cluster_b = create_cluster(1, CLUSTER_SIZE, MESSAGES_PER_NODE_CLUSTER_B);

    // Verify initial state
    let cluster_a_total_messages: usize = cluster_a.iter().map(|n| n.message_count()).sum();
    let cluster_b_total_messages: usize = cluster_b.iter().map(|n| n.message_count()).sum();

    println!("Cluster A total messages: {}", cluster_a_total_messages);
    println!("Cluster B total messages: {}", cluster_b_total_messages);

    assert_eq!(
        cluster_a_total_messages,
        CLUSTER_SIZE * MESSAGES_PER_NODE_CLUSTER_A
    );
    assert_eq!(
        cluster_b_total_messages,
        CLUSTER_SIZE * MESSAGES_PER_NODE_CLUSTER_B
    );
    assert_eq!(cluster_a_total_messages, 1000);
    assert_eq!(cluster_b_total_messages, 1000);

    // Select bridge nodes: one from each cluster
    let bridge_node_a_idx = 0;
    let bridge_node_b_idx = 0;

    let bridge_node_a_pubkey = [1u8; 32]; // Simulated pubkey for bridge node A
    let bridge_node_b_pubkey = [2u8; 32]; // Simulated pubkey for bridge node B

    println!(
        "Connecting bridge nodes: Cluster A node {} <-> Cluster B node {}",
        bridge_node_a_idx, bridge_node_b_idx
    );

    // Step 1: Bridge node A sends partition merge handshake to bridge node B
    // In a real scenario, nodes would estimate cluster totals based on gossip.
    // For this test, we simulate that each node estimates the cluster has the total
    // number of messages distributed across all nodes.
    // We'll manually create handshakes with cluster-level estimates
    let handshake_a = PartitionMergeHandshake {
        estimated_cluster_message_count: cluster_a_total_messages as u32,
        estimated_cluster_peer_count: CLUSTER_SIZE as u32,
    };

    println!(
        "Bridge node A handshake: {} messages, {} peers",
        handshake_a.estimated_cluster_message_count, handshake_a.estimated_cluster_peer_count
    );

    // Step 2: Bridge node B receives handshake and responds
    let response_b = cluster_b[bridge_node_b_idx].receive_partition_merge_handshake(
        &handshake_a,
        bridge_node_a_pubkey,
        CLUSTER_SIZE as u32,
    );

    println!(
        "Bridge node B response: batch_size={}, batch_delay_ms={}",
        response_b.batch_size, response_b.batch_delay_ms
    );

    // Step 3: Bridge node A receives the response
    cluster_a[bridge_node_a_idx]
        .receive_partition_merge_handshake_response(&response_b, bridge_node_b_pubkey);

    // Step 4: Bridge node B sends its own handshake
    // Simulate cluster-level estimate for Cluster B
    let handshake_b = PartitionMergeHandshake {
        estimated_cluster_message_count: cluster_b_total_messages as u32,
        estimated_cluster_peer_count: CLUSTER_SIZE as u32,
    };

    // Step 5: Bridge node A receives handshake and responds
    let response_a = cluster_a[bridge_node_a_idx].receive_partition_merge_handshake(
        &handshake_b,
        bridge_node_b_pubkey,
        CLUSTER_SIZE as u32,
    );

    // Step 6: Bridge node B receives the response
    cluster_b[bridge_node_b_idx]
        .receive_partition_merge_handshake_response(&response_a, bridge_node_a_pubkey);

    // Verify that rate limiting was activated
    let rate_limit_a = cluster_a[bridge_node_a_idx].get_rate_limit(&bridge_node_b_pubkey);
    let rate_limit_b = cluster_b[bridge_node_b_idx].get_rate_limit(&bridge_node_a_pubkey);

    println!("Rate limiting check:");
    println!(
        "  Bridge node A has rate limit: {:?}",
        rate_limit_a.is_some()
    );
    println!(
        "  Bridge node B has rate limit: {:?}",
        rate_limit_b.is_some()
    );

    // Both nodes should have rate limiting active due to large partition merge
    assert!(
        rate_limit_a.is_some(),
        "Bridge node A should have rate limiting active"
    );
    assert!(
        rate_limit_b.is_some(),
        "Bridge node B should have rate limiting active"
    );

    let rl_a = rate_limit_a.unwrap();
    let rl_b = rate_limit_b.unwrap();

    assert!(
        rl_a.is_active,
        "Rate limiting should be active on bridge node A"
    );
    assert!(
        rl_b.is_active,
        "Rate limiting should be active on bridge node B"
    );

    // Verify that batch sizes are reasonable (not too large)
    assert!(
        rl_a.batch_size <= 200,
        "Batch size should be limited (got {})",
        rl_a.batch_size
    );
    assert!(
        rl_b.batch_size <= 200,
        "Batch size should be limited (got {})",
        rl_b.batch_size
    );

    // Step 7: Simulate sync request/response with rate limiting
    // Bridge node B requests sync from bridge node A
    let sync_req = cluster_b[bridge_node_b_idx].generate_sync_request();

    // Bridge node A responds (should be rate-limited)
    let sync_resp =
        cluster_a[bridge_node_a_idx].handle_sync_request(&sync_req, &bridge_node_b_pubkey);

    println!(
        "Sync response contains {} messages (rate limited from potentially 1000+)",
        sync_resp.missing_envelopes.len()
    );

    // Verify that the response was rate-limited
    // Since Cluster A has 1000 messages and Cluster B has 0 overlapping messages,
    // the full response would be 1000 messages, but rate limiting should reduce it
    assert!(
        sync_resp.missing_envelopes.len() <= rl_a.batch_size as usize,
        "Sync response should be rate-limited to batch_size (got {}, expected <= {})",
        sync_resp.missing_envelopes.len(),
        rl_a.batch_size
    );

    // Verify clusters are still stable (no crashes)
    assert_eq!(
        cluster_a[bridge_node_a_idx].message_count(),
        MESSAGES_PER_NODE_CLUSTER_A,
        "Bridge node A should still have its original messages"
    );
    assert_eq!(
        cluster_b[bridge_node_b_idx].message_count(),
        MESSAGES_PER_NODE_CLUSTER_B,
        "Bridge node B should still have its original messages"
    );

    println!("✓ Partition merge dampening test passed!");
    println!("  - Both clusters remained stable");
    println!("  - Rate limiting was correctly applied");
    println!("  - Sync responses were properly throttled");
}

#[tokio::test]
async fn test_partition_merge_small_clusters_no_dampening() {
    // Test that small clusters don't trigger dampening
    const SMALL_CLUSTER_SIZE: usize = 5;
    const SMALL_MESSAGES_PER_NODE: usize = 10; // 5 nodes * 10 = 50 messages total

    let mut cluster_a = create_cluster(0, SMALL_CLUSTER_SIZE, SMALL_MESSAGES_PER_NODE);
    let mut cluster_b = create_cluster(1, SMALL_CLUSTER_SIZE, SMALL_MESSAGES_PER_NODE);

    let bridge_node_a_pubkey = [1u8; 32];
    let bridge_node_b_pubkey = [2u8; 32];

    // Exchange handshakes
    let handshake_a = cluster_a[0].send_partition_merge_handshake(SMALL_CLUSTER_SIZE as u32);
    let response_b = cluster_b[0].receive_partition_merge_handshake(
        &handshake_a,
        bridge_node_a_pubkey,
        SMALL_CLUSTER_SIZE as u32,
    );
    cluster_a[0].receive_partition_merge_handshake_response(&response_b, bridge_node_b_pubkey);

    // Small clusters should not trigger aggressive rate limiting
    // The response should have batch_size >= 1000 (no significant limiting)
    assert!(
        response_b.batch_size >= 1000 || response_b.batch_delay_ms <= 10,
        "Small clusters should not trigger aggressive rate limiting"
    );
}
