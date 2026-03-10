//! Quality of Service message prioritization queue.

use std::collections::VecDeque;

use crate::message::types::{ProtocolMessage, TransactionEnvelope};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MessagePriority {
    High,
    Normal,
    Low,
}

impl MessagePriority {
    pub fn for_message(msg: &ProtocolMessage) -> Self {
        match msg {
            ProtocolMessage::TopologyUpdate(_)
            | ProtocolMessage::SyncRequest(_)
            | ProtocolMessage::SyncResponse(_) => MessagePriority::High,
            ProtocolMessage::Transaction(_) => MessagePriority::Normal,
            // Low is unused right now but reserved for background diagnostic metrics
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
    pub fn push(&mut self, msg: ProtocolMessage) {
        let priority = MessagePriority::for_message(&msg);
        match priority {
            MessagePriority::High => self.high.push_back(msg),
            MessagePriority::Normal => self.normal.push_back(msg),
            MessagePriority::Low => self.low.push_back(msg),
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
    use crate::message::types::SyncRequest;

    fn make_tx() -> ProtocolMessage {
        ProtocolMessage::Transaction(TransactionEnvelope {
            message_id: [0; 32],
            origin_pubkey: [0; 32],
            tx_xdr: String::new(),
            ttl_hops: 1,
            timestamp: 0,
            signature: [0; 64],
        })
    }

    fn make_sync() -> ProtocolMessage {
        ProtocolMessage::SyncRequest(SyncRequest {
            known_message_ids: vec![],
        })
    }

    #[test]
    fn test_high_priority_pops_first_after_normals() {
        let mut queue = PriorityQueue::new();

        // Push 10 normal priority transactions
        for _ in 0..10 {
            queue.push(make_tx());
        }

        // Push 1 high priority sync request
        queue.push(make_sync());

        assert_eq!(queue.len(), 11);

        // The first popped item should be the sync request
        let popped = queue.pop().expect("Queue shouldn't be empty");
        assert!(matches!(popped, ProtocolMessage::SyncRequest(_)));

        // The remaining 10 items should be transactions
        for _ in 0..10 {
            let p = queue.pop().expect("Should have normal items");
            assert!(matches!(p, ProtocolMessage::Transaction(_)));
        }

        assert!(queue.is_empty());
    }
}
