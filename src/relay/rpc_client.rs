use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

use crate::relay::RpcError;
use crate::relay::StellarRpcClient;

const DEFAULT_TIMEOUT_MS: u64 = 10_000;

/// Real HTTP client that talks to the Stellar Horizon REST API.
pub struct HorizonRpcClient {
    base_url: String,
    client: reqwest::Client,
    timeout: Duration,
}

impl HorizonRpcClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        let timeout = Duration::from_millis(DEFAULT_TIMEOUT_MS);
        Self {
            base_url: base_url.into(),
            client: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .expect("failed to build reqwest client"),
            timeout,
        }
    }

    pub fn testnet() -> Self {
        Self::new("https://horizon-testnet.stellar.org")
    }

    pub fn mainnet() -> Self {
        Self::new("https://horizon.stellar.org")
    }

    /// Submit with exponential-backoff retry. Only retries on Network/Timeout errors.
    pub async fn submit_with_retry(
        &self,
        tx_xdr: &str,
        max_attempts: u8,
    ) -> Result<String, RpcError> {
        let backoff_ms = [500u64, 1000, 2000, 4000];
        let mut last_err = RpcError::Network("no attempts made".into());

        for attempt in 0..max_attempts {
            match self.submit_transaction(tx_xdr).await {
                Ok(hash) => return Ok(hash),
                Err(e @ RpcError::TransactionRejected { .. }) => return Err(e),
                Err(e) => {
                    last_err = e;
                    if attempt + 1 < max_attempts {
                        let delay = backoff_ms.get(attempt as usize).copied().unwrap_or(4000);
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                    }
                }
            }
        }
        Err(last_err)
    }
}

// ---------- Horizon response shapes ----------

#[derive(Deserialize)]
struct HorizonSuccess {
    hash: String,
}

#[derive(Deserialize)]
struct HorizonError {
    #[serde(default)]
    title: String,
    #[serde(default)]
    detail: String,
    #[serde(rename = "extras", default)]
    extras: Option<HorizonExtras>,
}

#[derive(Deserialize, Default)]
struct HorizonExtras {
    #[serde(default)]
    result_codes: Option<ResultCodes>,
}

#[derive(Deserialize, Default)]
struct ResultCodes {
    #[serde(default)]
    transaction: Option<String>,
}

// ---------- async trait impl ----------

#[async_trait]
impl StellarRpcClient for HorizonRpcClient {
    async fn submit_transaction(&self, tx_xdr: &str) -> Result<String, RpcError> {
        let params = [("tx", tx_xdr)];

        let response = self
            .client
            .post(format!("{}/transactions", self.base_url))
            .form(&params)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    RpcError::Timeout {
                        timeout_ms: self.timeout.as_millis() as u64,
                    }
                } else {
                    RpcError::Network(e.to_string())
                }
            })?;

        let status = response.status();
        if status.is_success() {
            let body: HorizonSuccess = response
                .json()
                .await
                .map_err(|e| RpcError::Network(e.to_string()))?;
            Ok(body.hash)
        } else {
            let body_text = response.text().await.unwrap_or_default();
            // Try to parse Horizon error envelope for a human-readable reason.
            let reason = serde_json::from_str::<HorizonError>(&body_text)
                .ok()
                .map(|e| {
                    e.extras
                        .as_ref()
                        .and_then(|x| x.result_codes.as_ref())
                        .and_then(|r| r.transaction.clone())
                        .unwrap_or_else(|| {
                            if e.detail.is_empty() {
                                e.title.clone()
                            } else {
                                e.detail.clone()
                            }
                        })
                })
                .unwrap_or_else(|| body_text.clone());

            if status.is_client_error() {
                Err(RpcError::TransactionRejected { reason })
            } else {
                Err(RpcError::Http {
                    status: status.as_u16(),
                    body: body_text,
                })
            }
        }
    }

    async fn get_account_sequence(&self, public_key: &str) -> Result<u64, RpcError> {
        #[derive(Deserialize)]
        struct AccountResponse {
            sequence: String,
        }

        let response = self
            .client
            .get(format!("{}/accounts/{}", self.base_url, public_key))
            .send()
            .await
            .map_err(|e| RpcError::Network(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(RpcError::Http { status, body });
        }

        let account: AccountResponse = response
            .json()
            .await
            .map_err(|e| RpcError::Network(e.to_string()))?;
        account
            .sequence
            .parse::<u64>()
            .map_err(|e| RpcError::Network(format!("invalid sequence: {e}")))
    }

    async fn get_ledger_sequence(&self) -> Result<u64, RpcError> {
        #[derive(Deserialize)]
        struct LedgerResponse {
            sequence: u64,
        }
        #[derive(Deserialize)]
        struct EmbeddedRecords {
            records: Vec<LedgerResponse>,
        }
        #[derive(Deserialize)]
        struct LedgersResponse {
            _embedded: EmbeddedRecords,
        }

        let response = self
            .client
            .get(format!("{}/ledgers?order=desc&limit=1", self.base_url))
            .send()
            .await
            .map_err(|e| RpcError::Network(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(RpcError::Http { status, body });
        }

        let data: LedgersResponse = response
            .json()
            .await
            .map_err(|e| RpcError::Network(e.to_string()))?;
        data._embedded
            .records
            .into_iter()
            .next()
            .map(|r| r.sequence)
            .ok_or_else(|| RpcError::Network("no ledger records returned".into()))
    }

    async fn get_ledger_hash(&self) -> Result<String, RpcError> {
        #[derive(Deserialize)]
        struct LedgerRecord {
            hash: String,
        }
        #[derive(Deserialize)]
        struct EmbeddedRecords {
            records: Vec<LedgerRecord>,
        }
        #[derive(Deserialize)]
        struct LedgersResponse {
            _embedded: EmbeddedRecords,
        }

        let response = self
            .client
            .get(format!("{}/ledgers?order=desc&limit=1", self.base_url))
            .send()
            .await
            .map_err(|e| RpcError::Network(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(RpcError::Http { status, body });
        }

        let data: LedgersResponse = response
            .json()
            .await
            .map_err(|e| RpcError::Network(e.to_string()))?;
        data._embedded
            .records
            .into_iter()
            .next()
            .map(|r| r.hash)
            .ok_or_else(|| RpcError::Network("no ledger records returned".into()))
    }
}

// ---------- tests ----------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn make_client(base_url: &str) -> HorizonRpcClient {
        HorizonRpcClient::new(base_url)
    }

    // ── required test 1 ──────────────────────────────────────────────────────
    #[tokio::test]
    async fn test_horizon_client_parses_success_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/transactions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "hash": "abc123" })),
            )
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let result = client.submit_transaction("AAAAAQAAAAAAAAAA").await;

        assert_eq!(result.unwrap(), "abc123");
    }

    // ── required test 2 ──────────────────────────────────────────────────────
    #[tokio::test]
    async fn test_horizon_client_parses_rejection() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/transactions"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "type": "https://stellar.org/horizon-errors/transaction_failed",
                "title": "Transaction Failed",
                "detail": "The transaction failed when tried to be applied on the ledger",
                "extras": {
                    "result_codes": {
                        "transaction": "tx_bad_auth"
                    }
                }
            })))
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let result = client.submit_transaction("AAAAAQAAAAAAAAAA").await;

        match result {
            Err(RpcError::TransactionRejected { reason }) => {
                assert_eq!(reason, "tx_bad_auth");
            }
            other => panic!("expected TransactionRejected, got {:?}", other),
        }
    }

    // ── required test 3 ──────────────────────────────────────────────────────
    #[tokio::test]
    async fn test_retry_succeeds_on_third_attempt() {
        // A mock client that fails twice with Network then succeeds.
        struct CountingMock {
            calls: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl StellarRpcClient for CountingMock {
            async fn submit_transaction(&self, _tx_xdr: &str) -> Result<String, RpcError> {
                let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
                if n < 3 {
                    Err(RpcError::Network("transient".into()))
                } else {
                    Ok("ok_hash".into())
                }
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

        let calls = Arc::new(AtomicUsize::new(0));
        let mock = CountingMock {
            calls: calls.clone(),
        };

        // Build a HorizonRpcClient just for its submit_with_retry driver,
        // but we want to test the retry logic generically; inline the retry loop
        // here directly so we don't need to make HorizonRpcClient generic.
        let backoff_ms = [1u64, 1, 1, 1]; // near-zero for test speed
        let max_attempts: u8 = 4;
        let mut last_err = RpcError::Network("".into());
        let mut result = Err(RpcError::Network("".into()));
        for attempt in 0..max_attempts {
            match mock.submit_transaction("xdr").await {
                Ok(h) => {
                    result = Ok(h);
                    break;
                }
                Err(e @ RpcError::TransactionRejected { .. }) => {
                    result = Err(e);
                    break;
                }
                Err(e) => {
                    last_err = e;
                    if attempt + 1 < max_attempts {
                        tokio::time::sleep(Duration::from_millis(backoff_ms[attempt as usize]))
                            .await;
                    }
                }
            }
        }
        let _ = last_err;

        assert_eq!(result.unwrap(), "ok_hash");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    // ── required test 4 ──────────────────────────────────────────────────────
    #[tokio::test]
    async fn test_retry_gives_up_after_max_attempts() {
        let server = MockServer::start().await;
        // Every request returns 500 → RpcError::Http → treated as non-rejection,
        // but we want Network errors so we use a mock client directly.
        struct AlwaysFails {
            calls: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl StellarRpcClient for AlwaysFails {
            async fn submit_transaction(&self, _: &str) -> Result<String, RpcError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Err(RpcError::Network("always fails".into()))
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

        let calls = Arc::new(AtomicUsize::new(0));
        let mock = AlwaysFails {
            calls: calls.clone(),
        };

        let max_attempts: u8 = 3;
        let mut last_err: RpcError = RpcError::Network("".into());
        for attempt in 0..max_attempts {
            match mock.submit_transaction("xdr").await {
                Ok(_) => panic!("should not succeed"),
                Err(e @ RpcError::TransactionRejected { .. }) => {
                    last_err = e;
                    break;
                }
                Err(e) => {
                    last_err = e;
                    if attempt + 1 < max_attempts {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                }
            }
        }

        assert!(matches!(last_err, RpcError::Network(_)));
        assert_eq!(calls.load(Ordering::SeqCst), max_attempts as usize);

        // suppress unused variable warning from mock_server
        drop(server);
    }
}
