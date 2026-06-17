use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use stellarconduit_core::message::{
    signing::{sign_envelope, verify_signature},
    types::{ProtocolMessage, SyncRequest, TopologyUpdate, TransactionEnvelope},
    RelayChainProof,
};

fn make_test_envelope(keypair: &SigningKey, tx_xdr: &str) -> TransactionEnvelope {
    TransactionEnvelope {
        message_id: [0u8; 32],
        origin_pubkey: keypair.verifying_key().to_bytes(),
        tx_xdr: tx_xdr.to_string(),
        ttl_hops: 10,
        timestamp: 1672531200,
        signature: [0u8; 64],
    }
}

#[test]
fn test_sign_then_verify_passes() {
    let mut csprng = OsRng;
    let keypair = SigningKey::generate(&mut csprng);
    let mut envelope = make_test_envelope(&keypair, "AAAAAQAAAAAAAAAA");

    sign_envelope(&keypair, &mut envelope).expect("signing should succeed");
    let result = verify_signature(&envelope).expect("verification should succeed");
    assert!(result, "signature should be valid");
}

#[test]
fn test_verify_tampered_tx_xdr() {
    let mut csprng = OsRng;
    let keypair = SigningKey::generate(&mut csprng);
    let mut envelope = make_test_envelope(&keypair, "AAAAAQAAAAAAAAAA");

    sign_envelope(&keypair, &mut envelope).expect("signing should succeed");

    // Tamper with the payload after signing
    envelope.tx_xdr = "TAMPERED_PAYLOAD_XDR".to_string();

    let result = verify_signature(&envelope);
    assert!(
        result.is_err(),
        "verification should fail due to tampered tx_xdr"
    );
}

#[test]
fn test_verify_tampered_timestamp() {
    let mut csprng = OsRng;
    let keypair = SigningKey::generate(&mut csprng);
    let mut envelope = make_test_envelope(&keypair, "AAAAAQAAAAAAAAAA");

    sign_envelope(&keypair, &mut envelope).expect("signing should succeed");

    // Tamper with the timestamp after signing
    envelope.timestamp += 1;

    let result = verify_signature(&envelope);
    assert!(
        result.is_err(),
        "verification should fail due to tampered timestamp"
    );
}

#[test]
fn test_verify_with_wrong_key_fails() {
    let mut csprng = OsRng;
    let keypair_a = SigningKey::generate(&mut csprng);
    let keypair_b = SigningKey::generate(&mut csprng);

    let mut envelope = make_test_envelope(&keypair_a, "AAAAAQAAAAAAAAAA");
    // Sign with key A but put key B's pubkey as origin
    sign_envelope(&keypair_a, &mut envelope).expect("signing should succeed");
    // Swap the pubkey to a different one
    envelope.origin_pubkey = keypair_b.verifying_key().to_bytes();

    let result = verify_signature(&envelope);
    assert!(
        result.is_err(),
        "verification should fail when origin_pubkey doesn't match signing key"
    );
}

#[test]
fn test_verify_invalid_signature() {
    let mut csprng = OsRng;
    let keypair = SigningKey::generate(&mut csprng);
    let mut envelope = make_test_envelope(&keypair, "AAAAAQAAAAAAAAAA");

    sign_envelope(&keypair, &mut envelope).expect("signing should succeed");

    // Corrupt the signature bytes
    envelope.signature[0] ^= 0xFF;
    envelope.signature[1] ^= 0xFF;

    let result = verify_signature(&envelope);
    assert!(
        result.is_err(),
        "verification should fail with corrupted signature bytes"
    );
}

// ─── Serialization Round-Trip Tests ──────────────────────────────────────────

#[test]
fn test_relay_chain_proof_sign_then_verify_passes() {
    let mut csprng = OsRng;
    let signing_key = SigningKey::generate(&mut csprng);
    let verifying_key = signing_key.verifying_key();
    let tx_id = [0xAB; 32];
    let chain_hash = [0xCD; 32];
    let sequence = 1234;

    let proof = RelayChainProof::sign(&signing_key, &tx_id, &chain_hash, sequence);

    assert_eq!(proof.chain_hash, chain_hash);
    assert_eq!(proof.sequence, sequence);
    assert!(proof.verify(&verifying_key, &tx_id));
}

#[test]
fn test_relay_chain_proof_rejects_wrong_tx_id() {
    let mut csprng = OsRng;
    let signing_key = SigningKey::generate(&mut csprng);
    let verifying_key = signing_key.verifying_key();
    let proof = RelayChainProof::sign(&signing_key, &[0xAB; 32], &[0xCD; 32], 1234);

    assert!(!proof.verify(&verifying_key, &[0xEF; 32]));
}

#[test]
fn test_relay_chain_proof_rejects_wrong_key() {
    let mut csprng = OsRng;
    let signing_key = SigningKey::generate(&mut csprng);
    let wrong_key = SigningKey::generate(&mut csprng);
    let proof = RelayChainProof::sign(&signing_key, &[0xAB; 32], &[0xCD; 32], 1234);

    assert!(!proof.verify(&wrong_key.verifying_key(), &[0xAB; 32]));
}

#[test]
fn test_relay_chain_proof_rejects_corrupted_signature() {
    let mut csprng = OsRng;
    let signing_key = SigningKey::generate(&mut csprng);
    let verifying_key = signing_key.verifying_key();
    let mut proof = RelayChainProof::sign(&signing_key, &[0xAB; 32], &[0xCD; 32], 1234);

    proof.signature[0] ^= 0xFF;

    assert!(!proof.verify(&verifying_key, &[0xAB; 32]));
}

#[test]
fn test_relay_chain_proof_rejects_corrupted_chain_hash() {
    let mut csprng = OsRng;
    let signing_key = SigningKey::generate(&mut csprng);
    let verifying_key = signing_key.verifying_key();
    let mut proof = RelayChainProof::sign(&signing_key, &[0xAB; 32], &[0xCD; 32], 1234);

    proof.chain_hash[0] ^= 0xFF;

    assert!(!proof.verify(&verifying_key, &[0xAB; 32]));
}

#[test]
fn test_relay_chain_proof_rejects_corrupted_sequence() {
    let mut csprng = OsRng;
    let signing_key = SigningKey::generate(&mut csprng);
    let verifying_key = signing_key.verifying_key();
    let mut proof = RelayChainProof::sign(&signing_key, &[0xAB; 32], &[0xCD; 32], 1234);

    proof.sequence += 1;

    assert!(!proof.verify(&verifying_key, &[0xAB; 32]));
}

#[test]
fn test_relay_chain_proof_msgpack_roundtrip() {
    let mut csprng = OsRng;
    let signing_key = SigningKey::generate(&mut csprng);
    let proof = RelayChainProof::sign(&signing_key, &[0xAB; 32], &[0xCD; 32], 1234);

    let bytes = rmp_serde::to_vec(&proof).expect("proof should serialize");
    let decoded: RelayChainProof = rmp_serde::from_slice(&bytes).expect("proof should deserialize");

    assert_eq!(decoded, proof);
}

#[test]
fn test_transaction_envelope_roundtrip() {
    let mut csprng = OsRng;
    let keypair = SigningKey::generate(&mut csprng);
    let mut envelope = make_test_envelope(&keypair, "AAAAAQAAAAAAAAAA");
    sign_envelope(&keypair, &mut envelope).expect("signing should succeed");

    let msg = ProtocolMessage::Transaction(envelope);
    let bytes = msg.to_bytes().expect("Failed to serialize");
    let decoded = ProtocolMessage::from_bytes(&bytes).expect("Failed to deserialize");

    assert_eq!(msg, decoded);
}

#[test]
fn test_topology_update_roundtrip() {
    let update = TopologyUpdate {
        origin_pubkey: [7u8; 32],
        directly_connected_peers: vec![[1u8; 32], [2u8; 32]],
        hops_to_relay: 2,
        topology_flags: vec![],
    };
    let msg = ProtocolMessage::TopologyUpdate(update);

    let bytes = msg.to_bytes().expect("Failed to serialize");
    let decoded = ProtocolMessage::from_bytes(&bytes).expect("Failed to deserialize");

    assert_eq!(msg, decoded);
}

#[test]
fn test_sync_request_roundtrip() {
    let req = SyncRequest {
        known_message_ids: vec![[1u8; 4], [2u8; 4]],
    };
    let msg = ProtocolMessage::SyncRequest(req);

    let bytes = msg.to_bytes().expect("Failed to serialize");
    let decoded = ProtocolMessage::from_bytes(&bytes).expect("Failed to deserialize");

    assert_eq!(msg, decoded);
}

// ─── Size Budget Test ────────────────────────────────────────────────────────

#[test]
fn test_envelope_serialized_size_under_budget() {
    let mut csprng = OsRng;
    let keypair = SigningKey::generate(&mut csprng);

    // Create a mock XDR string of around 300 bytes. Base64 is ~4 chars per 3 bytes.
    // 280 bytes of raw data = ~373 chars of base64. Let's make a ~280 character string.
    let mock_xdr = "AAAAAgAAAADZ/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9".to_string();

    let mut envelope = make_test_envelope(&keypair, &mock_xdr);
    sign_envelope(&keypair, &mut envelope).expect("signing should succeed");

    let msg = ProtocolMessage::Transaction(envelope);
    let bytes = msg.to_bytes().expect("Failed to serialize");

    // The requirement is that a realistic envelope serialization should be < 500 bytes.
    assert!(
        bytes.len() < 500,
        "Serialized envelope size {} must be under 500 bytes",
        bytes.len()
    );
}
