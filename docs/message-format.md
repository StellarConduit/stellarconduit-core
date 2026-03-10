# Message Format and Security Deep-Dive

## Overview

StellarConduit uses a custom binary protocol for mesh network communication. This document provides an authoritative specification of the message format, serialization, and cryptographic security model. Any client (Android, iOS, or other) implementing the protocol must follow these specifications exactly to ensure interoperability and security.

## Table of Contents

1. [Protocol Message Structure](#protocol-message-structure)
2. [MessagePack Serialization](#messagepack-serialization)
3. [Cryptographic Security Model](#cryptographic-security-model)
4. [Signature Generation Algorithm](#signature-generation-algorithm)
5. [Signature Verification Algorithm](#signature-verification-algorithm)
6. [Relay Node Integration](#relay-node-integration)
7. [Security Considerations](#security-considerations)
8. [Implementation Examples](#implementation-examples)

## Protocol Message Structure

### ProtocolMessage Enum

The top-level message type is a Rust enum serialized using MessagePack:

```rust
pub enum ProtocolMessage {
    Transaction(TransactionEnvelope),
    TopologyUpdate(TopologyUpdate),
    SyncRequest(SyncRequest),
    SyncResponse(SyncResponse),
}
```

### Message Type Breakdown

#### 1. Transaction (Variant 0)

Carries a signed Stellar transaction through the mesh network.

```rust
pub struct TransactionEnvelope {
    pub message_id: [u8; 32],      // SHA-256 hash (deterministic)
    pub origin_pubkey: [u8; 32],   // Ed25519 public key
    pub tx_xdr: String,             // Base64-encoded Stellar XDR
    pub ttl_hops: u8,               // Time-to-live in hops
    pub timestamp: u64,             // Unix timestamp (seconds)
    pub signature: [u8; 64],        // Ed25519 signature
}
```

**Field Descriptions:**

- `message_id`: A unique identifier for deduplication. Typically the SHA-256 hash of the payload.
- `origin_pubkey`: The Ed25519 public key of the device that created this message.
- `tx_xdr`: The Stellar transaction envelope encoded as base64. This is what gets submitted to the Stellar network.
- `ttl_hops`: Decremented by 1 at each hop. When it reaches 0, the message is dropped to prevent infinite propagation.
- `timestamp`: Unix timestamp (seconds since epoch) when the message was created. Used for replay attack prevention.
- `signature`: Ed25519 signature over the payload hash. Proves authenticity and integrity.

**Typical Size:** A TransactionEnvelope with a 300-byte XDR string serializes to approximately 450-480 bytes in MessagePack format.

#### 2. TopologyUpdate (Variant 1)

Announces a device's direct connections and distance to relay nodes.

```rust
pub struct TopologyUpdate {
    pub origin_pubkey: [u8; 32],
    pub directly_connected_peers: Vec<[u8; 32]>,
    pub hops_to_relay: u8,
}
```

**Purpose:** Enables routing decisions and relay node discovery.

#### 3. SyncRequest (Variant 2)

Requests missing messages from a peer (pull-based gossip).

```rust
pub struct SyncRequest {
    pub known_message_ids: Vec<[u8; 4]>,  // Truncated message IDs
}
```

**Purpose:** Allows devices to catch up on messages they missed while offline.

#### 4. SyncResponse (Variant 3)

Responds with the full envelopes that the requester is missing.

```rust
pub struct SyncResponse {
    pub missing_envelopes: Vec<TransactionEnvelope>,
}
```

## MessagePack Serialization

StellarConduit uses [MessagePack](https://msgpack.org/) for efficient binary serialization via the `rmp-serde` crate.


### Why MessagePack?

1. **Compact**: 20-30% smaller than JSON for typical payloads
2. **Fast**: Binary format with minimal parsing overhead
3. **Schema-less**: No need for .proto files like Protocol Buffers
4. **Cross-platform**: Implementations available for all major languages

### Serialization Format

MessagePack uses type tags to encode data structures. Here's how a `TransactionEnvelope` is encoded:

```
[Enum Variant Tag: 0x00] [Struct with 6 fields]
  ├─ message_id:     [fixarray 32] [32 bytes]
  ├─ origin_pubkey:  [fixarray 32] [32 bytes]
  ├─ tx_xdr:         [str8/str16/str32] [length] [UTF-8 bytes]
  ├─ ttl_hops:       [uint8] [1 byte]
  ├─ timestamp:      [uint64] [8 bytes]
  └─ signature:      [fixarray 64] [64 bytes]
```

### Example: Encoded TransactionEnvelope

Given this envelope:
```rust
TransactionEnvelope {
    message_id: [0x01; 32],
    origin_pubkey: [0x02; 32],
    tx_xdr: "AAAAAgAAAADZ/7+9".to_string(),  // 20 bytes
    ttl_hops: 10,
    timestamp: 1672531200,
    signature: [0x03; 64],
}
```

The MessagePack encoding is approximately:
- Enum variant tag: 1 byte
- Struct header: 1 byte
- message_id: 1 (array tag) + 32 (bytes) = 33 bytes
- origin_pubkey: 1 + 32 = 33 bytes
- tx_xdr: 1 (str tag) + 1 (length) + 20 (data) = 22 bytes
- ttl_hops: 1 byte
- timestamp: 1 (uint64 tag) + 8 (bytes) = 9 bytes
- signature: 1 + 64 = 65 bytes

**Total: ~165 bytes** for this minimal example.

For a typical 300-byte XDR transaction, the total size is approximately 450-480 bytes.

### Serialization API

```rust
use stellarconduit_core::message::types::ProtocolMessage;

// Serialize to bytes
let bytes: Vec<u8> = message.to_bytes()?;

// Deserialize from bytes
let message: ProtocolMessage = ProtocolMessage::from_bytes(&bytes)?;
```


## Cryptographic Security Model

### Threat Model

The mesh network is **trustless and adversarial**:
- Any device can join the network
- Malicious actors may attempt to forge transactions
- Messages traverse multiple untrusted hops
- No central authority validates messages

### Security Guarantees

The protocol provides:

1. **Authenticity**: Only the holder of the private key can create valid messages
2. **Integrity**: Any tampering with the message invalidates the signature
3. **Non-repudiation**: The creator cannot deny creating a signed message
4. **Replay Protection**: Timestamps prevent old messages from being reused

### Cryptographic Primitives

- **Signing Algorithm**: Ed25519 (RFC 8032)
- **Hash Function**: SHA-256 (FIPS 180-4)
- **Key Size**: 256 bits (32 bytes)
- **Signature Size**: 512 bits (64 bytes)

### Ed25519 Properties

Ed25519 was chosen for:
- **Speed**: Fast signing and verification on mobile devices
- **Security**: 128-bit security level, resistant to side-channel attacks
- **Deterministic**: Same message always produces the same signature
- **Small keys**: 32-byte public keys, 64-byte signatures

### Key Generation

Each device generates its own Ed25519 keypair on first launch:

```rust
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;

let mut csprng = OsRng;
let signing_key = SigningKey::generate(&mut csprng);
let verifying_key = signing_key.verifying_key();

// Store signing_key securely (e.g., Android Keystore, iOS Keychain)
// Broadcast verifying_key.to_bytes() as your peer identity
```

**CRITICAL**: The private key (SigningKey) must NEVER leave the device. Store it in:
- **Android**: Android Keystore System
- **iOS**: Secure Enclave / Keychain
- **Desktop**: OS-specific secure storage


## Signature Generation Algorithm

This is the **most critical section** for interoperability. Any client must follow this exact algorithm.

### The Payload Hash

The signature is computed over a **deterministic hash** of three fields:

```
SHA-256(origin_pubkey || timestamp || tx_xdr)
```

Where `||` denotes concatenation.

### Step-by-Step Algorithm

To create a valid `TransactionEnvelope`:

#### Step 1: Prepare the Components

```rust
let origin_pubkey: [u8; 32] = signing_key.verifying_key().to_bytes();
let timestamp: u64 = current_unix_timestamp();
let tx_xdr: String = "AAAAAgAAAAD..."; // Base64-encoded Stellar XDR
```

#### Step 2: Construct the Payload Buffer

Concatenate in this exact order:

```rust
use sha2::{Digest, Sha256};

let mut hasher = Sha256::new();

// 1. Public key (32 bytes, raw bytes)
hasher.update(origin_pubkey);

// 2. Timestamp (8 bytes, big-endian)
hasher.update(timestamp.to_be_bytes());

// 3. Transaction XDR (variable length, UTF-8 bytes)
hasher.update(tx_xdr.as_bytes());
```

**CRITICAL NOTES:**
- The timestamp MUST be encoded as **big-endian** (network byte order)
- The tx_xdr is hashed as **UTF-8 bytes**, not as a byte array
- The order is: pubkey → timestamp → tx_xdr (NOT alphabetical)

#### Step 3: Compute the Hash

```rust
let payload_hash: [u8; 32] = hasher.finalize().into();
```

This 32-byte hash is what gets signed.

#### Step 4: Sign the Hash

```rust
use ed25519_dalek::Signer;

let signature = signing_key.sign(&payload_hash);
let signature_bytes: [u8; 64] = signature.to_bytes();
```

#### Step 5: Construct the Envelope

```rust
let envelope = TransactionEnvelope {
    message_id: payload_hash,  // Reuse the hash as message ID
    origin_pubkey,
    tx_xdr,
    ttl_hops: 10,  // Or your desired TTL
    timestamp,
    signature: signature_bytes,
};
```


### Complete Example (Rust)

```rust
use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};
use stellarconduit_core::message::types::TransactionEnvelope;

fn create_signed_envelope(
    signing_key: &SigningKey,
    tx_xdr: String,
) -> TransactionEnvelope {
    let origin_pubkey = signing_key.verifying_key().to_bytes();
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // Compute payload hash
    let mut hasher = Sha256::new();
    hasher.update(origin_pubkey);
    hasher.update(timestamp.to_be_bytes());
    hasher.update(tx_xdr.as_bytes());
    let payload_hash: [u8; 32] = hasher.finalize().into();

    // Sign the hash
    let signature = signing_key.sign(&payload_hash);

    TransactionEnvelope {
        message_id: payload_hash,
        origin_pubkey,
        tx_xdr,
        ttl_hops: 10,
        timestamp,
        signature: signature.to_bytes(),
    }
}
```

### Pseudocode for Other Languages

```python
# Python example using ed25519 and hashlib
import hashlib
import time
from nacl.signing import SigningKey

def create_signed_envelope(signing_key: SigningKey, tx_xdr: str):
    origin_pubkey = bytes(signing_key.verify_key)
    timestamp = int(time.time())
    
    # Construct payload
    payload = b''
    payload += origin_pubkey  # 32 bytes
    payload += timestamp.to_bytes(8, byteorder='big')  # 8 bytes, big-endian
    payload += tx_xdr.encode('utf-8')  # Variable length
    
    # Hash the payload
    payload_hash = hashlib.sha256(payload).digest()
    
    # Sign the hash
    signature = signing_key.sign(payload_hash).signature
    
    return {
        'message_id': payload_hash,
        'origin_pubkey': origin_pubkey,
        'tx_xdr': tx_xdr,
        'ttl_hops': 10,
        'timestamp': timestamp,
        'signature': signature
    }
```


## Signature Verification Algorithm

When a device receives a `TransactionEnvelope`, it MUST verify the signature before processing.

### Step-by-Step Verification

#### Step 1: Extract the Public Key

```rust
use ed25519_dalek::VerifyingKey;

let verifying_key = VerifyingKey::from_bytes(&envelope.origin_pubkey)
    .map_err(|_| "Invalid public key")?;
```

#### Step 2: Reconstruct the Payload Hash

Use the **exact same algorithm** as signing:

```rust
use sha2::{Digest, Sha256};

let mut hasher = Sha256::new();
hasher.update(envelope.origin_pubkey);
hasher.update(envelope.timestamp.to_be_bytes());
hasher.update(envelope.tx_xdr.as_bytes());
let payload_hash: [u8; 32] = hasher.finalize().into();
```

#### Step 3: Verify the Signature

```rust
use ed25519_dalek::{Signature, Verifier};

let signature = Signature::from_bytes(&envelope.signature);

match verifying_key.verify(&payload_hash, &signature) {
    Ok(_) => {
        // Signature is valid
        println!("✓ Signature verified");
    }
    Err(_) => {
        // Signature is invalid - reject the message
        eprintln!("✗ Invalid signature - message rejected");
        return Err("Invalid signature");
    }
}
```

### Complete Verification Function

```rust
use stellarconduit_core::message::signing::verify_signature;
use stellarconduit_core::message::types::TransactionEnvelope;

fn process_received_envelope(envelope: &TransactionEnvelope) -> Result<(), String> {
    // Verify signature
    match verify_signature(envelope) {
        Ok(true) => {
            println!("Signature valid - processing message");
            // Continue processing...
            Ok(())
        }
        Ok(false) => Err("Signature verification returned false".to_string()),
        Err(e) => Err(format!("Signature verification failed: {}", e)),
    }
}
```

### Verification Failures

A signature verification can fail for several reasons:

1. **Malformed Public Key**: The `origin_pubkey` is not a valid Ed25519 public key
2. **Invalid Signature**: The signature doesn't match the payload hash
3. **Tampered Message**: Any field was modified after signing
4. **Wrong Algorithm**: The signature was created with a different algorithm

**CRITICAL**: Always reject messages with invalid signatures. Never process them further.


## Relay Node Integration

Relay nodes are devices with internet connectivity that bridge the mesh network to the Stellar blockchain.

### Transaction Flow

```
Mobile Device → Mesh Network → Relay Node → Stellar Network
     (1)            (2)            (3)           (4)
```

1. **Mobile Device**: Creates and signs a `TransactionEnvelope`
2. **Mesh Network**: Propagates the envelope via gossip protocol
3. **Relay Node**: Receives, verifies, and unwraps the envelope
4. **Stellar Network**: Processes the transaction on-chain

### Relay Node Processing

When a relay node receives a `ProtocolMessage::Transaction(envelope)`:

#### Step 1: Verify the Signature

```rust
use stellarconduit_core::message::signing::verify_signature;

if !verify_signature(&envelope)? {
    log::error!("Invalid signature - rejecting envelope");
    return Err("Invalid signature");
}
```

**CRITICAL**: Never submit transactions with invalid signatures to the Stellar network.

#### Step 2: Extract the Transaction XDR

```rust
let tx_xdr: &str = &envelope.tx_xdr;
```

The `tx_xdr` field contains the base64-encoded Stellar transaction envelope. This is the raw XDR that gets submitted to the Stellar RPC endpoint.

#### Step 3: Submit to Stellar Soroban RPC

```rust
use stellarconduit_core::relay::rpc_client::RpcClient;

let rpc_client = RpcClient::new("https://soroban-testnet.stellar.org");
let tx_hash = rpc_client.submit_transaction(tx_xdr).await?;

log::info!("Transaction submitted: {}", tx_hash);
```

The relay node makes a JSON-RPC call:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "sendTransaction",
  "params": {
    "transaction": "AAAAAgAAAAD..."
  }
}
```

#### Step 4: Handle the Response

```rust
match rpc_client.submit_transaction(tx_xdr).await {
    Ok(tx_hash) => {
        log::info!("✓ Transaction {} submitted successfully", tx_hash);
    }
    Err(e) => {
        log::error!("✗ Failed to submit transaction: {:?}", e);
        // Continue processing other envelopes
    }
}
```

### Complete Relay Processing Function

```rust
use stellarconduit_core::relay::process_envelope;
use stellarconduit_core::relay::rpc_client::RpcClient;
use stellarconduit_core::message::types::TransactionEnvelope;

async fn relay_transaction(
    rpc_client: &RpcClient,
    envelope: TransactionEnvelope,
) -> Result<String, Box<dyn std::error::Error>> {
    // Verify and submit
    let tx_hash = process_envelope(rpc_client, &envelope).await?;
    Ok(tx_hash)
}
```


### Origin Public Key vs. Stellar Source Account

**Important Distinction:**

- `origin_pubkey`: The Ed25519 public key of the mesh device (32 bytes)
- `source_account`: The Stellar account in the XDR transaction (32 bytes)

These are **typically different**:

1. The `origin_pubkey` identifies the device in the mesh network
2. The `source_account` (inside the XDR) is the Stellar account paying fees

### Fee Sponsorship Model

In most cases, the mobile device does not have XLM to pay transaction fees. The relay node can sponsor fees:

1. **Mobile Device**: Creates a transaction with `source_account` = relay's Stellar account
2. **Relay Node**: Verifies the signature and submits the transaction
3. **Stellar Network**: Deducts fees from the relay's account

Alternatively, the mobile device can use [fee-bump transactions](https://developers.stellar.org/docs/encyclopedia/fee-bump-transactions) where the relay wraps the original transaction.

### Security Implications

- The relay node trusts the mesh signature, not the Stellar signature
- The relay node can rate-limit submissions per `origin_pubkey`
- The relay node should validate the XDR before submission to prevent spam

## Security Considerations

### Attack Vectors and Mitigations

#### 1. Replay Attacks

**Attack**: Rebroadcast an old valid transaction.

**Mitigation**: 
- Check the `timestamp` field
- Reject messages older than a threshold (e.g., 5 minutes)
- Track seen `message_id` values in a bloom filter

```rust
const MAX_MESSAGE_AGE_SECS: u64 = 300; // 5 minutes

fn is_message_too_old(envelope: &TransactionEnvelope) -> bool {
    let now = current_unix_timestamp();
    let age = now.saturating_sub(envelope.timestamp);
    age > MAX_MESSAGE_AGE_SECS
}
```

#### 2. Signature Forgery

**Attack**: Create a transaction claiming to be from another device.

**Mitigation**: 
- Always verify the Ed25519 signature
- Reject any message with an invalid signature
- Never trust the `origin_pubkey` field alone

#### 3. Message Tampering

**Attack**: Modify a message in transit (e.g., change the XDR).

**Mitigation**: 
- The signature covers the entire payload
- Any modification invalidates the signature
- Intermediate nodes cannot tamper without detection


#### 4. Denial of Service (DoS)

**Attack**: Flood the network with valid but useless messages.

**Mitigation**:
- Implement rate limiting per `origin_pubkey`
- Use TTL to prevent infinite propagation
- Relay nodes can blacklist abusive peers

```rust
use std::collections::HashMap;
use std::time::{Duration, Instant};

struct RateLimiter {
    requests: HashMap<[u8; 32], Vec<Instant>>,
    max_per_minute: usize,
}

impl RateLimiter {
    fn check_rate_limit(&mut self, pubkey: &[u8; 32]) -> bool {
        let now = Instant::now();
        let cutoff = now - Duration::from_secs(60);
        
        let timestamps = self.requests.entry(*pubkey).or_insert_with(Vec::new);
        timestamps.retain(|&t| t > cutoff);
        
        if timestamps.len() >= self.max_per_minute {
            return false; // Rate limit exceeded
        }
        
        timestamps.push(now);
        true
    }
}
```

#### 5. Eclipse Attacks

**Attack**: Isolate a device by controlling all its peer connections.

**Mitigation**:
- Connect to multiple diverse peers
- Implement peer discovery mechanisms
- Periodically rotate connections

#### 6. Sybil Attacks

**Attack**: Create many fake identities to gain influence.

**Mitigation**:
- Reputation systems based on successful transactions
- Relay nodes can require proof-of-work for new peers
- Limit the number of connections per IP address

### Best Practices

1. **Always Verify Signatures**: Never trust unverified messages
2. **Validate Timestamps**: Reject messages that are too old or from the future
3. **Rate Limit**: Prevent spam by limiting messages per peer
4. **Use TTL**: Prevent infinite message propagation
5. **Secure Key Storage**: Store private keys in hardware-backed storage
6. **Monitor Anomalies**: Track unusual patterns (e.g., sudden spike in messages)
7. **Update Regularly**: Keep cryptographic libraries up to date

## Implementation Examples

### Android (Kotlin)

```kotlin
import org.bouncycastle.crypto.params.Ed25519PrivateKeyParameters
import org.bouncycastle.crypto.signers.Ed25519Signer
import java.security.MessageDigest
import java.nio.ByteBuffer

fun createSignedEnvelope(
    privateKey: Ed25519PrivateKeyParameters,
    txXdr: String
): TransactionEnvelope {
    val publicKey = privateKey.generatePublicKey().encoded
    val timestamp = System.currentTimeMillis() / 1000
    
    // Construct payload
    val buffer = ByteBuffer.allocate(32 + 8 + txXdr.length)
    buffer.put(publicKey)
    buffer.putLong(timestamp)
    buffer.put(txXdr.toByteArray(Charsets.UTF_8))
    
    // Hash the payload
    val digest = MessageDigest.getInstance("SHA-256")
    val payloadHash = digest.digest(buffer.array())
    
    // Sign the hash
    val signer = Ed25519Signer()
    signer.init(true, privateKey)
    signer.update(payloadHash, 0, payloadHash.size)
    val signature = signer.generateSignature()
    
    return TransactionEnvelope(
        messageId = payloadHash,
        originPubkey = publicKey,
        txXdr = txXdr,
        ttlHops = 10,
        timestamp = timestamp,
        signature = signature
    )
}
```


### iOS (Swift)

```swift
import CryptoKit
import Foundation

func createSignedEnvelope(
    privateKey: Curve25519.Signing.PrivateKey,
    txXdr: String
) throws -> TransactionEnvelope {
    let publicKey = privateKey.publicKey.rawRepresentation
    let timestamp = UInt64(Date().timeIntervalSince1970)
    
    // Construct payload
    var payload = Data()
    payload.append(publicKey)
    payload.append(withUnsafeBytes(of: timestamp.bigEndian) { Data($0) })
    payload.append(txXdr.data(using: .utf8)!)
    
    // Hash the payload
    let payloadHash = SHA256.hash(data: payload)
    
    // Sign the hash
    let signature = try privateKey.signature(for: Data(payloadHash))
    
    return TransactionEnvelope(
        messageId: Data(payloadHash),
        originPubkey: publicKey,
        txXdr: txXdr,
        ttlHops: 10,
        timestamp: timestamp,
        signature: signature
    )
}
```

### JavaScript/TypeScript (Node.js)

```typescript
import * as ed25519 from '@noble/ed25519';
import { sha256 } from '@noble/hashes/sha256';

async function createSignedEnvelope(
    privateKey: Uint8Array,
    txXdr: string
): Promise<TransactionEnvelope> {
    const publicKey = await ed25519.getPublicKey(privateKey);
    const timestamp = Math.floor(Date.now() / 1000);
    
    // Construct payload
    const timestampBytes = new Uint8Array(8);
    new DataView(timestampBytes.buffer).setBigUint64(0, BigInt(timestamp), false); // big-endian
    
    const txXdrBytes = new TextEncoder().encode(txXdr);
    
    const payload = new Uint8Array([
        ...publicKey,
        ...timestampBytes,
        ...txXdrBytes
    ]);
    
    // Hash the payload
    const payloadHash = sha256(payload);
    
    // Sign the hash
    const signature = await ed25519.sign(payloadHash, privateKey);
    
    return {
        messageId: payloadHash,
        originPubkey: publicKey,
        txXdr: txXdr,
        ttlHops: 10,
        timestamp: timestamp,
        signature: signature
    };
}
```

## Testing and Validation

### Test Vectors

To ensure interoperability, use these test vectors:

#### Test Vector 1: Minimal Transaction

**Input:**
```
origin_pubkey: [0x02; 32]
timestamp: 1672531200 (2023-01-01 00:00:00 UTC)
tx_xdr: "AAAAAgAAAAA="
```

**Expected Payload Hash (SHA-256):**
```
0x8f4e7c9d2a1b3f5e6d8c9a7b5e3f1d2c4a6b8e9f1a3c5d7e9f2b4c6d8e1a3b5c
```

(Note: This is a placeholder - run the actual implementation to get the real hash)

#### Test Vector 2: Full Transaction

Use the test cases in `src/message/types.rs` as reference implementations.

### Validation Checklist

When implementing a client, verify:

- [ ] Ed25519 keypair generation works correctly
- [ ] Timestamp is encoded as big-endian uint64
- [ ] Payload hash includes all three fields in the correct order
- [ ] Signature verification accepts valid signatures
- [ ] Signature verification rejects invalid signatures
- [ ] MessagePack serialization round-trips correctly
- [ ] Envelope size is under 500 bytes for typical transactions


## Frequently Asked Questions

### Q: Why is the timestamp in big-endian format?

**A:** Big-endian (network byte order) is the standard for network protocols. It ensures consistent byte ordering across different architectures (x86, ARM, etc.).

### Q: Can I use a different hash function instead of SHA-256?

**A:** No. The protocol requires SHA-256 for interoperability. All clients must use the same hash function.

### Q: What happens if two devices generate the same message_id?

**A:** The `message_id` is a SHA-256 hash, which has a 256-bit output space. The probability of collision is astronomically low (2^-256). In practice, this never happens.

### Q: Can I modify the TTL after signing?

**A:** No. The TTL is not part of the signed payload, but modifying it after signing is not recommended. Future versions may include TTL in the signature.

### Q: How do I handle clock skew between devices?

**A:** Accept messages with timestamps within a reasonable window (e.g., ±5 minutes). Devices should sync their clocks periodically using NTP.

### Q: What if the relay node is malicious?

**A:** A malicious relay can:
- Refuse to submit transactions (censorship)
- Submit transactions to the wrong network (testnet vs mainnet)

A malicious relay CANNOT:
- Forge transactions (requires the private key)
- Modify transactions (invalidates the signature)
- Steal funds (the transaction is already signed by the user)

**Mitigation**: Use multiple relay nodes and implement reputation systems.

### Q: Can I compress the tx_xdr field?

**A:** The protocol does not currently support compression. The tx_xdr must be base64-encoded XDR as specified by Stellar.

### Q: How do I handle message deduplication?

**A:** Use the `message_id` field. Store seen message IDs in a bloom filter or LRU cache. Reject messages with duplicate IDs.

### Q: What is the maximum message size?

**A:** There is no hard limit, but practical considerations:
- BLE has a ~512-byte MTU (requires fragmentation for larger messages)
- WiFi Direct can handle larger messages
- Keep messages under 1 KB for optimal performance

### Q: Can I add custom fields to the TransactionEnvelope?

**A:** No. Adding fields breaks interoperability. If you need custom data, encode it in the Stellar transaction memo field.

## References

### Specifications

- [Ed25519 (RFC 8032)](https://datatracker.ietf.org/doc/html/rfc8032)
- [SHA-256 (FIPS 180-4)](https://csrc.nist.gov/publications/detail/fips/180/4/final)
- [MessagePack Specification](https://github.com/msgpack/msgpack/blob/master/spec.md)
- [Stellar XDR Format](https://developers.stellar.org/docs/encyclopedia/xdr)

### Libraries

- **Rust**: `ed25519-dalek`, `sha2`, `rmp-serde`
- **Python**: `PyNaCl`, `hashlib`, `msgpack`
- **JavaScript**: `@noble/ed25519`, `@noble/hashes`, `msgpack-lite`
- **Android**: `BouncyCastle`, `MessageDigest`
- **iOS**: `CryptoKit`

### Related Documentation

- [Gossip Protocol](./gossip-protocol.md)
- [Transport Layer](./transport-layer.md)
- [Relay Node Implementation](./relay-node.md)

## Changelog

### Version 1.0 (Current)

- Initial specification
- Ed25519 signatures over SHA-256 hashes
- MessagePack serialization
- Four message types: Transaction, TopologyUpdate, SyncRequest, SyncResponse

### Future Considerations

- **Compression**: Add optional zstd compression for large messages
- **Batching**: Allow multiple transactions in a single envelope
- **Encryption**: Add optional end-to-end encryption for privacy
- **TTL in Signature**: Include TTL in the signed payload to prevent TTL manipulation

## Contributing

Found an error or ambiguity in this specification? Please open an issue or submit a pull request.

When proposing changes:
1. Ensure backward compatibility or propose a version bump
2. Update test vectors
3. Provide reference implementations
4. Document migration paths for existing clients

---

**Document Version**: 1.0  
**Last Updated**: 2026-03-10  
**Status**: Final  
**Authors**: StellarConduit Core Team
