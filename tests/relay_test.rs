use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use stellarconduit_core::message::signing::{sign_envelope, verify_signature};
use stellarconduit_core::message::types::TransactionEnvelope;
use stellarconduit_core::relay::process_envelope;
use stellarconduit_core::relay::rpc_client::RpcClient;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn create_valid_envelope() -> (TransactionEnvelope, SigningKey) {
    let mut csprng = OsRng;
    let signing_key = SigningKey::generate(&mut csprng);
    let verifying_key = signing_key.verifying_key();

    let mut envelope = TransactionEnvelope {
        message_id: [1u8; 32],
        origin_pubkey: verifying_key.to_bytes(),
        tx_xdr: "AAAAAgAAAADZ/7+9/7+9/7+9".to_string(),
        ttl_hops: 10,
        timestamp: 1672531200,
        signature: [0u8; 64],
    };

    sign_envelope(&signing_key, &mut envelope).expect("Failed to sign envelope");

    (envelope, signing_key)
}

#[tokio::test]
async fn test_process_envelope_success() {
    let mock_server = MockServer::start().await;

    let mock_response = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "status": "PENDING",
            "hash": "test_tx_hash_123"
        }
    });

    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mock_response))
        .mount(&mock_server)
        .await;

    let rpc_client = RpcClient::new(mock_server.uri());
    let (envelope, _) = create_valid_envelope();

    let result = process_envelope(&rpc_client, &envelope).await;

    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "test_tx_hash_123");
}

#[tokio::test]
async fn test_process_envelope_invalid_signature() {
    let mock_server = MockServer::start().await;
    let rpc_client = RpcClient::new(mock_server.uri());

    // Create an envelope with an invalid signature
    let mut envelope = TransactionEnvelope {
        message_id: [1u8; 32],
        origin_pubkey: [2u8; 32],
        tx_xdr: "AAAAAgAAAADZ/7+9/7+9/7+9".to_string(),
        ttl_hops: 10,
        timestamp: 1672531200,
        signature: [99u8; 64], // Invalid signature
    };

    // Tamper with the signature to make it invalid
    envelope.signature[0] = 0;

    let result = process_envelope(&rpc_client, &envelope).await;

    assert!(result.is_err());
    // The RPC endpoint should never be called for invalid signatures
}

#[tokio::test]
async fn test_process_envelope_rpc_failure() {
    let mock_server = MockServer::start().await;

    let mock_response = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "error": {
            "code": -32000,
            "message": "Transaction validation failed"
        }
    });

    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mock_response))
        .mount(&mock_server)
        .await;

    let rpc_client = RpcClient::new(mock_server.uri());
    let (envelope, _) = create_valid_envelope();

    let result = process_envelope(&rpc_client, &envelope).await;

    assert!(result.is_err());
}

#[tokio::test]
async fn test_verify_signature_before_rpc_call() {
    let mock_server = MockServer::start().await;

    // This mock should never be called because signature verification fails first
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "status": "PENDING",
                "hash": "should_not_reach_here"
            }
        })))
        .expect(0) // Expect zero calls
        .mount(&mock_server)
        .await;

    let rpc_client = RpcClient::new(mock_server.uri());

    // Create envelope with forged signature
    let envelope = TransactionEnvelope {
        message_id: [1u8; 32],
        origin_pubkey: [2u8; 32],
        tx_xdr: "AAAAAgAAAADZ/7+9/7+9/7+9".to_string(),
        ttl_hops: 10,
        timestamp: 1672531200,
        signature: [3u8; 64],
    };

    let result = process_envelope(&rpc_client, &envelope).await;

    assert!(result.is_err());
    // Mock server will verify that no HTTP calls were made
}

#[test]
fn test_signature_verification_standalone() {
    let (envelope, _) = create_valid_envelope();

    let result = verify_signature(&envelope);

    assert!(result.is_ok());
    assert_eq!(result.unwrap(), true);
}
