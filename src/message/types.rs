use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransactionEnvelope {
    /// Unique identifier for this message (usually hash of the payload)
    pub message_id: [u8; 32],

    /// The public key of the device that originally created this message
    pub origin_pubkey: [u8; 32],

    /// The actual base64-encoded Stellar XDR transaction envelope
    pub tx_xdr: String,

    /// Time-to-live in hops. Decremented by 1 at each hop. Drops when 0.
    pub ttl_hops: u8,

    /// Unix timestamp when the message was created
    pub timestamp: u64,

    /// Ed25519 signature of the payload by the `origin_pubkey`
    #[serde(with = "signature_serde")]
    pub signature: [u8; 64],
}

mod signature_serde {
    use serde::{
        de::{self, SeqAccess, Visitor},
        ser::SerializeTuple,
        Deserializer, Serializer,
    };
    use std::fmt;

    pub fn serialize<S>(sig: &[u8; 64], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut seq = serializer.serialize_tuple(64)?;
        for byte in sig.iter() {
            seq.serialize_element(byte)?;
        }
        seq.end()
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<[u8; 64], D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SignatureVisitor;

        impl<'de> Visitor<'de> for SignatureVisitor {
            type Value = [u8; 64];

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("an array of 64 bytes")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<[u8; 64], A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut arr = [0u8; 64];
                for (i, item) in arr.iter_mut().enumerate() {
                    *item = seq
                        .next_element()?
                        .ok_or_else(|| de::Error::invalid_length(i, &self))?;
                }
                Ok(arr)
            }
        }

        deserializer.deserialize_tuple(64, SignatureVisitor)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TopologyFlag {
    GoToSleep,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TopologyUpdate {
    pub origin_pubkey: [u8; 32],
    pub directly_connected_peers: Vec<[u8; 32]>,
    pub hops_to_relay: u8,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub topology_flags: Vec<TopologyFlag>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncRequest {
    /// A lightweight summary of the message IDs this device currently holds
    /// in its active buffer. For v1, this can just be a truncated list of recent IDs.
    pub known_message_ids: Vec<[u8; 4]>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncResponse {
    /// Full transaction envelopes that the requesting peer was determined to be missing
    pub missing_envelopes: Vec<TransactionEnvelope>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ProtocolMessage {
    /// A new or propagated transaction envelope
    Transaction(TransactionEnvelope),

    /// A topology heartbeat or routing table update
    TopologyUpdate(TopologyUpdate),

    /// A query asking for specific missing messages (for pull-based gossip)
    SyncRequest(SyncRequest),

    /// A response containing missing messages requested via SyncRequest
    SyncResponse(SyncResponse),
}

impl ProtocolMessage {
    pub fn to_bytes(&self) -> Result<Vec<u8>, rmp_serde::encode::Error> {
        rmp_serde::to_vec(self)
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self, rmp_serde::decode::Error> {
        rmp_serde::from_slice(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_mock_tx_envelope() -> TransactionEnvelope {
        // A typical Stellar XDR transaction envelope encoded in base64 is around 300 bytes.
        let mock_xdr = "AAAAAgAAAADZ/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9/7+9".to_string();

        TransactionEnvelope {
            message_id: [1u8; 32],
            origin_pubkey: [2u8; 32],
            tx_xdr: mock_xdr,
            ttl_hops: 10,
            timestamp: 1672531200,
            signature: [3u8; 64],
        }
    }

    #[test]
    fn test_serialization_roundtrip() {
        let envelope = create_mock_tx_envelope();
        let msg = ProtocolMessage::Transaction(envelope.clone());

        let bytes = msg.to_bytes().expect("Failed to serialize");
        let decoded = ProtocolMessage::from_bytes(&bytes).expect("Failed to deserialize");

        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_serialized_size_limit() {
        let envelope = create_mock_tx_envelope();
        let msg = ProtocolMessage::Transaction(envelope);

        let bytes = msg.to_bytes().expect("Failed to serialize");

        // The requirement is: "a TransactionEnvelope containing a typical 300-byte XDR string must serialize to under 500 bytes total"
        // Let's verify the size of our mock XDR string first
        let inner_xdr_size = match msg {
            ProtocolMessage::Transaction(ref tx) => tx.tx_xdr.len(),
            _ => unreachable!(),
        };
        assert!(
            inner_xdr_size > 250 && inner_xdr_size < 350,
            "Mock XDR should be around 300 bytes"
        );

        println!("Serialized size: {} bytes", bytes.len());
        assert!(
            bytes.len() < 500,
            "Serialized message must be under 500 bytes (was {})",
            bytes.len()
        );
    }
}
