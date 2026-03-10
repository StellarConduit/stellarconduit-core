use reqwest::Client;
use serde::{Deserialize, Serialize};

/// JSON-RPC request structure for Soroban RPC
#[derive(Serialize, Debug)]
pub struct SorobanRpcRequest {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: &'static str,
    pub params: SendTxParams,
}

/// Parameters for the sendTransaction method
#[derive(Serialize, Debug)]
pub struct SendTxParams {
    pub transaction: String, // The raw base64-encoded XDR
}

/// JSON-RPC response structure from Soroban RPC
#[derive(Deserialize, Debug)]
pub struct SorobanRpcResponse {
    pub id: u64,
    pub result: Option<TxResultData>,
    pub error: Option<RpcError>,
}

/// Transaction result data
#[derive(Deserialize, Debug)]
pub struct TxResultData {
    pub status: String, // e.g. "PENDING" or "ERROR"
    pub hash: Option<String>,
}

/// RPC error structure
#[derive(Deserialize, Debug, Clone, thiserror::Error)]
#[error("RPC Error {code}: {message}")]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

/// HTTP client for submitting transactions to Stellar Soroban RPC
pub struct RpcClient {
    endpoint: String,
    http: Client,
}

impl RpcClient {
    /// Create a new RPC client with the given endpoint URL
    /// 
    /// # Example
    /// ```
    /// use stellarconduit_core::relay::rpc_client::RpcClient;
    /// 
    /// let client = RpcClient::new("https://soroban-testnet.stellar.org");
    /// ```
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            http: Client::new(),
        }
    }

    /// Submit a base64-encoded XDR transaction to the Soroban network.
    /// 
    /// # Arguments
    /// * `tx_xdr` - The base64-encoded Stellar transaction XDR
    /// 
    /// # Returns
    /// * `Ok(String)` - The transaction hash on successful submission
    /// * `Err(RpcError)` - RPC error details if submission fails
    /// 
    /// # Example
    /// ```no_run
    /// # use stellarconduit_core::relay::rpc_client::RpcClient;
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let client = RpcClient::new("https://soroban-testnet.stellar.org");
    /// let tx_hash = client.submit_transaction("AAAAAgAAAAD...").await?;
    /// println!("Transaction hash: {}", tx_hash);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn submit_transaction(&self, tx_xdr: &str) -> Result<String, RpcError> {
        let request = SorobanRpcRequest {
            jsonrpc: "2.0",
            id: 1,
            method: "sendTransaction",
            params: SendTxParams {
                transaction: tx_xdr.to_string(),
            },
        };

        let response = self
            .http
            .post(&self.endpoint)
            .json(&request)
            .send()
            .await
            .map_err(|e| RpcError {
                code: -1,
                message: format!("HTTP request failed: {}", e),
            })?;

        let rpc_response: SorobanRpcResponse = response.json().await.map_err(|e| RpcError {
            code: -2,
            message: format!("Failed to parse JSON response: {}", e),
        })?;

        // Check for RPC-level errors
        if let Some(error) = rpc_response.error {
            return Err(error);
        }

        // Extract the transaction hash from the result
        if let Some(result) = rpc_response.result {
            if let Some(hash) = result.hash {
                Ok(hash)
            } else {
                Err(RpcError {
                    code: -3,
                    message: format!("Transaction status: {} but no hash returned", result.status),
                })
            }
        } else {
            Err(RpcError {
                code: -4,
                message: "No result or error in RPC response".to_string(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json_string, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn test_submit_transaction_success() {
        // Start a mock HTTP server
        let mock_server = MockServer::start().await;

        // Define the expected request body
        let expected_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendTransaction",
            "params": {
                "transaction": "AAAAAgAAAADZ/7+9/7+9/7+9"
            }
        });

        // Define the mock response
        let mock_response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "status": "PENDING",
                "hash": "abc123def456"
            }
        });

        Mock::given(method("POST"))
            .and(path("/"))
            .and(body_json_string(expected_request.to_string()))
            .respond_with(ResponseTemplate::new(200).set_body_json(mock_response))
            .mount(&mock_server)
            .await;

        // Create RPC client pointing to mock server
        let client = RpcClient::new(mock_server.uri());

        // Submit transaction
        let result = client
            .submit_transaction("AAAAAgAAAADZ/7+9/7+9/7+9")
            .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "abc123def456");
    }

    #[tokio::test]
    async fn test_submit_transaction_rpc_error() {
        let mock_server = MockServer::start().await;

        let mock_response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": {
                "code": -32600,
                "message": "Invalid transaction format"
            }
        });

        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(mock_response))
            .mount(&mock_server)
            .await;

        let client = RpcClient::new(mock_server.uri());
        let result = client.submit_transaction("invalid_xdr").await;

        assert!(result.is_err());
        let error = result.unwrap_err();
        assert_eq!(error.code, -32600);
        assert_eq!(error.message, "Invalid transaction format");
    }

    #[tokio::test]
    async fn test_submit_transaction_no_hash() {
        let mock_server = MockServer::start().await;

        let mock_response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "status": "ERROR"
            }
        });

        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(mock_response))
            .mount(&mock_server)
            .await;

        let client = RpcClient::new(mock_server.uri());
        let result = client.submit_transaction("some_xdr").await;

        assert!(result.is_err());
        let error = result.unwrap_err();
        assert_eq!(error.code, -3);
    }
}
