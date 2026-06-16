//! Mesh propagation and macro-merge dampening simulation tests.
//!
//! These tests model two independent clusters that each hold a disjoint set
//! of transaction envelopes. When a bridge node connects the clusters and
//! performs a SyncRequest / SyncResponse exchange, the receiving side uses
//! the macro-merge dampening logic in `GossipState` to gradually ingest the
//! missing envelopes instead of flooding the mesh all at once.

use stellarconduit_core::gossip::protocol::{GossipState, MACRO_MERGE_BATCH_SIZE};
use stellarconduit_core::message::types::{SyncRequest, SyncResponse, TransactionEnvelope};

/// Simple token bucket used to model per-link rate limiting in tests.
/// Time is tracked in virtual milliseconds, advanced manually by the test.
struct TokenBucket {
    capacity: usize,
    tokens: f64,
    refill_per_sec: f64,
    last_ms: u64,
}

impl TokenBucket {
    fn new(capacity: usize, refill_per_sec: usize) -> Self {
        Self {
            capacity,
            tokens: capacity as f64,
            refill_per_sec: refill_per_sec as f64,
            last_ms: 0,
        }
    }

    fn advance_time(&mut self, now_ms: u64) {
        if now_ms <= self.last_ms {
            return;
        }
        let elapsed_ms = now_ms - self.last_ms;
        let add = self.refill_per_sec * (elapsed_ms as f64 / 1000.0);
        self.tokens = (self.tokens + add).min(self.capacity as f64);
        self.last_ms = now_ms;
    }

    /// Returns how many tokens can be consumed (up to `requested`) without
    /// exceeding the bucket's capacity, and deducts that amount.
    fn take(&mut self, requested: usize) -> usize {
        let available = self.tokens.floor() as usize;
        if available == 0 {
            return 0;
        }
        let to_take = requested.min(available);
        self.tokens -= to_take as f64;
        to_take
    }
}

fn make_envelope(base_id: u8, idx: u32, timestamp: u64) -> TransactionEnvelope {
    let mut message_id = [0u8; 32];
    message_id[0] = base_id;
    // encode the index into the ID for uniqueness
    message_id[1..5].copy_from_slice(&idx.to_le_bytes());

    TransactionEnvelope {
        message_id,
        origin_pubkey: [base_id; 32],
        tx_xdr: format!("XDR_{}_{}", base_id, idx),
        ttl_hops: 10,
        timestamp,
        signature: [0u8; 64],
    }
}

/// Helper to populate a node with `count` envelopes that all share the same
/// base ID but have monotonically increasing timestamps.
fn populate_cluster_node(state: &mut GossipState, base_id: u8, count: usize) {
    for i in 0..count {
        let env = make_envelope(base_id, i as u32, i as u64);
        state.add_envelope(env);
    }
}

#[tokio::test]
async fn macro_merge_between_two_clusters_is_paced_and_rate_limited() {
    const MESSAGES_PER_CLUSTER: usize = 1000;
    const VIRTUAL_LIMIT_MS: u64 = 5_000;

    // For this simulation we only model one representative node per cluster,
    // but conceptually there are 50 nodes that share each cluster's state.
    let mut cluster_a_bridge = GossipState::new();
    let mut cluster_b_bridge = GossipState::new();

    // Populate each cluster with a disjoint set of envelopes.
    populate_cluster_node(&mut cluster_a_bridge, 0xAA, MESSAGES_PER_CLUSTER);
    populate_cluster_node(&mut cluster_b_bridge, 0xBB, MESSAGES_PER_CLUSTER);

    // Bridge B -> A: B generates a SyncRequest describing its state, A responds
    // with the envelopes that B is missing (its entire 0xAA set), and B then
    // performs macro-merge dampened recovery.
    let req_from_b: SyncRequest = cluster_b_bridge.generate_sync_request();
    let resp_from_a: SyncResponse = cluster_a_bridge.handle_sync_request(&req_from_b);
    assert_eq!(
        resp_from_a.missing_envelopes.len(),
        MESSAGES_PER_CLUSTER,
        "Bridge A should see that B is missing all of A's envelopes"
    );

    // This SyncResponse is large enough to trigger macro-merge handling.
    cluster_b_bridge.handle_sync_response(resp_from_a);
    assert!(
        cluster_b_bridge.pending_macro_merge_len() >= MESSAGES_PER_CLUSTER,
        "All envelopes from A should reside in B's macro-merge backlog initially"
    );
    assert_eq!(
        cluster_b_bridge.active_queue.len(),
        MESSAGES_PER_CLUSTER,
        "Macro-merge envelopes must not be injected into the active queue all at once; only B's original cluster messages should be present"
    );

    // Token bucket representing the constrained BLE / WiFi-Direct link.
    // Allow at most 250 envelopes per second, with initial full capacity.
    let mut bucket = TokenBucket::new(250, 250);

    let mut virtual_ms = 0u64;
    let mut total_pulled = 0usize;
    let tick_ms = 250u64; // 4 ticks per second

    while cluster_b_bridge.pending_macro_merge_len() > 0 && virtual_ms <= VIRTUAL_LIMIT_MS {
        bucket.advance_time(virtual_ms);
        let allowed = bucket.take(MACRO_MERGE_BATCH_SIZE);
        if allowed > 0 {
            let moved = cluster_b_bridge.process_paced_recovery_batch(allowed);
            total_pulled += moved;
            // We should never move more messages than the token bucket allowed.
            assert!(
                moved <= allowed,
                "Paced recovery must not exceed token bucket allowance"
            );
        }
        virtual_ms += tick_ms;
    }

    assert_eq!(
        total_pulled, MESSAGES_PER_CLUSTER,
        "Bridge B should have pulled all of A's envelopes within the virtual time limit"
    );
    assert_eq!(
        cluster_b_bridge.pending_macro_merge_len(),
        0,
        "Macro-merge backlog should be fully drained after paced recovery"
    );

    // All envelopes from A should now appear in B's active queue. The portion
    // of the queue corresponding to the macro-merge (i.e. the entries added
    // after B's original cluster messages) must be ordered by increasing
    // timestamp, even if older local messages still appear earlier in the
    // queue.
    let all_after_recovery: Vec<TransactionEnvelope> = cluster_b_bridge
        .active_queue
        .iter_envelopes()
        .cloned()
        .collect();
    assert!(
        all_after_recovery.len() >= 2 * MESSAGES_PER_CLUSTER,
        "After recovery B should see both its original cluster and A's cluster"
    );
    let macro_slice = &all_after_recovery[MESSAGES_PER_CLUSTER..];
    let mut last_ts = 0u64;
    for env in macro_slice {
        assert!(
            env.timestamp >= last_ts,
            "Recovered macro-merge envelopes should be ordered by increasing age (timestamp)"
        );
        last_ts = env.timestamp;
    }

    // Symmetric bridge A <- B: A pulls B's envelopes under the same constraints.
    let req_from_a: SyncRequest = cluster_a_bridge.generate_sync_request();
    let resp_from_b: SyncResponse = cluster_b_bridge.handle_sync_request(&req_from_a);
    assert_eq!(
        resp_from_b.missing_envelopes.len(),
        MESSAGES_PER_CLUSTER,
        "Bridge B should see that A is missing all of B's envelopes"
    );

    cluster_a_bridge.handle_sync_response(resp_from_b);
    assert!(
        cluster_a_bridge.pending_macro_merge_len() >= MESSAGES_PER_CLUSTER,
        "All envelopes from B should reside in A's macro-merge backlog initially"
    );

    let mut bucket_ab = TokenBucket::new(250, 250);
    let mut virtual_ms_ab = 0u64;
    let mut total_pulled_ab = 0usize;

    while cluster_a_bridge.pending_macro_merge_len() > 0 && virtual_ms_ab <= VIRTUAL_LIMIT_MS {
        bucket_ab.advance_time(virtual_ms_ab);
        let allowed = bucket_ab.take(MACRO_MERGE_BATCH_SIZE);
        if allowed > 0 {
            let moved = cluster_a_bridge.process_paced_recovery_batch(allowed);
            total_pulled_ab += moved;
            assert!(
                moved <= allowed,
                "Paced recovery A<-B must not exceed token bucket allowance"
            );
        }
        virtual_ms_ab += tick_ms;
    }

    assert_eq!(
        total_pulled_ab, MESSAGES_PER_CLUSTER,
        "Bridge A should have pulled all of B's envelopes within the virtual time limit"
    );
    assert_eq!(
        cluster_a_bridge.pending_macro_merge_len(),
        0,
        "Macro-merge backlog A<-B should be fully drained after paced recovery"
    );

    // Both representative bridge nodes now have the union of the two clusters'
    // 2 * 1000 envelopes available for gossip without having exceeded their
    // token bucket rate limits at any step.
    assert!(
        cluster_a_bridge.active_queue.len() >= 2 * MESSAGES_PER_CLUSTER,
        "Cluster A bridge node should see the union of both clusters"
    );
    assert!(
        cluster_b_bridge.active_queue.len() >= 2 * MESSAGES_PER_CLUSTER,
        "Cluster B bridge node should see the union of both clusters"
    );
}
