pub mod dedup;

use crate::message::types::TransactionEnvelope;
use crate::message::RelayChainProof;
use crate::relay::dedup::RelayDeduplicator;
use ed25519_dalek::SigningKey;
use log::info;

/// RPC client trait for submitting transactions to Stellar
pub trait StellarRpcClient {
    fn submit_transaction(&self, tx_xdr: &str) -> Result<String, String>;
    fn current_ledger_sequence(&self) -> Result<u64, String>;
    fn current_ledger_hash(&self) -> Result<String, String>;
}

/// Relay node that processes transaction envelopes and submits them to Stellar
pub struct RelayNode {
    deduplicator: RelayDeduplicator,
    rpc_client: Box<dyn StellarRpcClient>,
    signing_key: SigningKey,
}

impl RelayNode {
    pub fn new(
        capacity: usize,
        rpc_client: Box<dyn StellarRpcClient>,
        signing_key: SigningKey,
    ) -> Self {
        Self {
            deduplicator: RelayDeduplicator::new(capacity),
            rpc_client,
            signing_key,
        }
    }

    /// Process a transaction envelope, checking for duplicates before submission
    pub fn process_envelope(
        &mut self,
        envelope: &TransactionEnvelope,
    ) -> Result<RelayChainProof, String> {
        // Check if we've already processed this message_id
        if let Some(existing_proof) = self.deduplicator.check(&envelope.message_id) {
            info!(
                "Duplicate transaction detected for message_id {:?}, returning existing proof",
                envelope.message_id
            );
            return Ok(existing_proof);
        }

        // This is a new transaction, submit it to Stellar
        let tx_hash = self.rpc_client.submit_transaction(&envelope.tx_xdr)?;
        let tx_id = decode_hash_32(&tx_hash, "transaction hash")?;
        let sequence = self.rpc_client.current_ledger_sequence()?;
        let chain_hash = decode_hash_32(&self.rpc_client.current_ledger_hash()?, "ledger hash")?;

        let proof = RelayChainProof::sign(&self.signing_key, &tx_id, &chain_hash, sequence);

        // Record that we've successfully submitted this message_id
        self.deduplicator
            .mark_submitted(envelope.message_id, proof.clone());

        Ok(proof)
    }
}

fn decode_hash_32(hash: &str, label: &str) -> Result<[u8; 32], String> {
    let normalized = hash.strip_prefix("0x").unwrap_or(hash);
    let bytes = hex::decode(normalized).map_err(|e| format!("invalid {label}: {e}"))?;

    bytes.try_into().map_err(|bytes: Vec<u8>| {
        format!("invalid {label}: expected 32 bytes, got {}", bytes.len())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::types::TransactionEnvelope;

    struct MockRpcClient {
        submit_count: std::sync::Arc<std::sync::Mutex<usize>>,
        tx_hash: String,
        ledger_hash: String,
        ledger_sequence: u64,
    }

    impl StellarRpcClient for MockRpcClient {
        fn submit_transaction(&self, _tx_xdr: &str) -> Result<String, String> {
            let mut count = self.submit_count.lock().unwrap();
            *count += 1;
            Ok(self.tx_hash.clone())
        }

        fn current_ledger_sequence(&self) -> Result<u64, String> {
            Ok(self.ledger_sequence)
        }

        fn current_ledger_hash(&self) -> Result<String, String> {
            Ok(self.ledger_hash.clone())
        }
    }

    fn create_mock_client(submit_count: std::sync::Arc<std::sync::Mutex<usize>>) -> MockRpcClient {
        MockRpcClient {
            submit_count,
            tx_hash: hex::encode([0xAB; 32]),
            ledger_hash: hex::encode([0xCD; 32]),
            ledger_sequence: 42,
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

    fn create_signing_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    #[test]
    fn test_duplicate_submission_returns_cached_hash() {
        let submit_count = std::sync::Arc::new(std::sync::Mutex::new(0));
        let rpc_client = Box::new(create_mock_client(submit_count.clone()));
        let signing_key = create_signing_key();
        let verifying_key = signing_key.verifying_key();
        let mut relay = RelayNode::new(1000, rpc_client, signing_key);

        let envelope = create_test_envelope([1u8; 32]);

        // First submission
        let proof1 = relay.process_envelope(&envelope).unwrap();
        assert_eq!(*submit_count.lock().unwrap(), 1);
        assert!(proof1.verify(&verifying_key, &[0xAB; 32]));

        // Duplicate submission should return cached proof without calling RPC
        let proof2 = relay.process_envelope(&envelope).unwrap();
        assert_eq!(*submit_count.lock().unwrap(), 1); // Still 1, no new submission
        assert_eq!(proof1, proof2);
    }

    #[test]
    fn test_multiple_identical_envelopes_within_window() {
        let submit_count = std::sync::Arc::new(std::sync::Mutex::new(0));
        let rpc_client = Box::new(create_mock_client(submit_count.clone()));
        let signing_key = create_signing_key();
        let mut relay = RelayNode::new(1000, rpc_client, signing_key);

        let envelope = create_test_envelope([1u8; 32]);

        // Submit the same envelope 3 times (simulating 3 different hop paths)
        let proof1 = relay.process_envelope(&envelope).unwrap();
        let proof2 = relay.process_envelope(&envelope).unwrap();
        let proof3 = relay.process_envelope(&envelope).unwrap();

        // Should only submit once
        assert_eq!(*submit_count.lock().unwrap(), 1);
        // All should return the same proof
        assert_eq!(proof1, proof2);
        assert_eq!(proof2, proof3);
    }

    #[test]
    fn test_different_envelopes_submit_separately() {
        let submit_count = std::sync::Arc::new(std::sync::Mutex::new(0));
        let rpc_client = Box::new(create_mock_client(submit_count.clone()));
        let signing_key = create_signing_key();
        let mut relay = RelayNode::new(1000, rpc_client, signing_key);

        let envelope1 = create_test_envelope([1u8; 32]);
        let envelope2 = create_test_envelope([2u8; 32]);

        let proof1 = relay.process_envelope(&envelope1).unwrap();
        let proof2 = relay.process_envelope(&envelope2).unwrap();

        assert_eq!(*submit_count.lock().unwrap(), 2);
        assert_eq!(proof1.chain_hash, [0xCD; 32]);
        assert_eq!(proof2.chain_hash, [0xCD; 32]);
    }
}
