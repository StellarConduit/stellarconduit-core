pub mod dedup;
pub mod rpc_client;

use async_trait::async_trait;
use ed25519_dalek::SigningKey;
use log::info;

use crate::message::relay_proof::RelayChainProof;
use crate::message::types::TransactionEnvelope;
use crate::relay::dedup::RelayDeduplicator;

/// Errors returned by the Stellar RPC layer.
#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("HTTP error: {status} — {body}")]
    Http { status: u16, body: String },
    #[error("Transaction rejected: {reason}")]
    TransactionRejected { reason: String },
    #[error("Network error: {0}")]
    Network(String),
    #[error("Timeout after {timeout_ms}ms")]
    Timeout { timeout_ms: u64 },
}

/// Async trait for submitting transactions to the Stellar network.
#[async_trait]
pub trait StellarRpcClient: Send + Sync {
    async fn submit_transaction(&self, tx_xdr: &str) -> Result<String, RpcError>;
    async fn get_account_sequence(&self, public_key: &str) -> Result<u64, RpcError>;
    async fn get_ledger_sequence(&self) -> Result<u64, RpcError>;
    async fn get_ledger_hash(&self) -> Result<String, RpcError>;
}

/// Relay node that processes transaction envelopes and submits them to Stellar.
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

    /// Process a transaction envelope, checking for duplicates before submission.
    pub async fn process_envelope(
        &mut self,
        envelope: &TransactionEnvelope,
    ) -> Result<RelayChainProof, RpcError> {
        if let Some(existing_proof) = self.deduplicator.check(&envelope.message_id) {
            info!(
                "Duplicate transaction {:?}, returning cached proof",
                envelope.message_id
            );
            return Ok(existing_proof);
        }

        let tx_hash = self.rpc_client.submit_transaction(&envelope.tx_xdr).await?;
        let tx_id = decode_hash_32(&tx_hash, "transaction hash").map_err(RpcError::Network)?;
        let sequence = self.rpc_client.get_ledger_sequence().await?;
        let chain_hash_str = self.rpc_client.get_ledger_hash().await?;
        let chain_hash =
            decode_hash_32(&chain_hash_str, "ledger hash").map_err(RpcError::Network)?;

        let proof = RelayChainProof::sign(&self.signing_key, &tx_id, &chain_hash, sequence);
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct MockRpcClient {
        submit_count: Arc<AtomicUsize>,
        tx_hash: String,
        ledger_hash: String,
        ledger_sequence: u64,
    }

    #[async_trait]
    impl StellarRpcClient for MockRpcClient {
        async fn submit_transaction(&self, _tx_xdr: &str) -> Result<String, RpcError> {
            self.submit_count.fetch_add(1, Ordering::SeqCst);
            Ok(self.tx_hash.clone())
        }
        async fn get_account_sequence(&self, _: &str) -> Result<u64, RpcError> {
            Ok(0)
        }
        async fn get_ledger_sequence(&self) -> Result<u64, RpcError> {
            Ok(self.ledger_sequence)
        }
        async fn get_ledger_hash(&self) -> Result<String, RpcError> {
            Ok(self.ledger_hash.clone())
        }
    }

    fn make_client(submit_count: Arc<AtomicUsize>) -> MockRpcClient {
        MockRpcClient {
            submit_count,
            tx_hash: hex::encode([0xABu8; 32]),
            ledger_hash: hex::encode([0xCDu8; 32]),
            ledger_sequence: 42,
        }
    }

    fn create_signing_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
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

    #[tokio::test]
    async fn test_duplicate_submission_returns_cached_hash() {
        let submit_count = Arc::new(AtomicUsize::new(0));
        let signing_key = create_signing_key();
        let verifying_key = signing_key.verifying_key();
        let mut relay = RelayNode::new(
            1000,
            Box::new(make_client(submit_count.clone())),
            signing_key,
        );
        let envelope = create_test_envelope([1u8; 32]);

        let proof1 = relay.process_envelope(&envelope).await.unwrap();
        assert_eq!(submit_count.load(Ordering::SeqCst), 1);
        assert!(proof1.verify(&verifying_key, &[0xABu8; 32]));

        let proof2 = relay.process_envelope(&envelope).await.unwrap();
        assert_eq!(submit_count.load(Ordering::SeqCst), 1); // no second call
        assert_eq!(proof1, proof2);
    }

    #[tokio::test]
    async fn test_multiple_identical_envelopes_within_window() {
        let submit_count = Arc::new(AtomicUsize::new(0));
        let mut relay = RelayNode::new(
            1000,
            Box::new(make_client(submit_count.clone())),
            create_signing_key(),
        );
        let envelope = create_test_envelope([1u8; 32]);

        let p1 = relay.process_envelope(&envelope).await.unwrap();
        let p2 = relay.process_envelope(&envelope).await.unwrap();
        let p3 = relay.process_envelope(&envelope).await.unwrap();

        assert_eq!(submit_count.load(Ordering::SeqCst), 1);
        assert_eq!(p1, p2);
        assert_eq!(p2, p3);
    }

    #[tokio::test]
    async fn test_different_envelopes_submit_separately() {
        let submit_count = Arc::new(AtomicUsize::new(0));
        let mut relay = RelayNode::new(
            1000,
            Box::new(make_client(submit_count.clone())),
            create_signing_key(),
        );

        relay
            .process_envelope(&create_test_envelope([1u8; 32]))
            .await
            .unwrap();
        relay
            .process_envelope(&create_test_envelope([2u8; 32]))
            .await
            .unwrap();

        assert_eq!(submit_count.load(Ordering::SeqCst), 2);
    }
}
