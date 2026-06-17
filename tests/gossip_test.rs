use std::collections::HashSet;
use std::time::Duration;

use stellarconduit_core::gossip::bloom::SlidingBloomFilter;
use stellarconduit_core::gossip::fanout::{select_random_peers, FanoutCalculator};
use stellarconduit_core::gossip::round::GossipScheduler;
use stellarconduit_core::peer::identity::PeerIdentity;

fn msg_id(byte: u8) -> [u8; 32] {
    [byte; 32]
}

// ==========================================
// Bloom Filter Tests
// ==========================================

#[test]
fn test_bloom_new_message_returns_false() {
    let mut filter = SlidingBloomFilter::new(1000, 0.01);
    assert!(!filter.check_and_add(&msg_id(1)));
}

#[test]
fn test_bloom_seen_message_returns_true() {
    let mut filter = SlidingBloomFilter::new(1000, 0.01);
    filter.check_and_add(&msg_id(1));
    assert!(filter.check_and_add(&msg_id(1)));
}

#[test]
fn test_bloom_rotates_on_capacity() {
    let mut filter = SlidingBloomFilter::new(10, 0.01);

    for i in 0u32..10 {
        let mut id = [0u8; 32];
        id[0..4].copy_from_slice(&i.to_le_bytes());
        filter.check_and_add(&id);
    }

    // Insert one more to trigger rotation
    let trigger = [10u8; 32];
    filter.check_and_add(&trigger);

    // After rotation, previously inserted items should still be found (in previous window)
    let mut first = [0u8; 32];
    first[0] = 0;
    assert!(filter.check_and_add(&first));
}

#[test]
fn test_bloom_false_positive_rate_acceptable() {
    let mut filter = SlidingBloomFilter::new(10_000, 0.01);
    let capacity = 10_000;

    for i in 0u32..capacity as u32 {
        let mut id = [0u8; 32];
        id[0..4].copy_from_slice(&i.to_le_bytes());
        filter.check_and_add(&id);
    }

    let mut false_positives = 0;
    let test_sample_size = 1000;

    for i in 0u32..test_sample_size as u32 {
        let offset = (capacity as u32) + i;
        let mut id = [0u8; 32];
        id[0..4].copy_from_slice(&offset.to_le_bytes());
        if filter.check_and_add(&id) {
            false_positives += 1;
        }
    }

    let fp_rate = (false_positives as f64) / (test_sample_size as f64);
    assert!(
        fp_rate <= 0.05,
        "False positive rate too high: {:.2}%",
        fp_rate * 100.0
    );
}

// ==========================================
// Round Scheduler Tests
// ==========================================

#[test]
fn test_scheduler_triggers_in_active_mode() {
    let scheduler = GossipScheduler::new();
    let interval = scheduler.current_interval();
    assert!(
        interval <= Duration::from_millis(500),
        "Active interval should be fast"
    );
}

#[test]
fn test_scheduler_downgrades_to_idle() {
    let mut scheduler = GossipScheduler::new();
    scheduler.last_active_msg_time = std::time::Instant::now() - Duration::from_secs(60);
    assert!(scheduler.is_idle());
    let interval = scheduler.current_interval();
    assert!(
        interval >= Duration::from_secs(5),
        "Idle interval should be slow"
    );
}

// ==========================================
// Fanout Calculator Tests
// ==========================================

#[test]
fn test_fanout_below_min_returns_all_connections() {
    let calc = FanoutCalculator::new();
    assert_eq!(calc.calculate(1, None), 1);
    assert_eq!(calc.calculate(2, None), 2);
}

#[test]
fn test_fanout_above_max_capped() {
    let calc = FanoutCalculator::new();
    let target = calc.calculate(100, None);
    assert!(
        target <= 6,
        "Fanout should be capped at MAX_FANOUT. Got: {}",
        target
    );
}

#[test]
fn test_select_random_peers_unique() {
    let peers: Vec<PeerIdentity> = (0u8..5).map(|i| PeerIdentity::new([i; 32])).collect();

    let selected = select_random_peers(&peers, 3);
    assert_eq!(selected.len(), 3);

    let unique_peers: HashSet<PeerIdentity> = selected.into_iter().collect();
    assert_eq!(
        unique_peers.len(),
        3,
        "Selected peers must be strictly unique"
    );
}

// ==========================================
// Anti-Entropy Pull Protocol Tests
// ==========================================

use std::sync::Arc;
use tokio::sync::Mutex;

use stellarconduit_core::discovery::peer_list::PeerList;
use stellarconduit_core::gossip::protocol::{run_gossip_loop, GossipState};
use stellarconduit_core::gossip::round::GossipScheduler as GossipSched;
use stellarconduit_core::gossip::strike_tracker::StrikeTracker;
use stellarconduit_core::message::types::{SyncRequest, SyncResponse, TransactionEnvelope};
use stellarconduit_core::transport::unified::{TransportManager, TransportPreference};

fn make_envelope(id: u8) -> TransactionEnvelope {
    TransactionEnvelope {
        message_id: [id; 32],
        origin_pubkey: [0u8; 32],
        tx_xdr: format!("XDR{}", id),
        ttl_hops: 10,
        timestamp: id as u64,
        signature: [0u8; 64],
    }
}

/// GossipState::generate_sync_request returns a prefix for every buffered envelope.
#[test]
fn test_anti_entropy_sync_request_contains_all_known_ids() {
    let mut state = GossipState::new();
    state.add_envelope(make_envelope(0x01));
    state.add_envelope(make_envelope(0x02));
    state.add_envelope(make_envelope(0x03));

    let req = state.generate_sync_request();
    assert_eq!(req.known_message_ids.len(), 3);

    for (i, prefix) in req.known_message_ids.iter().enumerate() {
        let expected_byte = (i + 1) as u8;
        assert_eq!(*prefix, [expected_byte; 4]);
    }
}

/// Node B asks node A what it's missing; A correctly identifies the delta.
#[test]
fn test_anti_entropy_pull_delta_only_missing_envelopes() {
    let mut node_a = GossipState::new();
    node_a.add_envelope(make_envelope(0xAA));
    node_a.add_envelope(make_envelope(0xBB));
    node_a.add_envelope(make_envelope(0xCC));

    let mut node_b = GossipState::new();
    node_b.add_envelope(make_envelope(0xAA));

    let req: SyncRequest = node_b.generate_sync_request();
    let resp: SyncResponse = node_a.handle_sync_request(&req);

    // A should send B the two envelopes it doesn't have.
    assert_eq!(resp.missing_envelopes.len(), 2);
    let ids: Vec<u8> = resp
        .missing_envelopes
        .iter()
        .map(|e| e.message_id[0])
        .collect();
    assert!(ids.contains(&0xBB));
    assert!(ids.contains(&0xCC));
}

/// After a pull, the receiver's queue grows by the right amount.
#[test]
fn test_anti_entropy_handle_sync_response_merges_into_queue() {
    let mut state = GossipState::new();
    state.add_envelope(make_envelope(0x10));

    let resp = SyncResponse {
        missing_envelopes: vec![make_envelope(0x20), make_envelope(0x30)],
    };
    state.handle_sync_response(resp);

    assert_eq!(state.active_queue.len(), 3);
}

/// Duplicate envelopes in a SyncResponse are all accepted (dedup is bloom-filter's job).
#[test]
fn test_anti_entropy_sync_response_empty_no_panic() {
    let mut state = GossipState::new();
    let resp = SyncResponse {
        missing_envelopes: vec![],
    };
    state.handle_sync_response(resp);
    assert_eq!(state.active_queue.len(), 0);
}

/// When there are no active peers the gossip loop does not panic or stall.
#[tokio::test]
async fn test_anti_entropy_loop_no_peers_does_not_stall() {
    use tokio::time::timeout;

    let scheduler = GossipSched::new();
    let strike_tracker = StrikeTracker::new();
    let peer_list = Arc::new(Mutex::new(PeerList::new(300)));
    let transport_manager = Arc::new(Mutex::new(TransportManager::new(
        TransportPreference::BleOnly,
    )));
    let state = Arc::new(Mutex::new(GossipState::new()));

    let handle = tokio::spawn(run_gossip_loop(
        scheduler,
        strike_tracker,
        peer_list,
        transport_manager,
        None,
        state,
    ));

    let result = timeout(Duration::from_millis(200), async {
        tokio::time::sleep(Duration::from_millis(100)).await;
    })
    .await;
    assert!(result.is_ok(), "gossip loop stalled with no peers");
    handle.abort();
}

/// The macro-merge backlog path activates for large SyncResponse payloads.
#[test]
fn test_anti_entropy_macro_merge_uses_backlog() {
    use stellarconduit_core::gossip::protocol::MACRO_MERGE_BATCH_SIZE;

    let mut state = GossipState::new();

    // Build a response that exceeds the macro-merge threshold (500).
    let large_resp = SyncResponse {
        missing_envelopes: (0u16..501)
            .map(|i| TransactionEnvelope {
                message_id: {
                    let mut id = [0u8; 32];
                    id[0] = (i >> 8) as u8;
                    id[1] = (i & 0xFF) as u8;
                    id
                },
                origin_pubkey: [0u8; 32],
                tx_xdr: format!("XDR{}", i),
                ttl_hops: 10,
                timestamp: i as u64,
                signature: [0u8; 64],
            })
            .collect(),
    };

    state.handle_sync_response(large_resp);

    // Nothing should be in the active queue yet — all in the backlog.
    assert_eq!(state.active_queue.len(), 0);
    assert_eq!(state.pending_macro_merge_len(), 501);

    // Process one batch.
    let moved = state.process_paced_recovery_batch(MACRO_MERGE_BATCH_SIZE);
    assert_eq!(moved, MACRO_MERGE_BATCH_SIZE);
    assert_eq!(state.active_queue.len(), MACRO_MERGE_BATCH_SIZE);
    assert_eq!(
        state.pending_macro_merge_len(),
        501 - MACRO_MERGE_BATCH_SIZE
    );
}
