use async_trait::async_trait;
use stellarconduit_core::message::types::TransactionEnvelope;
use stellarconduit_core::metrics::Metrics;
use stellarconduit_core::relay::{RelayNode, RpcError, StellarRpcClient};

struct ExampleRpcClient;

#[async_trait]
impl StellarRpcClient for ExampleRpcClient {
    async fn submit_transaction(&self, tx_xdr: &str) -> Result<String, RpcError> {
        println!(
            "Submitting transaction: {}...",
            &tx_xdr[..20.min(tx_xdr.len())]
        );
        Ok(hex::encode([0xABu8; 32]))
    }
    async fn get_account_sequence(&self, _: &str) -> Result<u64, RpcError> {
        Ok(0)
    }
    async fn get_ledger_sequence(&self) -> Result<u64, RpcError> {
        Ok(1234)
    }
    async fn get_ledger_hash(&self) -> Result<String, RpcError> {
        Ok(hex::encode([0xCDu8; 32]))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    let seed_hex = std::env::var("SC_RELAY_SIGNING_KEY_HEX").map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "set SC_RELAY_SIGNING_KEY_HEX to a 32-byte hex relay signing seed",
        )
    })?;
    let seed_bytes = hex::decode(seed_hex)?;
    let seed: [u8; 32] = seed_bytes.try_into().map_err(|bytes: Vec<u8>| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "SC_RELAY_SIGNING_KEY_HEX must decode to 32 bytes, got {}",
                bytes.len()
            ),
        )
    })?;
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&seed);
    let verifying_key = signing_key.verifying_key();
    let mut relay = RelayNode::new(
        1000,
        Box::new(ExampleRpcClient),
        signing_key,
        Metrics::new(),
    );

    let envelope = TransactionEnvelope {
        message_id: [1u8; 32],
        origin_pubkey: [2u8; 32],
        tx_xdr: "AAAAAgAAAADZ/7+9/7+9/7+9EXAMPLE_XDR".to_string(),
        ttl_hops: 10,
        timestamp: 1672531200,
        signature: [3u8; 64],
    };

    println!("Processing transaction envelope...");
    println!("Origin: {:?}", hex::encode(&envelope.origin_pubkey[..8]));
    println!("Timestamp: {}", envelope.timestamp);

    match relay.process_envelope(&envelope, None).await {
        Ok(proof) => {
            println!("✓ Transaction submitted successfully!");
            println!("Proof sequence: {}", proof.sequence);
            println!(
                "Proof verifies: {}",
                proof.verify(&verifying_key, &[0xABu8; 32])
            );
        }
        Err(e) => {
            eprintln!("✗ Failed to process envelope: {}", e);
        }
    }

    Ok(())
}
