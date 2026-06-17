# Noise Protocol Transport Encryption

This document describes the Noise Protocol XX encryption layer implemented in `src/security/encryption.rs`.

## Overview

All data transmitted over BLE and WiFi-Direct connections is encrypted using the **Noise Protocol XX pattern** with ChaCha20-Poly1305 AEAD. This provides:

- **Mutual Authentication**: Both peers prove identity via Ed25519 signatures converted to X25519
- **Forward Secrecy**: Ephemeral session keys ensure past traffic cannot be decrypted if long-term keys are compromised
- **Replay Protection**: Noise protocol's counter-based nonce prevents replay attacks
- **Confidentiality**: ChaCha20-Poly1305 encryption hides message contents from eavesdroppers

## Architecture

### EncryptedConnection Wrapper

The `EncryptedConnection<C>` struct wraps any `Connection` implementation (BLE, WiFi-Direct) and transparently encrypts/decrypts all `ProtocolMessage` traffic:

```rust
pub struct EncryptedConnection<C: Connection> {
    inner: C,
    noise_state: snow::TransportState,
    remote_public_key: [u8; 32],
}
```

The wrapper:

1. Stores the inner connection for physical transport
2. Maintains Noise protocol state after handshake
3. Tracks the remote peer's long-term public key for identity verification

### Key Conversion: Ed25519 → X25519

Mesh devices use Ed25519 for message signing, but Noise Protocol requires X25519 for elliptic-curve Diffie-Hellman. The conversion follows **RFC 7748**:

```rust
pub fn ed25519_to_x25519(signing_key: &SigningKey) -> Result<[u8; 32], EncryptionError>
```

**Implementation**:

1. Hash the Ed25519 seed with SHA-512
2. Apply RFC 7748 clamping to the first 32 bytes
3. Return clamped bytes as X25519 secret

**Clamping formula**:

```
x25519_bytes[0] &= 248
x25519_bytes[31] &= 127
x25519_bytes[31] |= 64
```

This ensures the scalar is in the correct range for X25519 operations.

## Handshake Protocol

### Initiator (Outbound Connection)

```rust
pub async fn handshake_initiator(
    inner: C,
    local_key: &SigningKey,
) -> Result<Self, EncryptionError>
```

**Message Exchange**:

```
Initiator                          Responder
    |                                 |
    |--- Noise Message 1 (empty) ---->|
    |                                 |
    |<--- Noise Message 2 (remote_pk) |
    |                                 |
    |--- Noise Message 3 (empty) ---->|
    |                                 |
    <--- Encrypted Transport Mode --->|
```

1. **Message 1**: Initiator sends ephemeral public key
2. **Message 2**: Responder sends ephemeral key + long-term public key (encrypted)
3. **Message 3**: Initiator sends long-term public key (encrypted)
4. Both sides derive shared session keys and transition to transport mode

### Responder (Incoming Connection)

```rust
pub async fn handshake_responder(
    inner: C,
    local_key: &SigningKey,
) -> Result<Self, EncryptionError>
```

Same 3-message handshake, with roles reversed.

### Handshake Timeout

Both initiator and responder must complete the handshake within **2 seconds**:

```rust
pub const HANDSHAKE_TIMEOUT_SECS: u64 = 2;
```

If timeout occurs, `EncryptionError::HandshakeTimeout` is returned and the connection is dropped.

### Peer Identity Verification

After handshake, verify that the remote peer's public key matches the expected identity:

```rust
pub fn verify_peer_identity(&self, expected_peer: &PeerIdentity) -> Result<(), EncryptionError>
```

If the remote key doesn't match, return `EncryptionError::PeerPublicKeyMismatch` and disconnect.

## Message Encryption & Decryption

### Encryption

```rust
async fn encrypt_message(&mut self, msg: &ProtocolMessage) -> Result<Vec<u8>, EncryptionError>
```

1. Serialize `ProtocolMessage` to MessagePack bytes
2. Check message size ≤ 65535 bytes (Noise spec maximum)
3. Encrypt with Noise `write_message()` using ChaCha20-Poly1305
4. Return ciphertext with Poly1305 authentication tag

### Decryption

```rust
async fn decrypt_message(&mut self, ciphertext: &[u8]) -> Result<ProtocolMessage, EncryptionError>
```

1. Decrypt with Noise `read_message()` using ChaCha20-Poly1305
2. On MAC verification failure, return `EncryptionError::AuthenticationFailed`
3. Deserialize plaintext back to `ProtocolMessage`

**Authentication Failure Handling**: If the peer sends tampered ciphertext, the MAC verification fails in `read_message()`. This is treated as a severe error:

- The connection is immediately dropped
- `EncryptionError::AuthenticationFailed` is raised
- No attempt to recover or retry

## Wire Protocol

### Frame Format

Each Noise message is prefixed with a **2-byte big-endian length prefix**:

```
[Length: u16 BE] [Noise Message: variable]
```

**Example**:

- Message: `[0x01, 0x02, 0x03]` (3 bytes)
- Framed: `[0x00, 0x03, 0x01, 0x02, 0x03]` (5 bytes total)

```rust
pub fn frame_noise_message(msg: &[u8]) -> Result<Vec<u8>, EncryptionError>
```

### Message Chunking

Messages larger than the transport MTU must be chunked **before** encryption. The existing `MessageChunker` in `unified.rs` handles this:

```
ProtocolMessage → Serialize → Chunk (if needed) → Encrypt → Frame → Send
```

## Integration with TransportManager

To integrate `EncryptedConnection` into the transport layer:

```rust
// After establishing a physical connection
let conn = Box::new(WifiDirectConnection::connect_to(...).await?);

// Wrap with encryption (requires local signing key)
let enc_conn = EncryptedConnection::handshake_initiator(
    conn,
    &local_signing_key,
).await?;

// Verify remote peer identity
enc_conn.verify_peer_identity(&expected_peer)?;

// Store in TransportManager
transport_manager.active_connections.insert(peer_pubkey, Box::new(enc_conn));
```

## Error Handling

### EncryptionError Types

```rust
pub enum EncryptionError {
    HandshakeFailed(String),
    HandshakeTimeout,
    AuthenticationFailed,
    EncryptionFailed(String),
    DecryptionFailed(String),
    KeyConversionFailed(String),
    TransportError(TransportError),
    InvalidMessageSize(String),
    PeerPublicKeyMismatch,
}
```

### Conversion to TransportError

Encryption errors convert to transport errors for upper-layer handling:

```rust
impl From<EncryptionError> for TransportError {
    fn from(err: EncryptionError) -> Self {
        match err {
            EncryptionError::TransportError(te) => te,
            EncryptionError::HandshakeTimeout => TransportError::Timeout,
            _ => TransportError::BrokenPipe,
        }
    }
}
```

## Security Properties

### Threat Model

The Noise Protocol XX pattern protects against:

1. **Eavesdropping**: Ciphertext is unreadable without session keys
2. **Message Tampering**: Poly1305 MAC detects any bit-flip
3. **Replay Attacks**: Nonce counter prevents replayed messages
4. **Man-in-the-Middle**: Mutual authentication via long-term keys
5. **Identity Impersonation**: Remote peer's public key verified after handshake

### What It Does NOT Protect

- **Denial of Service**: An attacker can still drop connections or send invalid handshake messages
- **Traffic Analysis**: Connection patterns and message sizes are visible
- **Implementation Bugs**: If the underlying crypto library is compromised
- **Future Decryption**: If X25519 secrets are broken via quantum computing (post-quantum crypto out of scope)

## Performance Characteristics

### Handshake Latency

- **Requirement**: < 10ms on loopback
- **Implementation**: 3 round-trips, no blocking operations
- Timeout: 2 seconds (conservative for lossy wireless links)

### Encryption Overhead

- **ChaCha20-Poly1305**: 16-byte authentication tag per message
- **Framing**: 2-byte length prefix
- **Total overhead**: 18 bytes per encrypted message

### Throughput

On BLE (244-byte MTU):

- Effective payload: 244 - 14 (chunk header) - 18 (encryption) = 212 bytes
- Similar calculations for WiFi-Direct with larger MTU

## Testing

### Unit Tests

Located in `src/security/encryption.rs::tests`:

```bash
cargo test --lib security::encryption
```

Tests cover:

- Ed25519 → X25519 conversion and RFC 7748 clamping
- Frame/unframe round-trips
- Oversized message rejection
- Error type conversions
- Peer identity mismatch detection

### Integration Tests

Located in `tests/encryption_integration_test.rs`:

```bash
cargo test --test encryption_integration_test
```

Tests cover:

- End-to-end message serialization integrity
- Large message handling (up to 65KB)
- Error propagation through transport layer
- X25519 conversion consistency

## Deployment Considerations

### Local Key Management

The `SigningKey` (Ed25519 private key) must be:

1. Generated once and stored securely (Keychain, Keystore, or secure enclave)
2. Never transmitted over the network
3. Protected from memory access via (if possible) locked memory
4. Rotated per device per reset/reinstall

### Peer Bootstrapping

Before initiating a connection:

1. Obtain the remote peer's Ed25519 public key (from relay registry or discovery)
2. Create `PeerIdentity` from that public key
3. Call `handshake_initiator()` to establish encrypted connection
4. Call `verify_peer_identity()` to confirm the peer matches expectations

### Connection Pooling

Each peer typically has one encrypted connection. If a connection fails:

1. Drop the `EncryptedConnection`
2. Remove from `TransportManager.active_connections`
3. On next message, re-establish via `connect()` → `handshake_initiator()`

## Cryptographic Constants

All constants are hard-coded per specification:

| Constant            | Value                              | Source     |
| ------------------- | ---------------------------------- | ---------- |
| Noise Pattern       | `Noise_XX_25519_ChaChaPoly_SHA256` | Noise Spec |
| DH Function         | X25519                             | RFC 7748   |
| AEAD Cipher         | ChaCha20-Poly1305                  | RFC 8439   |
| Hash Function       | SHA-256                            | FIPS 180-4 |
| Max Message Size    | 65535 bytes                        | Noise Spec |
| Handshake Timeout   | 2 seconds                          | Issue #90  |
| Frame Length Prefix | 2 bytes, big-endian                | Custom     |

## Backward Compatibility

Currently, **all new connections use Noise encryption**. Legacy plaintext connections are not supported.

If plaintext fallback is needed in the future:

1. Add a `require_encryption: bool` flag to `TransportManager`
2. Wrap connections conditionally: `if require_encryption { EncryptedConnection::... }`
3. Ensure strict version negotiation to prevent downgrade attacks

## Future Improvements

1. **Connection Resumption**: Cache derived session keys to skip re-handshake on reconnect
2. **Key Rotation**: Implement periodic long-term key rotation
3. **Rate Limiting**: Add per-peer rate limiting during handshake phase
4. **Observability**: Emit metrics for handshake latency, encryption overhead
5. **Post-Quantum**: Evaluate hybrid schemes combining X25519 + post-quantum DH

## References

- [Noise Protocol Framework](https://noiseprotocol.org/)
- [RFC 7748: Elliptic Curves for Security](https://tools.ietf.org/html/rfc7748)
- [RFC 8439: ChaCha20 and Poly1305](https://tools.ietf.org/html/rfc8439)
- [Snow Crate (Noise Implementation)](https://github.com/mcginty/snow)
- [Ed25519 RFC 8032](https://tools.ietf.org/html/rfc8032)
