//! BLE GATT transport backend for StellarConduit.
//!
//! Provides `BlePeripheral` (GATT server / advertiser) and `BleCentral` (GATT client / scanner).
//! Both implement the `Connection` trait from `transport::connection`.
//!
//! Platform note: requires `btleplug` and a hardware or virtual BLE adapter at runtime.
//! Unit tests cover state-machine and chunking logic only; BLE integration tests need a real adapter.

use async_trait::async_trait;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use uuid::Uuid;

/// Test-only: number of consecutive times `BlePeripheral::start_advertising`
/// should fail before succeeding.
#[cfg(test)]
static ADVERTISE_FAIL_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Set the number of times `start_advertising` should fail (for testing retry logic).
#[cfg(test)]
pub fn set_advertise_fail_count(n: usize) {
    ADVERTISE_FAIL_COUNT.store(n, Ordering::SeqCst);
}

use crate::message::types::ProtocolMessage;
use crate::peer::identity::PeerIdentity;
use crate::transport::connection::{Connection, ConnectionState, TransportType};
use crate::transport::errors::TransportError;
use crate::transport::unified::{ChunkFrame, MessageChunker, MessageReassembler};

// ─── StellarConduit BLE Service UUIDs ────────────────────────────────────────

/// Primary StellarConduit GATT Service UUID
pub const SC_SERVICE_UUID: Uuid = Uuid::from_u128(0x00de_adbe_efca_feba_be00_0000_0000_0001_u128);

/// Write Characteristic — Central peers write inbound chunk frames here.
pub const SC_WRITE_CHAR_UUID: Uuid =
    Uuid::from_u128(0x00de_adbe_efca_feba_be00_0000_0000_0002_u128);

/// Notify Characteristic — Peripheral notifies connected Centrals with outbound chunk frames.
pub const SC_NOTIFY_CHAR_UUID: Uuid =
    Uuid::from_u128(0x00de_adbe_efca_feba_be00_0000_0000_0003_u128);

/// BLE MTU used for chunking. The BLE 4.2+ spec allows up to 517 bytes per ATT packet,
/// but we use a conservative 244 bytes (common negotiated value).
pub const BLE_ATT_MTU: usize = 244;

// ─── MAC Address Provider ─────────────────────────────────────────────────────

/// Trait for injectable MAC address generation.
/// Allows pluggable MAC address providers for testing and platform-specific implementations.
pub trait MacAddressProvider: Send + Sync {
    /// Returns a 6-byte random MAC address with the locally-administered bit set.
    /// The address must have bit 1 of byte 0 set (locally-administered bit).
    fn generate(&self) -> [u8; 6];
}

/// Default implementation that generates random MAC addresses with the locally-administered bit set.
pub struct RandomMacProvider;

impl MacAddressProvider for RandomMacProvider {
    fn generate(&self) -> [u8; 6] {
        use rand::random;
        let mut mac = random::<[u8; 6]>();
        mac[0] |= 0b00000010; // Set locally-administered bit
        mac[0] &= 0b11111110; // Clear multicast bit
        mac
    }
}

// ─── ChunkFrame wire encoding helpers ────────────────────────────────────────

/// Encodes a `ChunkFrame` into raw bytes for transmission over a BLE characteristic.
pub fn encode_chunk(frame: &ChunkFrame) -> Vec<u8> {
    let mut buf = Vec::with_capacity(14 + frame.payload.len());
    buf.extend_from_slice(&frame.message_id.to_le_bytes());
    buf.extend_from_slice(&frame.total_length.to_le_bytes());
    buf.extend_from_slice(&frame.offset.to_le_bytes());
    buf.extend_from_slice(&frame.payload_size.to_le_bytes());
    buf.extend_from_slice(&frame.payload);
    buf
}

/// Decodes raw bytes from a BLE characteristic into a `ChunkFrame`.
/// Returns `None` if the buffer is malformed.
pub fn decode_chunk(data: &[u8]) -> Option<ChunkFrame> {
    if data.len() < 14 {
        return None;
    }

    let message_id = u32::from_le_bytes(data[0..4].try_into().ok()?);
    let total_length = u32::from_le_bytes(data[4..8].try_into().ok()?);
    let offset = u32::from_le_bytes(data[8..12].try_into().ok()?);
    let payload_size = u16::from_le_bytes(data[12..14].try_into().ok()?);
    let payload = data[14..].to_vec();

    if payload.len() != payload_size as usize {
        return None;
    }

    Some(ChunkFrame {
        message_id,
        total_length,
        offset,
        payload_size,
        payload,
    })
}

// ─── BlePeripheral ────────────────────────────────────────────────────────────

/// GATT Server (Peripheral / Advertiser) side of a BLE connection.
///
/// Advertises `SC_SERVICE_UUID` and exposes the Write + Notify characteristics.
/// Incoming chunk frames written to the Write Characteristic are reassembled
/// into complete `ProtocolMessage`s and delivered via `recv()`.
pub struct BlePeripheral {
    state: ConnectionState,
    remote_peer: PeerIdentity,
    chunker: MessageChunker,
    reassembler: Arc<Mutex<MessageReassembler>>,
    /// Inbox: reassembled raw message bytes ready to be deserialized.
    inbox_tx: mpsc::Sender<Vec<u8>>,
    inbox_rx: mpsc::Receiver<Vec<u8>>,
}

impl BlePeripheral {
    /// Construct a `BlePeripheral` in `Disconnected` state.
    /// Call `connect()` (or `start_advertising()`) to begin advertising.
    pub fn new(remote_peer: PeerIdentity) -> Self {
        let (tx, rx) = mpsc::channel(64);
        Self {
            state: ConnectionState::Disconnected,
            remote_peer,
            chunker: MessageChunker { mtu: BLE_ATT_MTU },
            reassembler: Arc::new(Mutex::new(MessageReassembler::new())),
            inbox_tx: tx,
            inbox_rx: rx,
        }
    }

    /// Start advertising `SC_SERVICE_UUID` and expose the GATT service.
    ///
    /// In a real device context this method would:
    /// 1. Acquire a `btleplug` manager and adapter.
    /// 2. Build and register a GATT service descriptor with Write + Notify characteristics.
    /// 3. Begin BLE advertising with the local `PeerIdentity` pubkey in the manufacturer data.
    /// 4. Spawn an async task that listens for characteristic writes, decodes ChunkFrames,
    ///    feeds them to the `MessageReassembler`, and pushes complete messages to the inbox channel.
    ///
    /// Returns `Err(TransportError::ConnectionRefused)` if no BLE adapter is available.
    pub async fn start_advertising(&mut self) -> Result<(), TransportError> {
        #[cfg(test)]
        {
            let prev = ADVERTISE_FAIL_COUNT.load(Ordering::SeqCst);
            if prev > 0 {
                ADVERTISE_FAIL_COUNT.store(prev - 1, Ordering::SeqCst);
                return Err(TransportError::ConnectionRefused);
            }
        }
        // Platform integration: btleplug adapter acquisition would happen here.
        // For now we transition state to Connected to allow unit-testable logic.
        self.state = ConnectionState::Connected;
        Ok(())
    }

    /// Feed a raw encoded chunk frame (as received from the Write Characteristic) into
    /// the reassembler. If the frame completes a message, it is pushed to the inbox.
    ///
    /// This would be called from the btleplug characteristic write callback.
    pub async fn ingest_chunk_bytes(&mut self, data: &[u8]) -> Result<(), TransportError> {
        let frame = decode_chunk(data).ok_or(TransportError::BrokenPipe)?;
        let mut reassembler = self.reassembler.lock().await;
        if let Some(bytes) = reassembler.receive_chunk(frame) {
            self.inbox_tx
                .send(bytes)
                .await
                .map_err(|_| TransportError::BrokenPipe)?;
        }
        Ok(())
    }
}

#[async_trait]
impl Connection for BlePeripheral {
    fn remote_peer(&self) -> PeerIdentity {
        self.remote_peer.clone()
    }

    fn transport_type(&self) -> TransportType {
        TransportType::Ble
    }

    fn state(&self) -> ConnectionState {
        self.state
    }

    async fn connect(&mut self) -> Result<(), TransportError> {
        self.start_advertising().await
    }

    async fn send(&mut self, msg: ProtocolMessage) -> Result<(), TransportError> {
        if self.state != ConnectionState::Connected {
            return Err(TransportError::NotConnected);
        }

        let bytes = rmp_serde::to_vec(&msg).map_err(|_| TransportError::BrokenPipe)?;
        let frames = self.chunker.chunk(&bytes);

        for frame in frames {
            let _encoded = encode_chunk(&frame);
            // Platform integration: write `encoded` to the Notify Characteristic
            // via the btleplug peripheral handle so connected Centrals are notified.
        }

        Ok(())
    }

    async fn recv(&mut self) -> Result<ProtocolMessage, TransportError> {
        if self.state != ConnectionState::Connected {
            return Err(TransportError::NotConnected);
        }

        let bytes = self
            .inbox_rx
            .recv()
            .await
            .ok_or(TransportError::BrokenPipe)?;
        let msg = rmp_serde::from_slice(&bytes).map_err(|_| TransportError::BrokenPipe)?;
        Ok(msg)
    }

    async fn disconnect(&mut self) -> Result<(), TransportError> {
        self.state = ConnectionState::Disconnected;
        // Platform integration: stop BLE advertising and release the adapter.
        Ok(())
    }
}

// ─── BleCentral ───────────────────────────────────────────────────────────────

/// GATT Client (Central / Scanner) side of a BLE connection.
///
/// Scans for peripherals advertising `SC_SERVICE_UUID` and connects to a specific
/// device identified by its `PeerIdentity`. Sends messages by writing `ChunkFrame`s
/// to the remote peripheral's Write Characteristic.
pub struct BleCentral {
    state: ConnectionState,
    remote_peer: PeerIdentity,
    local_peer: PeerIdentity,
    chunker: MessageChunker,
    inbox_tx: mpsc::Sender<Vec<u8>>,
    inbox_rx: mpsc::Receiver<Vec<u8>>,
    current_mac: [u8; 6],
    mac_provider: Box<dyn MacAddressProvider>,
    last_rotation: Instant,
    rotation_interval: Duration,
}

impl BleCentral {
    /// Construct a `BleCentral` in `Disconnected` state.
    /// Call `connect()` (or `scan_and_connect()`) to begin scanning.
    pub fn new(remote_peer: PeerIdentity) -> Self {
        let (tx, rx) = mpsc::channel(64);
        let mac_provider = Box::new(RandomMacProvider);
        let initial_mac = mac_provider.generate();
        Self {
            state: ConnectionState::Disconnected,
            remote_peer,
            local_peer: PeerIdentity::new([0u8; 32]),
            chunker: MessageChunker { mtu: BLE_ATT_MTU },
            inbox_tx: tx,
            inbox_rx: rx,
            current_mac: initial_mac,
            mac_provider,
            last_rotation: Instant::now(),
            rotation_interval: Duration::from_secs(15 * 60), // 15 minutes default
        }
    }

    /// Set the local peer identity so the Handshake is sent with the correct pubkey.
    pub fn with_local_peer(mut self, local: PeerIdentity) -> Self {
        self.local_peer = local;
        self
    }

    /// Rotate the BLE Central's MAC address if the rotation interval has elapsed.
    ///
    /// Returns `Some(new_mac)` if a rotation occurred, or `None` if the interval has not yet elapsed.
    /// The new MAC address has the locally-administered bit set (bit 1 of byte 0).
    pub fn rotate_mac_if_due(&mut self) -> Option<[u8; 6]> {
        if self.last_rotation.elapsed() >= self.rotation_interval {
            let new_mac = self.mac_provider.generate();
            self.current_mac = new_mac;
            self.last_rotation = Instant::now();
            Some(new_mac)
        } else {
            None
        }
    }

    /// Set the MAC address provider for testing or platform-specific implementations.
    #[cfg(test)]
    pub fn with_mac_provider(mut self, provider: Box<dyn MacAddressProvider>) -> Self {
        self.mac_provider = provider;
        self
    }

    /// Set the rotation interval for testing.
    #[cfg(test)]
    pub fn with_rotation_interval(mut self, interval: Duration) -> Self {
        self.rotation_interval = interval;
        self
    }

    /// Get the current MAC address.
    pub fn current_mac(&self) -> [u8; 6] {
        self.current_mac
    }

    /// Scan for BLE peripherals advertising `SC_SERVICE_UUID` and connect to the one
    /// whose manufacturer advertisement data matches `target.pubkey`.
    ///
    /// In a real device context this method would:
    /// 1. Acquire a `btleplug` manager and adapter.
    /// 2. Start a BLE scan filtered to `SC_SERVICE_UUID`.
    /// 3. Match discovered peripherals by decoding the manufacturer data pubkey field.
    /// 4. Connect to the matching peripheral, discover characteristics.
    /// 5. Subscribe to the Notify Characteristic, spawning a task that feeds inbound frames
    ///    to the inbox channel via `decode_chunk` / `MessageReassembler`.
    /// 6. Send a `ProtocolMessage::Handshake` as the first message.
    ///
    /// Returns `Err(TransportError::ConnectionRefused)` if the target cannot be found.
    pub async fn scan_and_connect(&mut self) -> Result<(), TransportError> {
        // Platform integration: btleplug scan + connect would happen here.
        self.state = ConnectionState::Connected;

        // Send Handshake as the first message after connecting.
        let handshake = ProtocolMessage::Handshake {
            pubkey: self.local_peer.pubkey,
        };
        let bytes = rmp_serde::to_vec(&handshake).map_err(|_| TransportError::BrokenPipe)?;
        let frames = self.chunker.chunk(&bytes);
        for frame in frames {
            let _encoded = encode_chunk(&frame);
            // Platform integration: write `_encoded` to the remote Peripheral's
            // Write Characteristic via the btleplug central handle.
        }

        Ok(())
    }

    /// Feed a raw notification chunk frame received from the Notify Characteristic
    /// into the message inbox. Called from the btleplug notification listener task.
    pub async fn ingest_notification_bytes(
        &self,
        data: &[u8],
        reassembler: &mut MessageReassembler,
    ) -> Result<(), TransportError> {
        let frame = decode_chunk(data).ok_or(TransportError::BrokenPipe)?;
        if let Some(bytes) = reassembler.receive_chunk(frame) {
            self.inbox_tx
                .send(bytes)
                .await
                .map_err(|_| TransportError::BrokenPipe)?;
        }
        Ok(())
    }
}

#[async_trait]
impl Connection for BleCentral {
    fn remote_peer(&self) -> PeerIdentity {
        self.remote_peer.clone()
    }

    fn transport_type(&self) -> TransportType {
        TransportType::Ble
    }

    fn state(&self) -> ConnectionState {
        self.state
    }

    async fn connect(&mut self) -> Result<(), TransportError> {
        self.scan_and_connect().await
    }

    async fn send(&mut self, msg: ProtocolMessage) -> Result<(), TransportError> {
        if self.state != ConnectionState::Connected {
            return Err(TransportError::NotConnected);
        }

        let bytes = rmp_serde::to_vec(&msg).map_err(|_| TransportError::BrokenPipe)?;
        let frames = self.chunker.chunk(&bytes);

        for frame in frames {
            let _encoded = encode_chunk(&frame);
            // Platform integration: write `encoded` to the remote Peripheral's
            // Write Characteristic via the btleplug central handle.
        }

        Ok(())
    }

    async fn recv(&mut self) -> Result<ProtocolMessage, TransportError> {
        if self.state != ConnectionState::Connected {
            return Err(TransportError::NotConnected);
        }

        let bytes = self
            .inbox_rx
            .recv()
            .await
            .ok_or(TransportError::BrokenPipe)?;
        let msg = rmp_serde::from_slice(&bytes).map_err(|_| TransportError::BrokenPipe)?;
        Ok(msg)
    }

    async fn disconnect(&mut self) -> Result<(), TransportError> {
        self.state = ConnectionState::Disconnected;
        // Platform integration: disconnect from the BLE peripheral.
        Ok(())
    }
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::connection::ConnectionState;

    fn make_peer() -> PeerIdentity {
        PeerIdentity::new([0xABu8; 32])
    }

    // ── UUID sanity ──────────────────────────────────────────────────────────

    #[test]
    fn service_uuids_are_distinct() {
        assert_ne!(SC_SERVICE_UUID, SC_WRITE_CHAR_UUID);
        assert_ne!(SC_SERVICE_UUID, SC_NOTIFY_CHAR_UUID);
        assert_ne!(SC_WRITE_CHAR_UUID, SC_NOTIFY_CHAR_UUID);
    }

    #[test]
    fn service_uuids_are_nonzero() {
        assert_ne!(SC_SERVICE_UUID, Uuid::from_u128(0));
        assert_ne!(SC_WRITE_CHAR_UUID, Uuid::from_u128(0));
        assert_ne!(SC_NOTIFY_CHAR_UUID, Uuid::from_u128(0));
    }

    // ── Chunk encode / decode round‑trip ─────────────────────────────────────

    #[test]
    fn encode_decode_roundtrip() {
        let frame = ChunkFrame {
            message_id: 0xDEAD,
            total_length: 100,
            offset: 0,
            payload_size: 4,
            payload: vec![1, 2, 3, 4],
        };

        let encoded = encode_chunk(&frame);
        let decoded = decode_chunk(&encoded).expect("decode should succeed");
        assert_eq!(decoded, frame);
    }

    #[test]
    fn decode_rejects_short_buffer() {
        assert!(decode_chunk(&[0u8; 13]).is_none());
    }

    #[test]
    fn decode_rejects_mismatched_payload_size() {
        let frame = ChunkFrame {
            message_id: 1,
            total_length: 10,
            offset: 0,
            payload_size: 5, // claims 5 bytes but payload will have 4
            payload: vec![1, 2, 3, 4],
        };
        let mut raw = encode_chunk(&frame);
        // Corrupt payload_size field (bytes 12-13) to claim 5 bytes
        raw[12] = 5;
        raw[13] = 0;
        // Remove one byte from payload to cause mismatch
        raw.pop();
        assert!(decode_chunk(&raw).is_none());
    }

    // ── BlePeripheral initial state ──────────────────────────────────────────

    #[test]
    fn ble_peripheral_initial_state_is_disconnected() {
        let p = BlePeripheral::new(make_peer());
        assert_eq!(p.state(), ConnectionState::Disconnected);
    }

    #[test]
    fn ble_peripheral_transport_type_is_ble() {
        let p = BlePeripheral::new(make_peer());
        assert_eq!(p.transport_type(), TransportType::Ble);
    }

    #[test]
    fn ble_peripheral_remote_peer_matches() {
        let peer = make_peer();
        let p = BlePeripheral::new(peer.clone());
        assert_eq!(p.remote_peer().pubkey, peer.pubkey);
    }

    // ── BleCentral initial state ─────────────────────────────────────────────

    #[test]
    fn ble_central_initial_state_is_disconnected() {
        let c = BleCentral::new(make_peer());
        assert_eq!(c.state(), ConnectionState::Disconnected);
    }

    #[test]
    fn ble_central_transport_type_is_ble() {
        let c = BleCentral::new(make_peer());
        assert_eq!(c.transport_type(), TransportType::Ble);
    }

    // ── send/recv state guards ───────────────────────────────────────────────

    #[tokio::test]
    async fn peripheral_send_fails_when_disconnected() {
        let mut p = BlePeripheral::new(make_peer());
        use crate::message::types::{ProtocolMessage, TopologyUpdate};
        let msg = ProtocolMessage::TopologyUpdate(TopologyUpdate {
            origin_pubkey: [0u8; 32],
            directly_connected_peers: vec![],
            hops_to_relay: 0,
            topology_flags: vec![],
        });
        assert_eq!(p.send(msg).await, Err(TransportError::NotConnected));
    }

    #[tokio::test]
    async fn central_send_fails_when_disconnected() {
        let mut c = BleCentral::new(make_peer());
        use crate::message::types::{ProtocolMessage, TopologyUpdate};
        let msg = ProtocolMessage::TopologyUpdate(TopologyUpdate {
            origin_pubkey: [0u8; 32],
            directly_connected_peers: vec![],
            hops_to_relay: 0,
            topology_flags: vec![],
        });
        assert_eq!(c.send(msg).await, Err(TransportError::NotConnected));
    }

    // ── connect() transitions to Connected (stub) ────────────────────────────

    #[tokio::test]
    async fn peripheral_connect_transitions_to_connected() {
        let mut p = BlePeripheral::new(make_peer());
        p.connect().await.unwrap();
        assert_eq!(p.state(), ConnectionState::Connected);
    }

    #[tokio::test]
    async fn central_connect_transitions_to_connected() {
        let mut c = BleCentral::new(make_peer());
        c.connect().await.unwrap();
        assert_eq!(c.state(), ConnectionState::Connected);
    }

    // ── disconnect() transitions to Disconnected ─────────────────────────────

    #[tokio::test]
    async fn peripheral_disconnect_transitions_to_disconnected() {
        let mut p = BlePeripheral::new(make_peer());
        p.connect().await.unwrap();
        p.disconnect().await.unwrap();
        assert_eq!(p.state(), ConnectionState::Disconnected);
    }

    #[tokio::test]
    async fn central_disconnect_transitions_to_disconnected() {
        let mut c = BleCentral::new(make_peer());
        c.connect().await.unwrap();
        c.disconnect().await.unwrap();
        assert_eq!(c.state(), ConnectionState::Disconnected);
    }

    // ── Ingest chunk → inbox pipeline (BlePeripheral) ────────────────────────

    #[tokio::test]
    async fn peripheral_ingest_chunk_delivers_complete_message() {
        use crate::message::types::{ProtocolMessage, TopologyUpdate};

        let mut p = BlePeripheral::new(make_peer());
        p.connect().await.unwrap();

        // Serialize a small message and chunk it
        let msg = ProtocolMessage::TopologyUpdate(TopologyUpdate {
            origin_pubkey: [0xCCu8; 32],
            directly_connected_peers: vec![[0xAAu8; 32]],
            hops_to_relay: 2,
            topology_flags: vec![],
        });
        let bytes = rmp_serde::to_vec(&msg).unwrap();
        let chunker = MessageChunker { mtu: BLE_ATT_MTU };
        let frames = chunker.chunk(&bytes);

        // Feed each chunk through the ingest path
        for frame in frames {
            let raw = encode_chunk(&frame);
            p.ingest_chunk_bytes(&raw).await.unwrap();
        }

        // recv() should return the original message
        let received = p.recv().await.unwrap();
        assert_eq!(received, msg);
    }

    // ── MAC Address Randomization ─────────────────────────────────────────────

    /// Mock MAC provider for testing that returns predictable values.
    struct MockMacProvider {
        counter: std::sync::atomic::AtomicU8,
    }

    impl MockMacProvider {
        fn new() -> Self {
            Self {
                counter: std::sync::atomic::AtomicU8::new(0),
            }
        }
    }

    impl MacAddressProvider for MockMacProvider {
        fn generate(&self) -> [u8; 6] {
            let val = self
                .counter
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            [val, 0x02, 0x00, 0x00, 0x00, 0x00] // locally-administered bit set
        }
    }

    #[test]
    fn test_random_mac_locally_administered_bit() {
        let provider = RandomMacProvider;
        for _ in 0..1000 {
            let mac = provider.generate();
            // Bit 1 of byte 0 should be set (locally-administered)
            assert!(
                mac[0] & 0b00000010 != 0,
                "MAC {:02X?} missing locally-administered bit",
                mac
            );
            // Bit 0 of byte 0 should be clear (unicast)
            assert!(
                mac[0] & 0b00000001 == 0,
                "MAC {:02X?} has multicast bit set",
                mac
            );
        }
    }

    #[test]
    fn test_rotate_mac_returns_none_before_interval() {
        let mut c = BleCentral::new(make_peer())
            .with_mac_provider(Box::new(MockMacProvider::new()))
            .with_rotation_interval(Duration::from_secs(60));

        let result = c.rotate_mac_if_due();
        assert_eq!(result, None, "Should return None before interval elapses");
    }

    #[test]
    fn test_rotate_mac_returns_new_mac_after_interval() {
        let mut c = BleCentral::new(make_peer());
        let c = c
            .with_mac_provider(Box::new(MockMacProvider::new()))
            .with_rotation_interval(Duration::from_millis(1));

        let initial_mac = c.current_mac();

        // Sleep briefly to ensure the interval has elapsed
        std::thread::sleep(Duration::from_millis(10));

        let mut c = c; // rebind to allow mutable operation
        let result = c.rotate_mac_if_due();
        assert!(
            result.is_some(),
            "Should return Some after interval elapses"
        );
        let new_mac = result.unwrap();
        assert_ne!(new_mac, initial_mac, "New MAC should differ from initial");
    }

    #[test]
    fn test_rotate_mac_no_double_rotation() {
        let mut c = BleCentral::new(make_peer());
        let c = c
            .with_mac_provider(Box::new(MockMacProvider::new()))
            .with_rotation_interval(Duration::from_millis(1));

        std::thread::sleep(Duration::from_millis(10));

        let mut c = c;
        let first_rotation = c.rotate_mac_if_due();
        assert!(first_rotation.is_some());

        let second_rotation = c.rotate_mac_if_due();
        assert_eq!(
            second_rotation, None,
            "Second immediate rotation should return None"
        );
    }

    #[test]
    fn test_current_mac_getter() {
        let c = BleCentral::new(make_peer());
        let mac = c.current_mac();
        assert_eq!(mac.len(), 6);
        // Verify locally-administered bit is set
        assert!(mac[0] & 0b00000010 != 0);
    }
}

