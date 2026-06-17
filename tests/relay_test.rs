use std::sync::atomic::{AtomicUsize, Ordering};

use ed25519_dalek::SigningKey;
use stellarconduit_core::message::signing::verify_signature;
use stellarconduit_core::message::types::TransactionEnvelope;
use stellarconduit_core::relay::RelayNode;
use stellarconduit_core::relay::StellarRpcClient;

struct MockRpcClient {
    submit_count: AtomicUsize,
    should_fail: bool,
    tx_hash: String,
    ledger_hash: String,
    ledger_sequence: u64,
}

impl MockRpcClient {
    fn new(tx_hash: &str) -> Self {
        Self {
            submit_count: AtomicUsize::new(0),
            should_fail: false,
            tx_hash: tx_hash.to_string(),
            ledger_hash: hex::encode([0xCD; 32]),
            ledger_sequence: 1234,
        }
    }

    fn failing() -> Self {
        Self {
            submit_count: AtomicUsize::new(0),
            should_fail: true,
            tx_hash: String::new(),
            ledger_hash: hex::encode([0xCD; 32]),
            ledger_sequence: 1234,
        }
    }
}

impl StellarRpcClient for MockRpcClient {
    fn submit_transaction(&self, _tx_xdr: &str) -> Result<String, String> {
        self.submit_count.fetch_add(1, Ordering::SeqCst);
        if self.should_fail {
            Err("RPC error".to_string())
        } else {
            Ok(self.tx_hash.clone())
        }
    }

    fn current_ledger_sequence(&self) -> Result<u64, String> {
        Ok(self.ledger_sequence)
    }

    fn current_ledger_hash(&self) -> Result<String, String> {
        Ok(self.ledger_hash.clone())
    }
}

fn create_envelope(origin: [u8; 32]) -> TransactionEnvelope {
    TransactionEnvelope {
        message_id: [1u8; 32],
        origin_pubkey: origin,
        tx_xdr: "AAAAAQAAAAAAAAAA".to_string(),
        ttl_hops: 10,
        timestamp: 1672531200,
        signature: [3u8; 64],
    }
}

fn relay_signing_key() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}

#[test]
fn test_process_envelope_success() {
    let tx_id = [0xAB; 32];
    let rpc_client = Box::new(MockRpcClient::new(&hex::encode(tx_id)));
    let signing_key = relay_signing_key();
    let verifying_key = signing_key.verifying_key();
    let mut relay = RelayNode::new(1000, rpc_client, signing_key);

    let envelope = create_envelope([2u8; 32]);
    let result = relay.process_envelope(&envelope);

    assert!(result.is_ok());
    let proof = result.unwrap();
    assert_eq!(proof.chain_hash, [0xCD; 32]);
    assert_eq!(proof.sequence, 1234);
    assert!(proof.verify(&verifying_key, &tx_id));
}

#[test]
fn test_process_envelope_rpc_failure() {
    let rpc_client = Box::new(MockRpcClient::failing());
    let mut relay = RelayNode::new(1000, rpc_client, relay_signing_key());

    let envelope = create_envelope([2u8; 32]);
    let result = relay.process_envelope(&envelope);

    assert!(result.is_err());
}

#[test]
fn test_process_envelope_deduplicates() {
    let tx_id = [0xAB; 32];
    let rpc_client = Box::new(MockRpcClient::new(&hex::encode(tx_id)));
    let mut relay = RelayNode::new(1000, rpc_client, relay_signing_key());

    let envelope = create_envelope([2u8; 32]);

    // First call submits
    let proof1 = relay.process_envelope(&envelope).unwrap();

    // Second call returns cached — RPC not called again
    let proof2 = relay.process_envelope(&envelope).unwrap();
    assert_eq!(proof2, proof1);
}

#[test]
fn test_verify_signature_standalone() {
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    let mut csprng = OsRng;
    let signing_key = SigningKey::generate(&mut csprng);
    let verifying_key = signing_key.verifying_key();

    let mut envelope = TransactionEnvelope {
        message_id: [1u8; 32],
        origin_pubkey: verifying_key.to_bytes(),
        tx_xdr: "AAAAAQAAAAAAAAAA".to_string(),
        ttl_hops: 10,
        timestamp: 1672531200,
        signature: [0u8; 64],
    };

    stellarconduit_core::message::signing::sign_envelope(&signing_key, &mut envelope)
        .expect("Failed to sign envelope");

    let result = verify_signature(&envelope);
    assert!(result.is_ok());
    assert!(result.unwrap());
}
