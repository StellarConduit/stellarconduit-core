pub mod dedup;

use crate::message::types::TransactionEnvelope;
use crate::relay::dedup::RelayDeduplicator;
use log::info;

/// RPC client trait for submitting transactions to Stellar
pub trait StellarRpcClient {
    fn submit_transaction(&self, tx_xdr: &str) -> Result<String, String>;
}

/// Relay node that processes transaction envelopes and submits them to Stellar
pub struct RelayNode {
    deduplicator: RelayDeduplicator,
    rpc_client: Box<dyn StellarRpcClient>,
}

impl RelayNode {
    pub fn new(capacity: usize, rpc_client: Box<dyn StellarRpcClient>) -> Self {
        Self {
            deduplicator: RelayDeduplicator::new(capacity),
            rpc_client,
        }
    }

    /// Process a transaction envelope, checking for duplicates before submission
    pub fn process_envelope(&mut self, envelope: &TransactionEnvelope) -> Result<String, String> {
        // Check if we've already processed this message_id
        if let Some(existing_hash) = self.deduplicator.check(&envelope.message_id) {
            info!(
                "Duplicate transaction detected for message_id {:?}, returning existing hash: {}",
                envelope.message_id, existing_hash
            );
            return Ok(existing_hash);
        }

        // This is a new transaction, submit it to Stellar
        let tx_hash = self.rpc_client.submit_transaction(&envelope.tx_xdr)?;

        // Record that we've successfully submitted this message_id
        self.deduplicator
            .mark_submitted(envelope.message_id, tx_hash.clone());

        Ok(tx_hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::types::TransactionEnvelope;

    struct MockRpcClient {
        submit_count: std::sync::Arc<std::sync::Mutex<usize>>,
    }

    impl StellarRpcClient for MockRpcClient {
        fn submit_transaction(&self, _tx_xdr: &str) -> Result<String, String> {
            let mut count = self.submit_count.lock().unwrap();
            *count += 1;
            Ok(format!("tx_hash_{}", count))
        }
    }

    fn create_test_envelope(message_id: [u8; 32]) -> TransactionEnvelope {
        TransactionEnvelope {
            message_id,
            origin_pubkey: [2u8; 32],
            tx_xdr: "AAAAAQAAAAAAAAAA".to_string(),
            ttl_hops: 10,
            timestamp: 1672531200,
            signature: [3u8; 64],
        }
    }

    #[test]
    fn test_duplicate_submission_returns_cached_hash() {
        let submit_count = std::sync::Arc::new(std::sync::Mutex::new(0));
        let rpc_client = Box::new(MockRpcClient {
            submit_count: submit_count.clone(),
        });
        let mut relay = RelayNode::new(1000, rpc_client);

        let envelope = create_test_envelope([1u8; 32]);

        // First submission
        let hash1 = relay.process_envelope(&envelope).unwrap();
        assert_eq!(*submit_count.lock().unwrap(), 1);

        // Duplicate submission should return cached hash without calling RPC
        let hash2 = relay.process_envelope(&envelope).unwrap();
        assert_eq!(*submit_count.lock().unwrap(), 1); // Still 1, no new submission
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_multiple_identical_envelopes_within_window() {
        let submit_count = std::sync::Arc::new(std::sync::Mutex::new(0));
        let rpc_client = Box::new(MockRpcClient {
            submit_count: submit_count.clone(),
        });
        let mut relay = RelayNode::new(1000, rpc_client);

        let envelope = create_test_envelope([1u8; 32]);

        // Submit the same envelope 3 times (simulating 3 different hop paths)
        let hash1 = relay.process_envelope(&envelope).unwrap();
        let hash2 = relay.process_envelope(&envelope).unwrap();
        let hash3 = relay.process_envelope(&envelope).unwrap();

        // Should only submit once
        assert_eq!(*submit_count.lock().unwrap(), 1);
        // All should return the same hash
        assert_eq!(hash1, hash2);
        assert_eq!(hash2, hash3);
    }

    #[test]
    fn test_different_envelopes_submit_separately() {
        let submit_count = std::sync::Arc::new(std::sync::Mutex::new(0));
        let rpc_client = Box::new(MockRpcClient {
            submit_count: submit_count.clone(),
        });
        let mut relay = RelayNode::new(1000, rpc_client);

        let envelope1 = create_test_envelope([1u8; 32]);
        let envelope2 = create_test_envelope([2u8; 32]);

        let hash1 = relay.process_envelope(&envelope1).unwrap();
        let hash2 = relay.process_envelope(&envelope2).unwrap();

        assert_eq!(*submit_count.lock().unwrap(), 2);
        assert_ne!(hash1, hash2);
    }
}
