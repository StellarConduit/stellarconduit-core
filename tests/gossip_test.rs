use std::collections::HashSet;
use std::time::Duration;

// Updated to match your exact submodule structure
use stellarconduit_core::gossip::bloom::BloomFilter;
use stellarconduit_core::gossip::fanout::FanoutCalculator;
use stellarconduit_core::gossip::round::RoundScheduler;

// ==========================================
// Bloom Filter Tests
// ==========================================

#[test]
fn test_bloom_new_message_returns_false() {
    let mut filter = BloomFilter::new(1000);
    let message = b"unique_tx_hash_001";
    assert_eq!(filter.check_and_add(message), false);
}

#[test]
fn test_bloom_seen_message_returns_true() {
    let mut filter = BloomFilter::new(1000);
    let message = b"unique_tx_hash_002";
    
    filter.check_and_add(message);
    assert_eq!(filter.check_and_add(message), true);
}

#[test]
fn test_bloom_rotates_on_capacity() {
    let mut filter = BloomFilter::new(5);
    
    for i in 0..5 {
        let msg = format!("msg_{}", i);
        filter.check_and_add(msg.as_bytes());
    }
    
    filter.check_and_add(b"trigger_rotation_msg");
    assert_eq!(filter.check_and_add(b"msg_0"), true);
}

#[test]
fn test_bloom_false_positive_rate_acceptable() {
    let capacity = 10_000;
    let mut filter = BloomFilter::new(capacity);
    
    for i in 0..capacity {
        let msg = format!("known_hash_{}", i);
        filter.check_and_add(msg.as_bytes());
    }
    
    let mut false_positives = 0;
    let test_sample_size = 1000;
    
    for i in 0..test_sample_size {
        let msg = format!("unknown_hash_{}", i);
        if filter.check_and_add(msg.as_bytes()) {
            false_positives += 1;
        }
    }
    
    let fp_rate = (false_positives as f64) / (test_sample_size as f64);
    assert!(fp_rate <= 0.05, "False positive rate too high: {:.2}%", fp_rate * 100.0);
}

// ==========================================
// Round Scheduler Tests
// ==========================================

#[test]
fn test_scheduler_triggers_in_active_mode() {
    let mut scheduler = RoundScheduler::new();
    scheduler.record_activity();
    
    let interval = scheduler.get_interval();
    assert!(interval <= Duration::from_millis(500), "Active interval should be fast");
}

#[test]
fn test_scheduler_downgrades_to_idle() {
    let mut scheduler = RoundScheduler::new();
    scheduler.record_activity();
    scheduler.advance_time(Duration::from_secs(60));
    
    let interval = scheduler.get_interval();
    assert!(interval >= Duration::from_secs(5), "Idle interval should be slow");
}

// ==========================================
// Fanout Calculator Tests
// ==========================================

#[test]
fn test_fanout_below_min_returns_all_connections() {
    let calc = FanoutCalculator::new();
    assert_eq!(calc.calculate_target(1), 1);
    assert_eq!(calc.calculate_target(2), 2);
}

#[test]
fn test_fanout_above_max_capped() {
    let calc = FanoutCalculator::new();
    let target = calc.calculate_target(100);
    assert!(target <= 6, "Fanout should be capped at MAX_FANOUT. Got: {}", target);
}

#[test]
fn test_select_random_peers_unique() {
    let calc = FanoutCalculator::new();
    let peers = vec!["peer_A", "peer_B", "peer_C", "peer_D", "peer_E"];
    
    let selected = calc.select_random(&peers, 3);
    assert_eq!(selected.len(), 3);
    
    let unique_peers: HashSet<_> = selected.into_iter().collect();
    assert_eq!(unique_peers.len(), 3, "Selected peers must be strictly unique");
}