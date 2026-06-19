//! Quality of Service message prioritization queue.

use std::cmp::Reverse;
use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::message::types::{ProtocolMessage, TransactionEnvelope};

/// Maximum envelopes held in the Normal tier before the oldest is dropped.
const NORMAL_CAPACITY: usize = 10_000;

/// Maximum messages held in the Low tier before the oldest is dropped.
const LOW_CAPACITY: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MessagePriority {
    High,
    Normal,
    Low,
}

impl MessagePriority {
    pub fn for_message(msg: &ProtocolMessage) -> Self {
        match msg {
            ProtocolMessage::TopologyUpdate(update) => {
                MessagePriority::for_topology_update(update.hops_to_relay)
            }
            ProtocolMessage::SyncRequest(_) | ProtocolMessage::SyncResponse(_) => {
                MessagePriority::High
            }
            ProtocolMessage::Transaction(_) => MessagePriority::Normal,
        }
    }

    /// TopologyUpdates from peers more than 5 hops from a relay are less useful
    /// for routing decisions and are demoted to Low priority.
    pub fn for_topology_update(hops_to_relay: u8) -> Self {
        if hops_to_relay > 5 {
            MessagePriority::Low
        } else {
            MessagePriority::High
        }
    }

    /// Returns a numeric sort key for a message. Higher = process first.
    /// Used within the Normal tier to prioritize urgent envelopes.
    pub fn urgency_score(msg: &ProtocolMessage) -> u32 {
        match msg {
            ProtocolMessage::Transaction(env) => {
                // Urgency increases as TTL decreases: TTL=15 → 1, TTL=1 → 15
                let ttl_urgency = (16u32).saturating_sub(env.ttl_hops as u32);

                // Urgency increases with age: older envelopes need forwarding sooner
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let age_secs = now.saturating_sub(env.timestamp);
                let age_urgency = (age_secs / 60).min(10) as u32;

                ttl_urgency + age_urgency
            }
            _ => 0,
        }
    }
}

/// A priority queue that orders messages into High, Normal, and Low queues.
pub struct PriorityQueue {
    high: VecDeque<ProtocolMessage>,
    normal: VecDeque<ProtocolMessage>,
    low: VecDeque<ProtocolMessage>,
}

impl PriorityQueue {
    pub fn new() -> Self {
        Self {
            high: VecDeque::new(),
            normal: VecDeque::new(),
            low: VecDeque::new(),
        }
    }

    /// Push a message into the appropriate priority tier.
    ///
    /// Returns the dropped message if the Normal or Low tier capacity was exceeded.
    pub fn push(&mut self, msg: ProtocolMessage) -> Option<ProtocolMessage> {
        let priority = MessagePriority::for_message(&msg);
        match priority {
            MessagePriority::High => {
                self.high.push_back(msg);
                None
            }
            MessagePriority::Normal => {
                self.normal.push_back(msg);
                if self.normal.len() > NORMAL_CAPACITY {
                    let dropped = self.normal.pop_front();
                    log::warn!(
                        "Normal queue exceeded capacity ({NORMAL_CAPACITY}), dropping oldest envelope"
                    );
                    dropped
                } else {
                    None
                }
            }
            MessagePriority::Low => {
                self.low.push_back(msg);
                if self.low.len() > LOW_CAPACITY {
                    let dropped = self.low.pop_front();
                    log::warn!(
                        "Low queue exceeded capacity ({LOW_CAPACITY}), dropping oldest message"
                    );
                    dropped
                } else {
                    None
                }
            }
        }
    }

    /// Pop the next highest priority message from the queue.
    pub fn pop(&mut self) -> Option<ProtocolMessage> {
        if let Some(msg) = self.high.pop_front() {
            return Some(msg);
        }
        if let Some(msg) = self.normal.pop_front() {
            return Some(msg);
        }
        if let Some(msg) = self.low.pop_front() {
            return Some(msg);
        }
        None
    }

    /// Drain up to `batch_size` messages from the Normal tier, sorted by urgency
    /// score descending (highest urgency first). Remaining Normal messages are
    /// retained for future rounds.
    pub fn drain_batch(&mut self, batch_size: usize) -> Vec<ProtocolMessage> {
        if batch_size == 0 {
            return Vec::new();
        }

        let mut all_normal: Vec<ProtocolMessage> = self.normal.drain(..).collect();
        all_normal.sort_by_key(|msg| Reverse(MessagePriority::urgency_score(msg)));

        let take = batch_size.min(all_normal.len());
        let batch: Vec<ProtocolMessage> = all_normal.drain(..take).collect();

        // Return remaining messages to the queue in urgency order so the next
        // drain_batch call doesn't need to re-sort from scratch.
        self.normal.extend(all_normal);

        batch
    }

    /// The total number of messages spanning all priority tiers.
    pub fn len(&self) -> usize {
        self.high.len() + self.normal.len() + self.low.len()
    }

    /// Check if all tiers are empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Iterates over all `TransactionEnvelope`s in the queue (typically inside `Normal`).
    pub fn iter_envelopes(&self) -> impl Iterator<Item = &TransactionEnvelope> {
        self.high
            .iter()
            .chain(self.normal.iter())
            .chain(self.low.iter())
            .filter_map(|msg| match msg {
                ProtocolMessage::Transaction(env) => Some(env),
                _ => None,
            })
    }
}

impl Default for PriorityQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::types::{SyncRequest, TopologyUpdate};

    fn make_tx_with_ttl(ttl: u8) -> ProtocolMessage {
        ProtocolMessage::Transaction(TransactionEnvelope {
            message_id: [ttl; 32],
            origin_pubkey: [0; 32],
            tx_xdr: String::new(),
            ttl_hops: ttl,
            timestamp: 0,
            signature: [0; 64],
        })
    }

    fn make_tx() -> ProtocolMessage {
        make_tx_with_ttl(1)
    }

    fn make_sync() -> ProtocolMessage {
        ProtocolMessage::SyncRequest(SyncRequest {
            known_message_ids: vec![],
        })
    }

    fn make_topology_update(hops_to_relay: u8) -> ProtocolMessage {
        ProtocolMessage::TopologyUpdate(TopologyUpdate {
            origin_pubkey: [hops_to_relay; 32],
            directly_connected_peers: vec![],
            hops_to_relay,
            topology_flags: vec![],
        })
    }

    #[test]
    fn test_high_priority_pops_first_after_normals() {
        let mut queue = PriorityQueue::new();

        for _ in 0..10 {
            queue.push(make_tx());
        }

        queue.push(make_sync());

        assert_eq!(queue.len(), 11);

        let popped = queue.pop().expect("Queue shouldn't be empty");
        assert!(matches!(popped, ProtocolMessage::SyncRequest(_)));

        for _ in 0..10 {
            let p = queue.pop().expect("Should have normal items");
            assert!(matches!(p, ProtocolMessage::Transaction(_)));
        }

        assert!(queue.is_empty());
    }

    #[test]
    fn test_urgency_score_increases_with_low_ttl() {
        let score_high_ttl = MessagePriority::urgency_score(&make_tx_with_ttl(15));
        let score_low_ttl = MessagePriority::urgency_score(&make_tx_with_ttl(1));

        assert!(
            score_low_ttl > score_high_ttl,
            "TTL=1 (score={score_low_ttl}) should have higher urgency than TTL=15 (score={score_high_ttl})"
        );
    }

    #[test]
    fn test_drain_batch_sorted_by_urgency() {
        let mut queue = PriorityQueue::new();

        for ttl in [10u8, 2, 8, 1, 5] {
            queue.push(make_tx_with_ttl(ttl));
        }

        let batch = queue.drain_batch(5);
        assert_eq!(batch.len(), 5);

        let ttls: Vec<u8> = batch
            .iter()
            .map(|msg| match msg {
                ProtocolMessage::Transaction(env) => env.ttl_hops,
                _ => panic!("Expected Transaction"),
            })
            .collect();

        // Lower TTL → higher urgency → drained first
        assert_eq!(ttls, vec![1, 2, 5, 8, 10]);
    }

    #[test]
    fn test_topology_update_uses_low_tier_for_far_peers() {
        let mut queue = PriorityQueue::new();

        // hops=8 > 5 → Low tier
        queue.push(make_topology_update(8));

        // A Normal-tier transaction
        queue.push(make_tx_with_ttl(10));

        // pop() order: High → Normal → Low
        // The transaction (Normal) must come before the topology update (Low)
        let first = queue.pop().expect("Should have a message");
        assert!(
            matches!(first, ProtocolMessage::Transaction(_)),
            "Normal-tier transaction should precede Low-tier topology update"
        );

        let second = queue.pop().expect("Should have a message");
        assert!(
            matches!(second, ProtocolMessage::TopologyUpdate(_)),
            "hops=8 topology update should be in Low tier"
        );
    }

    #[test]
    fn test_normal_tier_cap_drops_oldest() {
        let mut queue = PriorityQueue::new();

        for i in 0u32..=10_000 {
            let mut id = [0u8; 32];
            id[0..4].copy_from_slice(&i.to_le_bytes());
            queue.push(ProtocolMessage::Transaction(TransactionEnvelope {
                message_id: id,
                origin_pubkey: [0; 32],
                tx_xdr: String::new(),
                ttl_hops: 10,
                timestamp: 0,
                signature: [0; 64],
            }));
        }

        // 10 001 pushed, oldest dropped → exactly 10 000 remain
        assert_eq!(queue.normal.len(), NORMAL_CAPACITY);
        assert_eq!(queue.len(), NORMAL_CAPACITY);
    }
}
