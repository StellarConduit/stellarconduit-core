use ed25519_dalek::SigningKey;
use stellarconduit_core::{
    message::{types::TransactionEnvelope, RelayChainProof},
    peer::peer_node::Peer,
    persistence::db::MeshDatabase,
};

fn create_mock_envelope(id: u8) -> TransactionEnvelope {
    TransactionEnvelope {
        message_id: [id; 32],
        origin_pubkey: [2u8; 32],
        tx_xdr: format!("XDR_PAYLOAD_{}", id),
        ttl_hops: 10,
        timestamp: 123456789,
        signature: [3u8; 64],
    }
}

fn create_mock_peer(id: u8) -> Peer {
    let mut peer = Peer::new([id; 32]);
    peer.reputation = 90;
    peer.last_seen_unix_sec = 987654321;
    peer.is_banned = false;
    peer.supported_transports = 3; // BLE + WiFi
    peer.is_relay_node = true;
    peer.bytes_sent = 1024;
    peer.bytes_received = 2048;
    peer
}

fn create_relay_proof() -> (SigningKey, [u8; 32], RelayChainProof) {
    let signing_key = SigningKey::from_bytes(&[7u8; 32]);
    let tx_id = [0xAB; 32];
    let proof = RelayChainProof::sign(&signing_key, &tx_id, &[0xCD; 32], 1234);
    (signing_key, tx_id, proof)
}

#[tokio::test]
async fn test_db_init_creates_tables() {
    let db = MeshDatabase::init(":memory:")
        .await
        .expect("Failed to init DB");
    // If it initialized without error, tables were created.
    assert!(db.load_all_peers().await.is_ok());
    assert!(db.load_pending_envelopes().await.is_ok());
}

#[tokio::test]
async fn test_save_and_load_peer() {
    let db = MeshDatabase::init(":memory:").await.unwrap();
    let peer = create_mock_peer(42);

    db.save_peer(&peer).await.expect("Failed to save peer");

    let loaded_peers = db.load_all_peers().await.expect("Failed to load peers");
    assert_eq!(loaded_peers.len(), 1);

    let loaded = &loaded_peers[0];
    assert_eq!(loaded.identity.pubkey, peer.identity.pubkey);
    assert_eq!(loaded.reputation, peer.reputation);
    assert_eq!(loaded.last_seen_unix_sec, peer.last_seen_unix_sec);
    assert_eq!(loaded.is_banned, peer.is_banned);
    assert_eq!(loaded.supported_transports, peer.supported_transports);
    assert_eq!(loaded.is_relay_node, peer.is_relay_node);
    assert_eq!(loaded.bytes_sent, peer.bytes_sent);
    assert_eq!(loaded.bytes_received, peer.bytes_received);
}

#[tokio::test]
async fn test_upsert_peer() {
    let db = MeshDatabase::init(":memory:").await.unwrap();
    let mut peer = create_mock_peer(42);
    db.save_peer(&peer).await.unwrap();

    // Modify and update
    peer.reputation = 50;
    peer.is_banned = true;
    db.save_peer(&peer).await.unwrap();

    let loaded_peers = db.load_all_peers().await.unwrap();
    assert_eq!(loaded_peers.len(), 1, "Should upsert, not insert a new row");

    let loaded = &loaded_peers[0];
    assert_eq!(loaded.reputation, 50);
    assert!(loaded.is_banned);
}

#[tokio::test]
async fn test_save_and_load_envelope() {
    let db = MeshDatabase::init(":memory:").await.unwrap();
    let env = create_mock_envelope(99);

    db.save_envelope(&env)
        .await
        .expect("Failed to save envelope");

    let loaded_envs = db
        .load_pending_envelopes()
        .await
        .expect("Failed to load envelopes");
    assert_eq!(loaded_envs.len(), 1);

    let loaded = &loaded_envs[0];
    assert_eq!(loaded.message_id, env.message_id);
    assert_eq!(loaded.tx_xdr, env.tx_xdr);
    assert_eq!(loaded.ttl_hops, env.ttl_hops);
    assert_eq!(loaded.timestamp, env.timestamp);
    assert_eq!(loaded.signature, env.signature);
}

#[tokio::test]
async fn test_delete_envelope() {
    let db = MeshDatabase::init(":memory:").await.unwrap();
    let env_1 = create_mock_envelope(1);
    let env_2 = create_mock_envelope(2);

    db.save_envelope(&env_1).await.unwrap();
    db.save_envelope(&env_2).await.unwrap();

    let loaded = db.load_pending_envelopes().await.unwrap();
    assert_eq!(loaded.len(), 2);

    // Delete envelope 1
    db.delete_envelope(&env_1.message_id)
        .await
        .expect("Failed to delete");

    let loaded_after = db.load_pending_envelopes().await.unwrap();
    assert_eq!(loaded_after.len(), 1);
    assert_eq!(loaded_after[0].message_id, env_2.message_id);
}

#[tokio::test]
async fn test_save_and_get_relay_proof() {
    let db = MeshDatabase::init(":memory:").await.unwrap();
    let (signing_key, tx_id, proof) = create_relay_proof();

    db.save_relay_proof(&tx_id, &proof)
        .await
        .expect("Failed to save relay proof");

    let loaded = db
        .get_relay_proof(&tx_id)
        .await
        .expect("Failed to load relay proof")
        .expect("relay proof should exist");

    assert_eq!(loaded, proof);
    assert!(loaded.verify(&signing_key.verifying_key(), &tx_id));
}

#[tokio::test]
async fn test_get_relay_proof_missing_returns_none() {
    let db = MeshDatabase::init(":memory:").await.unwrap();

    let missing = db
        .get_relay_proof(&[0xEF; 32])
        .await
        .expect("missing relay proof lookup should not fail");

    assert_eq!(missing, None);
}
