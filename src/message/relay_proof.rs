use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayChainProof {
    #[serde(with = "signature_serde")]
    pub signature: [u8; 64],
    pub chain_hash: [u8; 32],
    pub sequence: u64,
}

impl RelayChainProof {
    pub fn sign(
        signing_key: &SigningKey,
        tx_id: &[u8; 32],
        chain_hash: &[u8; 32],
        sequence: u64,
    ) -> Self {
        let message = proof_message_bytes(tx_id, chain_hash, sequence);
        let signature = signing_key.sign(&message);

        Self {
            signature: signature.to_bytes(),
            chain_hash: *chain_hash,
            sequence,
        }
    }

    pub fn verify(&self, verifying_key: &VerifyingKey, tx_id: &[u8; 32]) -> bool {
        let message = proof_message_bytes(tx_id, &self.chain_hash, self.sequence);
        let signature = Signature::from_bytes(&self.signature);

        verifying_key.verify(&message, &signature).is_ok()
    }
}

fn proof_message_bytes(tx_id: &[u8; 32], chain_hash: &[u8; 32], sequence: u64) -> [u8; 72] {
    let mut message = [0u8; 72];
    message[..32].copy_from_slice(tx_id);
    message[32..64].copy_from_slice(chain_hash);
    message[64..].copy_from_slice(&sequence.to_be_bytes());
    message
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
        for byte in sig {
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
