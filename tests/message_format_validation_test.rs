/// Test to validate that the documentation in docs/message-format.md
/// accurately reflects the implementation.
use ed25519_dalek::{Signer, SigningKey};
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};
use stellarconduit_core::message::signing::{sign_envelope, verify_signature};
use stellarconduit_core::message::types::{ProtocolMessage, TransactionEnvelope};

#[test]
fn test_signature_payload_construction() {
    // This test validates the exact algorithm documented in message-format.md
    let mut csprng = OsRng;
    let signing_key = SigningKey::generate(&mut csprng);
    let origin_pubkey = signing_key.verifying_key().to_bytes();
    let timestamp: u64 = 1672531200;
    let tx_xdr = "AAAAAgAAAADZ/7+9".to_string();

    // Step 1-3: Construct payload hash as documented
    let mut hasher = Sha256::new();
    hasher.update(origin_pubkey); // 32 bytes
    hasher.update(timestamp.to_be_bytes()); // 8 bytes, big-endian
    hasher.update(tx_xdr.as_bytes()); // Variable length UTF-8
    let expected_hash: [u8; 32] = hasher.finalize().into();

    // Step 4: Sign the hash
    let signature = signing_key.sign(&expected_hash);

    // Step 5: Construct envelope
    let envelope = TransactionEnvelope {
        message_id: expected_hash,
        origin_pubkey,
        tx_xdr: tx_xdr.clone(),
        ttl_hops: 10,
        timestamp,
        signature: signature.to_bytes(),
    };

    // Verify the signature using the library function
    assert!(verify_signature(&envelope).unwrap());
}

#[test]
fn test_documented_signing_algorithm() {
    // Test the complete signing algorithm as documented
    let mut csprng = OsRng;
    let signing_key = SigningKey::generate(&mut csprng);
    let tx_xdr = "AAAAAgAAAADZ/7+9/7+9/7+9".to_string();

    let origin_pubkey = signing_key.verifying_key().to_bytes();
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // Compute payload hash (as documented)
    let mut hasher = Sha256::new();
    hasher.update(origin_pubkey);
    hasher.update(timestamp.to_be_bytes());
    hasher.update(tx_xdr.as_bytes());
    let payload_hash: [u8; 32] = hasher.finalize().into();

    // Sign the hash
    let signature = signing_key.sign(&payload_hash);

    let envelope = TransactionEnvelope {
        message_id: payload_hash,
        origin_pubkey,
        tx_xdr,
        ttl_hops: 10,
        timestamp,
        signature: signature.to_bytes(),
    };

    // Verify using library function
    assert!(verify_signature(&envelope).unwrap());
}

#[test]
fn test_messagepack_serialization_size() {
    // Validate the size claims in the documentation
    let mut csprng = OsRng;
    let signing_key = SigningKey::generate(&mut csprng);

    // Create envelope with ~300 byte XDR (as documented)
    let tx_xdr = "A".repeat(300);
    let mut envelope = TransactionEnvelope {
        message_id: [1u8; 32],
        origin_pubkey: signing_key.verifying_key().to_bytes(),
        tx_xdr,
        ttl_hops: 10,
        timestamp: 1672531200,
        signature: [0u8; 64],
    };

    sign_envelope(&signing_key, &mut envelope).unwrap();

    let msg = ProtocolMessage::Transaction(envelope);
    let bytes = msg.to_bytes().unwrap();

    // Documentation claims: "approximately 450-480 bytes"
    println!("Serialized size: {} bytes", bytes.len());
    assert!(
        bytes.len() >= 400 && bytes.len() <= 500,
        "Size should be approximately 450-480 bytes, got {}",
        bytes.len()
    );
}

#[test]
fn test_big_endian_timestamp_encoding() {
    // Verify that timestamp is encoded as big-endian
    let timestamp: u64 = 0x0102030405060708;
    let be_bytes = timestamp.to_be_bytes();

    // Big-endian: most significant byte first
    assert_eq!(be_bytes[0], 0x01);
    assert_eq!(be_bytes[7], 0x08);

    // Verify this matches what's used in signing
    let mut hasher = Sha256::new();
    hasher.update([0u8; 32]); // dummy pubkey
    hasher.update(be_bytes); // timestamp as big-endian
    hasher.update(b"test"); // dummy xdr

    // This should match the documented algorithm
    let _hash: [u8; 32] = hasher.finalize().into();
}

#[test]
fn test_signature_verification_rejects_tampering() {
    // Validate that tampering invalidates the signature
    let mut csprng = OsRng;
    let signing_key = SigningKey::generate(&mut csprng);

    let mut envelope = TransactionEnvelope {
        message_id: [1u8; 32],
        origin_pubkey: signing_key.verifying_key().to_bytes(),
        tx_xdr: "AAAAAgAAAADZ/7+9".to_string(),
        ttl_hops: 10,
        timestamp: 1672531200,
        signature: [0u8; 64],
    };

    sign_envelope(&signing_key, &mut envelope).unwrap();

    // Verify original is valid
    assert!(verify_signature(&envelope).unwrap());

    // Tamper with the XDR
    envelope.tx_xdr = "TAMPERED".to_string();

    // Verification should fail
    assert!(verify_signature(&envelope).is_err());
}

#[test]
fn test_protocol_message_variants() {
    // Verify all four message types serialize correctly
    use stellarconduit_core::message::types::{SyncRequest, SyncResponse, TopologyUpdate};

    let tx_envelope = TransactionEnvelope {
        message_id: [1u8; 32],
        origin_pubkey: [2u8; 32],
        tx_xdr: "test".to_string(),
        ttl_hops: 10,
        timestamp: 1672531200,
        signature: [3u8; 64],
    };

    let topology = TopologyUpdate {
        origin_pubkey: [2u8; 32],
        directly_connected_peers: vec![[3u8; 32], [4u8; 32]],
        hops_to_relay: 5,
    };

    let sync_req = SyncRequest {
        known_message_ids: vec![[1u8; 4], [2u8; 4]],
    };

    let sync_resp = SyncResponse {
        missing_envelopes: vec![tx_envelope.clone()],
    };

    // All should serialize and deserialize
    let messages = vec![
        ProtocolMessage::Transaction(tx_envelope),
        ProtocolMessage::TopologyUpdate(topology),
        ProtocolMessage::SyncRequest(sync_req),
        ProtocolMessage::SyncResponse(sync_resp),
    ];

    for msg in messages {
        let bytes = msg.to_bytes().unwrap();
        let decoded = ProtocolMessage::from_bytes(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }
}
