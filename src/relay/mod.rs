pub mod dedup;
pub mod rpc_client;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ed25519_dalek::SigningKey;
use log::{info, warn};

use crate::message::relay_proof::RelayChainProof;
use crate::message::types::TransactionEnvelope;
use crate::metrics::Metrics;
use crate::peer::peer_node::Peer;
use crate::peer::reputation::{apply_reward, RewardReason};
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

/// Configuration governing how transient submission failures are retried.
///
/// Only transient RPC errors (`RpcError::Network`, `RpcError::Timeout`,
/// `RpcError::Http`) are retried; `RpcError::TransactionRejected` is terminal
/// and never retried.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of submission attempts (including the first). Default: 5.
    pub max_attempts: u32,
    /// Initial backoff interval. Default: 500ms.
    pub initial_backoff: Duration,
    /// Backoff multiplier applied after each failure. Default: 2.0.
    pub multiplier: f64,
    /// Maximum backoff cap. Default: 30 seconds.
    pub max_backoff: Duration,
    /// Jitter factor (0.0–1.0). Adds randomness to prevent thundering herd. Default: 0.2.
    pub jitter: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            initial_backoff: Duration::from_millis(500),
            multiplier: 2.0,
            max_backoff: Duration::from_secs(30),
            jitter: 0.2,
        }
    }
}

/// Relay node that processes transaction envelopes and submits them to Stellar.
pub struct RelayNode {
    deduplicator: RelayDeduplicator,
    rpc_client: Box<dyn StellarRpcClient>,
    signing_key: SigningKey,
    metrics: Arc<Metrics>,
    retry_config: RetryConfig,
}

impl RelayNode {
    pub fn new(
        capacity: usize,
        rpc_client: Box<dyn StellarRpcClient>,
        signing_key: SigningKey,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            deduplicator: RelayDeduplicator::new(capacity),
            rpc_client,
            signing_key,
            metrics,
            retry_config: RetryConfig::default(),
        }
    }

    /// Override the retry configuration. Returns `self` for builder-style chaining.
    pub fn with_retry_config(mut self, retry_config: RetryConfig) -> Self {
        self.retry_config = retry_config;
        self
    }

    /// Submit a transaction to the Stellar RPC node, retrying transient failures
    /// with exponential backoff and jitter.
    ///
    /// `RpcError::TransactionRejected` is never retried — a rejected transaction
    /// will always be rejected. All other errors are treated as transient and
    /// retried up to `retry_config.max_attempts` times before being surfaced to
    /// the caller.
    async fn submit_with_retry(&self, tx_xdr: &str) -> Result<String, RpcError> {
        let mut backoff = self.retry_config.initial_backoff;
        let mut attempt = 0;

        loop {
            attempt += 1;
            match self.rpc_client.submit_transaction(tx_xdr).await {
                Ok(hash) => return Ok(hash),
                Err(RpcError::TransactionRejected { reason }) => {
                    // Never retry a rejected transaction.
                    return Err(RpcError::TransactionRejected { reason });
                }
                Err(e) => {
                    if attempt >= self.retry_config.max_attempts {
                        return Err(e);
                    }
                    self.metrics
                        .relay_submission_retries
                        .fetch_add(1, Ordering::Relaxed);
                    let jitter_ms = (backoff.as_millis() as f64
                        * self.retry_config.jitter
                        * rand::random::<f64>()) as u64;
                    let sleep_duration = backoff + Duration::from_millis(jitter_ms);
                    warn!(
                        "Relay submission attempt {}/{} failed: {}. Retrying in {:?}.",
                        attempt, self.retry_config.max_attempts, e, sleep_duration
                    );
                    tokio::time::sleep(sleep_duration).await;
                    backoff = backoff
                        .mul_f64(self.retry_config.multiplier)
                        .min(self.retry_config.max_backoff);
                }
            }
        }
    }

    /// Process a transaction envelope, checking for duplicates before submission.
    ///
    /// Pass `peer` to apply a reputation reward on the origin peer when the
    /// transaction is successfully submitted for the first time.
    pub async fn process_envelope(
        &mut self,
        envelope: &TransactionEnvelope,
        peer: Option<&mut Peer>,
    ) -> Result<RelayChainProof, RpcError> {
        if let Some(existing_proof) = self.deduplicator.check(&envelope.message_id) {
            info!(
                "Duplicate transaction {:?}, returning cached proof",
                envelope.message_id
            );
            return Ok(existing_proof);
        }

        let tx_hash = match self.submit_with_retry(&envelope.tx_xdr).await {
            Ok(h) => {
                self.metrics
                    .transactions_submitted
                    .fetch_add(1, Ordering::Relaxed);
                h
            }
            Err(RpcError::TransactionRejected { reason }) => {
                self.metrics
                    .transactions_rejected
                    .fetch_add(1, Ordering::Relaxed);
                return Err(RpcError::TransactionRejected { reason });
            }
            Err(e) => return Err(e),
        };

        let tx_id = decode_hash_32(&tx_hash, "transaction hash").map_err(RpcError::Network)?;
        let sequence = self.rpc_client.get_ledger_sequence().await?;
        let chain_hash_str = self.rpc_client.get_ledger_hash().await?;
        let chain_hash =
            decode_hash_32(&chain_hash_str, "ledger hash").map_err(RpcError::Network)?;

        let proof = RelayChainProof::sign(&self.signing_key, &tx_id, &chain_hash, sequence);
        self.deduplicator
            .mark_submitted(envelope.message_id, proof.clone());

        if let Some(p) = peer {
            apply_reward(p, RewardReason::SuccessfullyRoutedTx);
        }

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

    /// Which transient error a `FlakyRpcClient` returns while in its failing window.
    #[derive(Clone, Copy)]
    enum FlakyError {
        Network,
        Timeout,
        Rejected,
    }

    /// Mock client that fails its first `fail_count` `submit_transaction` calls
    /// with `error`, then succeeds. Set `fail_count` to `usize::MAX` to fail
    /// every call.
    struct FlakyRpcClient {
        submit_count: Arc<AtomicUsize>,
        fail_count: usize,
        error: FlakyError,
        tx_hash: String,
        ledger_hash: String,
        ledger_sequence: u64,
    }

    impl FlakyRpcClient {
        fn new(submit_count: Arc<AtomicUsize>, fail_count: usize, error: FlakyError) -> Self {
            Self {
                submit_count,
                fail_count,
                error,
                tx_hash: hex::encode([0xABu8; 32]),
                ledger_hash: hex::encode([0xCDu8; 32]),
                ledger_sequence: 42,
            }
        }
    }

    #[async_trait]
    impl StellarRpcClient for FlakyRpcClient {
        async fn submit_transaction(&self, _tx_xdr: &str) -> Result<String, RpcError> {
            let prior = self.submit_count.fetch_add(1, Ordering::SeqCst);
            if prior < self.fail_count {
                return Err(match self.error {
                    FlakyError::Network => RpcError::Network("transient".to_string()),
                    FlakyError::Timeout => RpcError::Timeout { timeout_ms: 1000 },
                    FlakyError::Rejected => RpcError::TransactionRejected {
                        reason: "rejected".to_string(),
                    },
                });
            }
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

    /// Fast retry config so backoff sleeps don't slow the test suite down.
    fn fast_retry_config() -> RetryConfig {
        RetryConfig {
            max_attempts: 5,
            initial_backoff: Duration::from_millis(1),
            multiplier: 2.0,
            max_backoff: Duration::from_millis(5),
            jitter: 0.2,
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
            Metrics::new(),
        );
        let envelope = create_test_envelope([1u8; 32]);

        let proof1 = relay.process_envelope(&envelope, None).await.unwrap();
        assert_eq!(submit_count.load(Ordering::SeqCst), 1);
        assert!(proof1.verify(&verifying_key, &[0xABu8; 32]));

        let proof2 = relay.process_envelope(&envelope, None).await.unwrap();
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
            Metrics::new(),
        );
        let envelope = create_test_envelope([1u8; 32]);

        let p1 = relay.process_envelope(&envelope, None).await.unwrap();
        let p2 = relay.process_envelope(&envelope, None).await.unwrap();
        let p3 = relay.process_envelope(&envelope, None).await.unwrap();

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
            Metrics::new(),
        );

        relay
            .process_envelope(&create_test_envelope([1u8; 32]), None)
            .await
            .unwrap();
        relay
            .process_envelope(&create_test_envelope([2u8; 32]), None)
            .await
            .unwrap();

        assert_eq!(submit_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_relay_metrics_transactions_submitted() {
        let submit_count = Arc::new(AtomicUsize::new(0));
        let metrics = Metrics::new();
        let mut relay = RelayNode::new(
            1000,
            Box::new(make_client(submit_count.clone())),
            create_signing_key(),
            metrics.clone(),
        );

        relay
            .process_envelope(&create_test_envelope([1u8; 32]), None)
            .await
            .unwrap();
        relay
            .process_envelope(&create_test_envelope([2u8; 32]), None)
            .await
            .unwrap();

        assert_eq!(metrics.transactions_submitted.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn test_relay_metrics_transactions_rejected() {
        struct RejectingClient;

        #[async_trait]
        impl StellarRpcClient for RejectingClient {
            async fn submit_transaction(&self, _: &str) -> Result<String, RpcError> {
                Err(RpcError::TransactionRejected {
                    reason: "test".to_string(),
                })
            }
            async fn get_account_sequence(&self, _: &str) -> Result<u64, RpcError> {
                Ok(0)
            }
            async fn get_ledger_sequence(&self) -> Result<u64, RpcError> {
                Ok(0)
            }
            async fn get_ledger_hash(&self) -> Result<String, RpcError> {
                Ok(String::new())
            }
        }

        let metrics = Metrics::new();
        let mut relay = RelayNode::new(
            1000,
            Box::new(RejectingClient),
            create_signing_key(),
            metrics.clone(),
        );

        let result = relay
            .process_envelope(&create_test_envelope([3u8; 32]), None)
            .await;
        assert!(matches!(result, Err(RpcError::TransactionRejected { .. })));
        assert_eq!(metrics.transactions_rejected.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.transactions_submitted.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn test_retry_succeeds_on_second_attempt() {
        let submit_count = Arc::new(AtomicUsize::new(0));
        let client = FlakyRpcClient::new(submit_count.clone(), 1, FlakyError::Network);
        let mut relay =
            RelayNode::new(1000, Box::new(client), create_signing_key(), Metrics::new())
                .with_retry_config(fast_retry_config());

        let result = relay
            .process_envelope(&create_test_envelope([1u8; 32]), None)
            .await;

        assert!(result.is_ok());
        assert_eq!(submit_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_no_retry_on_rejected() {
        let submit_count = Arc::new(AtomicUsize::new(0));
        let client = FlakyRpcClient::new(submit_count.clone(), usize::MAX, FlakyError::Rejected);
        let mut relay =
            RelayNode::new(1000, Box::new(client), create_signing_key(), Metrics::new())
                .with_retry_config(fast_retry_config());

        let result = relay
            .process_envelope(&create_test_envelope([2u8; 32]), None)
            .await;

        assert!(matches!(result, Err(RpcError::TransactionRejected { .. })));
        assert_eq!(submit_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_gives_up_after_max_attempts() {
        let submit_count = Arc::new(AtomicUsize::new(0));
        let config = fast_retry_config();
        let max_attempts = config.max_attempts as usize;
        let client = FlakyRpcClient::new(submit_count.clone(), usize::MAX, FlakyError::Timeout);
        let mut relay =
            RelayNode::new(1000, Box::new(client), create_signing_key(), Metrics::new())
                .with_retry_config(config);

        let result = relay
            .process_envelope(&create_test_envelope([3u8; 32]), None)
            .await;

        assert!(matches!(result, Err(RpcError::Timeout { .. })));
        assert_eq!(submit_count.load(Ordering::SeqCst), max_attempts);
    }

    #[tokio::test]
    async fn test_retry_counter_increments() {
        let submit_count = Arc::new(AtomicUsize::new(0));
        let metrics = Metrics::new();
        let client = FlakyRpcClient::new(submit_count.clone(), 1, FlakyError::Network);
        let mut relay = RelayNode::new(
            1000,
            Box::new(client),
            create_signing_key(),
            metrics.clone(),
        )
        .with_retry_config(fast_retry_config());

        relay
            .process_envelope(&create_test_envelope([4u8; 32]), None)
            .await
            .unwrap();

        assert_eq!(metrics.relay_submission_retries.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn test_dedup_cache_not_polluted_by_failed_attempts() {
        let submit_count = Arc::new(AtomicUsize::new(0));
        let message_id = [5u8; 32];
        let client = FlakyRpcClient::new(submit_count.clone(), usize::MAX, FlakyError::Timeout);
        let mut relay =
            RelayNode::new(1000, Box::new(client), create_signing_key(), Metrics::new())
                .with_retry_config(fast_retry_config());

        let result = relay
            .process_envelope(&create_test_envelope(message_id), None)
            .await;

        assert!(result.is_err());
        // A future re-submission (from a different relay) must still be able to
        // succeed, so the dedup cache must not retain the failed message_id.
        assert!(relay.deduplicator.check(&message_id).is_none());
    }
}
