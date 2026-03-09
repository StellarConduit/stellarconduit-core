use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use stellarconduit_core::message::signing::sign_envelope;
use stellarconduit_core::message::types::TransactionEnvelope;
use stellarconduit_core::relay::process_envelope;
use stellarconduit_core::relay::rpc_client::RpcClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    // Initialize the RPC client with Stellar testnet endpoint
    let rpc_client = RpcClient::new("https://soroban-testnet.stellar.org");

    // Simulate receiving a transaction envelope from the mesh
    // In production, this would come from the gossip protocol
    let mut csprng = OsRng;
    let signing_key = SigningKey::generate(&mut csprng);
    let verifying_key = signing_key.verifying_key();

    // Create a mock transaction envelope
    // In production, this would be a real Stellar XDR transaction
    let mut envelope = TransactionEnvelope {
        message_id: [1u8; 32],
        origin_pubkey: verifying_key.to_bytes(),
        tx_xdr: "AAAAAgAAAADZ/7+9/7+9/7+9EXAMPLE_XDR".to_string(),
        ttl_hops: 10,
        timestamp: chrono::Utc::now().timestamp() as u64,
        signature: [0u8; 64],
    };

    // Sign the envelope
    sign_envelope(&signing_key, &mut envelope)?;

    println!("Processing transaction envelope...");
    println!("Origin: {:?}", hex::encode(&envelope.origin_pubkey[..8]));
    println!("Timestamp: {}", envelope.timestamp);

    // Process the envelope: verify signature and submit to Stellar
    match process_envelope(&rpc_client, &envelope).await {
        Ok(tx_hash) => {
            println!("✓ Transaction submitted successfully!");
            println!("Transaction hash: {}", tx_hash);
        }
        Err(e) => {
            eprintln!("✗ Failed to process envelope: {}", e);
        }
    }

    Ok(())
}
