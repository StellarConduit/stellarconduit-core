pub mod rpc_client;

use crate::message::signing::verify_signature;
use crate::message::types::TransactionEnvelope;
use log::{error, info, warn};
use rpc_client::{RpcClient, RpcError};

/// Process a received TransactionEnvelope from the mesh network.
/// 
/// This function:
/// 1. Verifies the envelope's Ed25519 signature
/// 2. Submits the transaction to the Stellar Soroban RPC endpoint
/// 3. Logs the result
/// 
/// Returns Ok(tx_hash) on successful submission, or Err on failure.
/// Note: Signature verification failures are logged but do not crash the relay.
pub async fn process_envelope(
    rpc_client: &RpcClient,
    envelope: &TransactionEnvelope,
) -> Result<String, ProcessEnvelopeError> {
    // Step 1: Verify signature
    match verify_signature(envelope) {
        Ok(true) => {
            info!(
                "Signature verified for envelope from origin: {:?}",
                hex::encode(&envelope.origin_pubkey[..8])
            );
        }
        Ok(false) => {
            warn!("Signature verification returned false - should not happen");
            return Err(ProcessEnvelopeError::InvalidSignature);
        }
        Err(e) => {
            error!("Signature verification failed: {}", e);
            return Err(ProcessEnvelopeError::SignatureVerificationFailed(
                e.to_string(),
            ));
        }
    }

    // Step 2: Submit to Soroban RPC
    match rpc_client.submit_transaction(&envelope.tx_xdr).await {
        Ok(tx_hash) => {
            info!(
                "Transaction submitted successfully. Hash: {}, Origin: {:?}",
                tx_hash,
                hex::encode(&envelope.origin_pubkey[..8])
            );
            Ok(tx_hash)
        }
        Err(e) => {
            error!(
                "Failed to submit transaction from origin {:?}: {:?}",
                hex::encode(&envelope.origin_pubkey[..8]),
                e
            );
            Err(ProcessEnvelopeError::RpcSubmissionFailed(e))
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum ProcessEnvelopeError {
    #[error("Invalid signature")]
    InvalidSignature,

    #[error("Signature verification failed: {0}")]
    SignatureVerificationFailed(String),

    #[error("RPC submission failed: {0}")]
    RpcSubmissionFailed(RpcError),
}
