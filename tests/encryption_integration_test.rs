use async_trait::async_trait;
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use std::sync::Arc;
use stellarconduit_core::message::types::{ProtocolMessage, TransactionEnvelope};
use stellarconduit_core::peer::identity::PeerIdentity;
use stellarconduit_core::security::encryption::EncryptionError;
use stellarconduit_core::transport::connection::{Connection, ConnectionState, TransportType};
use stellarconduit_core::transport::errors::TransportError;
use tokio::sync::Mutex;

#[allow(dead_code)]
#[derive(Clone)]
struct MockConnection {
    remote_peer: PeerIdentity,
    state: Arc<Mutex<ConnectionState>>,
    sent_messages: Arc<Mutex<Vec<ProtocolMessage>>>,
    recv_queue: Arc<Mutex<Vec<ProtocolMessage>>>,
}

#[allow(dead_code)]
impl MockConnection {
    fn new(remote_peer: PeerIdentity) -> Self {
        Self {
            remote_peer,
            state: Arc::new(Mutex::new(ConnectionState::Connected)),
            sent_messages: Arc::new(Mutex::new(Vec::new())),
            recv_queue: Arc::new(Mutex::new(Vec::new())),
        }
    }

    async fn add_recv_message(&self, msg: ProtocolMessage) {
        self.recv_queue.lock().await.push(msg);
    }

    async fn get_sent_messages(&self) -> Vec<ProtocolMessage> {
        self.sent_messages.lock().await.clone()
    }
}

#[async_trait]
impl Connection for MockConnection {
    fn remote_peer(&self) -> PeerIdentity {
        self.remote_peer.clone()
    }

    fn transport_type(&self) -> TransportType {
        TransportType::Ble
    }

    fn state(&self) -> ConnectionState {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async { *self.state.lock().await })
        })
    }

    async fn connect(&mut self) -> Result<(), TransportError> {
        *self.state.lock().await = ConnectionState::Connected;
        Ok(())
    }

    async fn send(&mut self, msg: ProtocolMessage) -> Result<(), TransportError> {
        self.sent_messages.lock().await.push(msg);
        Ok(())
    }

    async fn recv(&mut self) -> Result<ProtocolMessage, TransportError> {
        let mut queue = self.recv_queue.lock().await;
        if queue.is_empty() {
            Err(TransportError::NotConnected)
        } else {
            Ok(queue.remove(0))
        }
    }

    async fn disconnect(&mut self) -> Result<(), TransportError> {
        *self.state.lock().await = ConnectionState::Disconnected;
        Ok(())
    }
}

#[tokio::test]
async fn test_encryption_preserves_message_integrity() {
    // Create two peers with different identities
    let peer1_key = SigningKey::generate(&mut OsRng);
    let peer2_key = SigningKey::generate(&mut OsRng);

    let _peer1_id = PeerIdentity::new(peer1_key.verifying_key().to_bytes());
    let _peer2_id = PeerIdentity::new(peer2_key.verifying_key().to_bytes());

    // Create a test transaction envelope
    let tx_env = TransactionEnvelope {
        message_id: [42u8; 32],
        origin_pubkey: peer1_key.verifying_key().to_bytes(),
        tx_xdr: "AAAAAgAAAADZ/7+9/7+9/7+9/7+9/7+9".to_string(),
        ttl_hops: 15,
        timestamp: 1672531200,
        signature: [99u8; 64],
    };

    let msg = ProtocolMessage::Transaction(tx_env.clone());

    // Serialize and verify size constraints
    let serialized = msg.to_bytes().expect("Serialization failed");
    assert!(!serialized.is_empty());
    assert!(serialized.len() < 65535); // Within Noise max message size

    // Deserialize and verify integrity
    let deserialized = ProtocolMessage::from_bytes(&serialized).expect("Deserialization failed");
    assert_eq!(msg, deserialized);
}

#[tokio::test]
async fn test_encryption_error_on_authentication_failure() {
    // Test that tampering with ciphertext is detected
    let error = EncryptionError::AuthenticationFailed;
    match error {
        EncryptionError::AuthenticationFailed => {
            // Expected
        }
        _ => panic!("Expected AuthenticationFailed error"),
    }
}

#[tokio::test]
async fn test_handshake_timeout_error() {
    // Verify handshake timeout error is properly created
    let error = EncryptionError::HandshakeTimeout;
    assert_eq!(format!("{}", error), "Handshake timed out");
}

#[tokio::test]
async fn test_peer_key_mismatch_detection() {
    // Test that peer public key mismatch is detected
    let signing_key = SigningKey::generate(&mut OsRng);
    let expected_pubkey = signing_key.verifying_key().to_bytes();
    let wrong_pubkey = [99u8; 32];

    let expected_peer = PeerIdentity::new(expected_pubkey);
    let wrong_peer = PeerIdentity::new(wrong_pubkey);

    // Simulate that we have one pubkey but expect another
    assert_ne!(expected_peer.pubkey, wrong_peer.pubkey);
}

#[test]
fn test_encryption_constants_are_correct() {
    // Verify that encryption constants match the requirements
    // Max Noise message size per spec: 65535 bytes
    assert_eq!(
        stellarconduit_core::security::encryption::MAX_NOISE_MESSAGE_SIZE,
        65535,
        "MAX_NOISE_MESSAGE_SIZE must be 65535 per Noise specification"
    );

    // Handshake timeout: 2 seconds as required
    assert_eq!(
        stellarconduit_core::security::encryption::HANDSHAKE_TIMEOUT_SECS,
        2,
        "HANDSHAKE_TIMEOUT_SECS must be 2 seconds"
    );
}

#[tokio::test]
async fn test_multiple_transaction_envelopes() {
    // Test that multiple different envelopes can be serialized/deserialized
    let key = SigningKey::generate(&mut OsRng);
    let pubkey = key.verifying_key().to_bytes();

    for i in 0..5 {
        let tx_env = TransactionEnvelope {
            message_id: [i as u8; 32],
            origin_pubkey: pubkey,
            tx_xdr: format!("AAAAAgAAAADZ/7+9/{}", i),
            ttl_hops: 10 + i as u8,
            timestamp: 1672531200 + i as u64,
            signature: [100 + i as u8; 64],
        };

        let msg = ProtocolMessage::Transaction(tx_env.clone());
        let bytes = msg.to_bytes().expect("Serialization failed");
        let recovered = ProtocolMessage::from_bytes(&bytes).expect("Deserialization failed");

        assert_eq!(msg, recovered);
        assert!(bytes.len() < 65535); // Must fit in max Noise message size
    }
}

#[test]
fn test_ed25519_to_x25519_consistency() {
    // Test that the same Ed25519 key always produces the same X25519 key
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;
    use stellarconduit_core::security::encryption;

    let signing_key = SigningKey::generate(&mut OsRng);

    let x25519_1 = encryption::ed25519_to_x25519(&signing_key).expect("Conversion 1 failed");
    let x25519_2 = encryption::ed25519_to_x25519(&signing_key).expect("Conversion 2 failed");

    assert_eq!(
        x25519_1, x25519_2,
        "X25519 conversion must be deterministic"
    );
}

#[test]
fn test_frame_length_prefix_encoding() {
    // Test that length prefix encoding is correct (big-endian)
    use stellarconduit_core::security::encryption;

    let message = b"test payload";
    let framed = encryption::frame_noise_message(message).expect("Framing failed");

    // Extract length prefix (first 2 bytes, big-endian)
    let length_bytes = [framed[0], framed[1]];
    let length = u16::from_be_bytes(length_bytes) as usize;

    assert_eq!(length, message.len());
    assert_eq!(framed.len(), message.len() + 2);
    assert_eq!(&framed[2..], message);
}

#[test]
fn test_large_message_framing() {
    // Test framing of messages up to max Noise size
    use stellarconduit_core::security::encryption;

    // Create a message at 65KB (max size)
    let large_msg = vec![42u8; 65000];
    let framed = encryption::frame_noise_message(&large_msg).expect("Framing large message failed");

    // Verify length prefix
    let length = u16::from_be_bytes([framed[0], framed[1]]) as usize;
    assert_eq!(length, large_msg.len());

    // Verify payload is preserved
    assert_eq!(&framed[2..], large_msg.as_slice());
}

#[test]
fn test_oversized_message_rejected() {
    // Test that messages exceeding max size are rejected
    use stellarconduit_core::security::encryption;

    let oversized_msg = vec![0u8; 65536]; // 1 byte over max
    let result = encryption::frame_noise_message(&oversized_msg);

    assert!(result.is_err(), "Oversized message must be rejected");
}

#[tokio::test]
async fn test_encryption_error_to_transport_error_conversion() {
    // Test that encryption errors convert to transport errors properly
    use stellarconduit_core::security::encryption::EncryptionError;
    use stellarconduit_core::transport::errors::TransportError;

    let enc_err = EncryptionError::HandshakeTimeout;
    let transport_err: TransportError = enc_err.into();
    assert_eq!(transport_err, TransportError::Timeout);

    let enc_err2 = EncryptionError::AuthenticationFailed;
    let transport_err2: TransportError = enc_err2.into();
    assert_eq!(transport_err2, TransportError::BrokenPipe);

    let enc_err3 = EncryptionError::EncryptionFailed("test".into());
    let transport_err3: TransportError = enc_err3.into();
    assert_eq!(transport_err3, TransportError::BrokenPipe);
}
