use rand::random;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const CHUNK_FRAME_HEADER_SIZE: usize = 14;
pub const MAX_MESSAGE_SIZE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct ChunkFrame {
    pub message_id: u32,
    pub total_length: u32,
    pub offset: u32,
    pub payload_size: u16,
    pub payload: Vec<u8>,
}

pub struct MessageChunker {
    pub mtu: usize,
}

impl MessageChunker {
    pub fn chunk(&self, message_bytes: &[u8]) -> Vec<ChunkFrame> {
        if message_bytes.is_empty() {
            return Vec::new();
        }

        if self.mtu <= CHUNK_FRAME_HEADER_SIZE {
            return Vec::new();
        }

        if message_bytes.len() > MAX_MESSAGE_SIZE_BYTES {
            return Vec::new();
        }

        let payload_capacity = self.mtu - CHUNK_FRAME_HEADER_SIZE;
        let payload_capacity = payload_capacity.min(u16::MAX as usize);
        if payload_capacity == 0 {
            return Vec::new();
        }

        let message_id = random::<u32>();
        let total_length = message_bytes.len() as u32;
        let mut frames = Vec::new();
        let mut offset = 0usize;

        while offset < message_bytes.len() {
            let end = (offset + payload_capacity).min(message_bytes.len());
            let payload = message_bytes[offset..end].to_vec();

            frames.push(ChunkFrame {
                message_id,
                total_length,
                offset: offset as u32,
                payload_size: payload.len() as u16,
                payload,
            });

            offset = end;
        }

        frames
    }
}

struct PartialMessageBuffer {
    total_length: usize,
    data: Vec<u8>,
    received_map: Vec<bool>,
    received_bytes: usize,
    last_updated: Instant,
}

pub struct MessageReassembler {
    buffers: HashMap<u32, PartialMessageBuffer>,
}

impl MessageReassembler {
    pub fn new() -> Self {
        Self {
            buffers: HashMap::new(),
        }
    }

    pub fn receive_chunk(&mut self, chunk: ChunkFrame) -> Option<Vec<u8>> {
        let total_length = chunk.total_length as usize;
        if total_length == 0 || total_length > MAX_MESSAGE_SIZE_BYTES {
            return None;
        }

        if usize::from(chunk.payload_size) != chunk.payload.len() {
            return None;
        }

        let start = chunk.offset as usize;
        let end = start.checked_add(chunk.payload.len())?;

        if start >= total_length || end > total_length {
            return None;
        }

        let buffer = self
            .buffers
            .entry(chunk.message_id)
            .or_insert_with(|| PartialMessageBuffer {
                total_length,
                data: vec![0u8; total_length],
                received_map: vec![false; total_length],
                received_bytes: 0,
                last_updated: Instant::now(),
            });

        if buffer.total_length != total_length {
            return None;
        }

        for (idx, byte) in (start..end).zip(chunk.payload.iter().copied()) {
            if !buffer.received_map[idx] {
                buffer.received_map[idx] = true;
                buffer.received_bytes += 1;
            }
            buffer.data[idx] = byte;
        }

        buffer.last_updated = Instant::now();

        if buffer.received_bytes == buffer.total_length {
            if let Some(completed) = self.buffers.remove(&chunk.message_id) {
                return Some(completed.data);
            }
        }

        None
    }

    pub fn cleanup_stale_buffers(&mut self, timeout_ms: u64) {
        let timeout = Duration::from_millis(timeout_ms);
        let now = Instant::now();
        self.buffers
            .retain(|_, buffer| now.duration_since(buffer.last_updated) <= timeout);
    }

    pub fn in_flight_buffer_count(&self) -> usize {
        self.buffers.len()
    }
}

impl Default for MessageReassembler {
    fn default() -> Self {
        Self::new()
    }
}

// ─── TransportPreference ──────────────────────────────────────────────────────

/// Controls which physical transport the `TransportManager` will attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportPreference {
    /// Automatically pick the best available transport.
    /// Tries WiFi-Direct first; falls back to BLE on failure.
    Auto,
    /// Force BLE even if WiFi-Direct is available.
    BleOnly,
    /// Force WiFi-Direct. Returns an error if no `SocketAddr` is provided or connection fails.
    WifiOnly,
}

// ─── TransportManager ─────────────────────────────────────────────────────────

use std::net::SocketAddr;

use crate::message::types::ProtocolMessage;
use crate::peer::identity::PeerIdentity;
use crate::transport::ble_transport::BleCentral;
use crate::transport::connection::Connection;
use crate::transport::errors::TransportError;
use crate::transport::power::{InterfacePowerState, PowerManager};
use crate::transport::wifi_transport::WifiDirectConnection;

#[derive(Debug, Clone, PartialEq)]
struct PendingMessage {
    peer: PeerIdentity,
    message: ProtocolMessage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PowerTickOutcome {
    pub interface_state: InterfacePowerState,
    pub topology_flags: Vec<crate::message::types::TopologyFlag>,
    pub flushed_transactions: usize,
}

/// Manages per-peer transport connections, automatically selecting the best
/// available physical transport and falling back gracefully.
pub struct TransportManager {
    preference: TransportPreference,
    /// One `Box<dyn Connection>` per peer pubkey.
    active_connections: HashMap<[u8; 32], Box<dyn Connection>>,
    power_manager: PowerManager,
    pending_messages: VecDeque<PendingMessage>,
}

impl TransportManager {
    pub fn new(preference: TransportPreference) -> Self {
        Self::with_power_manager(preference, PowerManager::new(current_unix_duration()))
    }

    pub fn with_power_manager(
        preference: TransportPreference,
        power_manager: PowerManager,
    ) -> Self {
        Self {
            preference,
            active_connections: HashMap::new(),
            power_manager,
            pending_messages: VecDeque::new(),
        }
    }

    /// Number of currently active peer connections (test helper).
    pub fn connection_count(&self) -> usize {
        self.active_connections.len()
    }

    pub fn pending_message_count(&self) -> usize {
        self.pending_messages.len()
    }

    /// Open a connection to `peer` using the best available transport.
    ///
    /// - `wifi_addr`: the peer's WiFi-Direct P2P IP address (externally provided).
    ///   Pass `None` to skip WiFi-Direct entirely.
    ///
    /// Fallback order for `Auto`:
    ///   1. WiFi-Direct (if `wifi_addr` is `Some`)
    ///   2. BLE (via `BleCentral`)
    ///
    /// Replaces any existing connection for the same peer.
    pub async fn connect(
        &mut self,
        peer: PeerIdentity,
        wifi_addr: Option<SocketAddr>,
    ) -> Result<(), TransportError> {
        let conn: Box<dyn Connection> = match self.preference {
            TransportPreference::WifiOnly => {
                let addr = wifi_addr.ok_or(TransportError::NotConnected)?;
                let c = WifiDirectConnection::connect_to(peer.clone(), addr).await?;
                Box::new(c)
            }
            TransportPreference::BleOnly => {
                let mut c = BleCentral::new(peer.clone());
                c.connect().await?;
                Box::new(c)
            }
            TransportPreference::Auto => {
                // Try WiFi-Direct first
                if let Some(addr) = wifi_addr {
                    match WifiDirectConnection::connect_to(peer.clone(), addr).await {
                        Ok(c) => Box::new(c),
                        Err(TransportError::ConnectionRefused) | Err(TransportError::Timeout) => {
                            // Fall back to BLE
                            log::debug!("WiFi-Direct failed for peer; falling back to BLE");
                            let mut c = BleCentral::new(peer.clone());
                            c.connect().await?;
                            Box::new(c)
                        }
                        Err(e) => return Err(e),
                    }
                } else {
                    // No WiFi addr — go straight to BLE
                    let mut c = BleCentral::new(peer.clone());
                    c.connect().await?;
                    Box::new(c)
                }
            }
        };

        self.active_connections.insert(peer.pubkey, conn);
        Ok(())
    }

    /// Send a message to a specific peer.
    ///
    /// On `BrokenPipe`, removes the connection and attempts BLE fallback.
    pub async fn send_to(
        &mut self,
        peer: &PeerIdentity,
        msg: ProtocolMessage,
    ) -> Result<(), TransportError> {
        self.send_to_at(peer, msg, current_unix_duration()).await
    }

    pub async fn send_to_at(
        &mut self,
        peer: &PeerIdentity,
        msg: ProtocolMessage,
        now: Duration,
    ) -> Result<(), TransportError> {
        self.power_tick_at(now).await?;

        if matches!(msg, ProtocolMessage::Transaction(_))
            && self.power_manager.mode() == crate::transport::power::PowerMode::SynchronizedLowPower
            && !self.power_manager.interface_state(now).ble_enabled
        {
            let _ = self.power_manager.record_outbound_transaction(now);
            self.pending_messages.push_back(PendingMessage {
                peer: peer.clone(),
                message: msg,
            });
            return Ok(());
        }

        let _ = self.power_manager.record_outbound_transaction(now);
        self.send_now(peer, msg).await
    }

    /// Poll each active connection for the next message.
    /// Returns the first `(PeerIdentity, ProtocolMessage)` received.
    /// On `BrokenPipe`, removes the failed connection and attempts BLE fallback.
    pub async fn recv_any(&mut self) -> Option<(PeerIdentity, ProtocolMessage)> {
        self.recv_any_at(current_unix_duration()).await
    }

    pub async fn recv_any_at(&mut self, now: Duration) -> Option<(PeerIdentity, ProtocolMessage)> {
        let tick = self.power_tick_at(now).await.ok()?;
        if !tick.interface_state.ble_enabled && !tick.interface_state.wifi_enabled {
            return None;
        }

        let keys: Vec<[u8; 32]> = self.active_connections.keys().copied().collect();

        for pubkey in keys {
            let peer = if let Some(conn) = self.active_connections.get(&pubkey) {
                conn.remote_peer()
            } else {
                continue;
            };

            // We can't directly await on a &mut through the map, so use a temp approach.
            // NOTE: In production this would use tokio::select! across all connections.
            // For the testable synchronous fallback logic, we poll them in turn.
            let result = {
                if let Some(conn) = self.active_connections.get_mut(&pubkey) {
                    // Non-blocking check: use try_recv pattern by attempting recv with a timeout
                    Some(
                        tokio::time::timeout(std::time::Duration::from_millis(1), conn.recv())
                            .await,
                    )
                } else {
                    None
                }
            };

            match result {
                Some(Ok(Ok(msg))) => {
                    if matches!(msg, ProtocolMessage::Transaction(_)) {
                        let _ = self.power_manager.record_incoming_transaction(now);
                    }
                    return Some((peer, msg));
                }
                Some(Ok(Err(TransportError::BrokenPipe))) => {
                    log::debug!("recv_any: BrokenPipe — falling back to BLE for peer");
                    self.active_connections.remove(&pubkey);
                    let _ = self.ble_fallback(peer).await;
                }
                _ => continue,
            }
        }

        None
    }

    /// Disconnect a specific peer by pubkey.
    /// Returns true if the peer was connected and disconnected.
    pub async fn disconnect_peer(&mut self, pubkey: &[u8; 32]) -> bool {
        if let Some(mut conn) = self.active_connections.remove(pubkey) {
            let _ = conn.disconnect().await;
            true
        } else {
            false
        }
    }

    /// Disconnect all active connections and clear the map.
    pub async fn shutdown(&mut self) {
        for (_, mut conn) in self.active_connections.drain() {
            let _ = conn.disconnect().await;
        }
        self.pending_messages.clear();
    }

    pub async fn power_tick(&mut self) -> Result<PowerTickOutcome, TransportError> {
        self.power_tick_at(current_unix_duration()).await
    }

    // ── Internal ─────────────────────────────────────────────────────────────

    async fn power_tick_at(&mut self, now: Duration) -> Result<PowerTickOutcome, TransportError> {
        let decision = self.power_manager.tick(now);
        let mut flushed_transactions = 0usize;

        if decision.wake_network {
            while let Some(pending) = self.pending_messages.pop_front() {
                self.send_now(&pending.peer, pending.message).await?;
                flushed_transactions += 1;
            }
        }

        Ok(PowerTickOutcome {
            interface_state: decision.interface_state,
            topology_flags: decision.topology_flags,
            flushed_transactions,
        })
    }

    async fn send_now(
        &mut self,
        peer: &PeerIdentity,
        msg: ProtocolMessage,
    ) -> Result<(), TransportError> {
        if let Some(conn) = self.active_connections.get_mut(&peer.pubkey) {
            match conn.send(msg.clone()).await {
                Ok(()) => return Ok(()),
                Err(TransportError::BrokenPipe) => {
                    log::debug!("send_to: BrokenPipe — removing connection for peer");
                    self.active_connections.remove(&peer.pubkey);
                    self.ble_fallback(peer.clone()).await?;
                    if let Some(conn) = self.active_connections.get_mut(&peer.pubkey) {
                        return conn.send(msg).await;
                    }
                    return Err(TransportError::NotConnected);
                }
                Err(e) => return Err(e),
            }
        }

        Err(TransportError::NotConnected)
    }

    async fn ble_fallback(&mut self, peer: PeerIdentity) -> Result<(), TransportError> {
        log::debug!("ble_fallback: connecting via BLE for peer");
        let mut c = BleCentral::new(peer.clone());
        c.connect().await?;
        self.active_connections.insert(peer.pubkey, Box::new(c));
        Ok(())
    }
}

fn current_unix_duration() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn chunker_slices_respecting_mtu() {
        let mtu = 32usize;
        let chunker = MessageChunker { mtu };
        let message: Vec<u8> = (0..100u8).collect();

        let chunks = chunker.chunk(&message);
        assert!(!chunks.is_empty());

        for chunk in &chunks {
            let frame_size = CHUNK_FRAME_HEADER_SIZE + chunk.payload.len();
            assert!(frame_size <= mtu);
            assert_eq!(usize::from(chunk.payload_size), chunk.payload.len());
            assert_eq!(chunk.total_length as usize, message.len());
        }

        let mut reassembler = MessageReassembler::new();
        let mut rebuilt = None;
        for chunk in chunks {
            if let Some(bytes) = reassembler.receive_chunk(chunk) {
                rebuilt = Some(bytes);
            }
        }

        assert_eq!(rebuilt, Some(message));
    }

    #[test]
    fn reassembler_handles_out_of_order_chunks() {
        let chunker = MessageChunker { mtu: 40 };
        let message: Vec<u8> = (0..200u16).map(|v| (v % 251) as u8).collect();
        let mut chunks = chunker.chunk(&message);

        assert!(chunks.len() >= 3);
        chunks.swap(0, 1);
        let len = chunks.len();
        chunks.swap(len - 1, len - 2);

        let mut reassembler = MessageReassembler::new();
        let mut rebuilt = None;

        for chunk in chunks {
            if let Some(bytes) = reassembler.receive_chunk(chunk) {
                rebuilt = Some(bytes);
            }
        }

        assert_eq!(rebuilt, Some(message));
    }

    #[test]
    fn stale_buffers_are_cleaned_up() {
        let mut reassembler = MessageReassembler::new();
        let chunk = ChunkFrame {
            message_id: 7,
            total_length: 10,
            offset: 0,
            payload_size: 4,
            payload: vec![1, 2, 3, 4],
        };

        assert_eq!(reassembler.receive_chunk(chunk), None);
        assert_eq!(reassembler.in_flight_buffer_count(), 1);

        thread::sleep(Duration::from_millis(20));
        reassembler.cleanup_stale_buffers(5);
        assert_eq!(reassembler.in_flight_buffer_count(), 0);
    }

    #[test]
    fn oversized_message_is_rejected() {
        let mut reassembler = MessageReassembler::new();
        let chunk = ChunkFrame {
            message_id: 1,
            total_length: (MAX_MESSAGE_SIZE_BYTES + 1) as u32,
            offset: 0,
            payload_size: 1,
            payload: vec![1],
        };

        assert_eq!(reassembler.receive_chunk(chunk), None);
        assert_eq!(reassembler.in_flight_buffer_count(), 0);
    }

    // ── TransportManager tests ────────────────────────────────────────────────

    use crate::message::types::{
        ProtocolMessage, TopologyFlag, TopologyUpdate, TransactionEnvelope,
    };
    use crate::transport::connection::{ConnectionState, TransportType};
    use crate::transport::power::{PowerManager, IDLE_SLEEP_TIMEOUT};
    use crate::transport::unified::{TransportManager, TransportPreference};
    use async_trait::async_trait;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };
    use tokio::net::TcpListener;

    fn peer(b: u8) -> PeerIdentity {
        PeerIdentity::new([b; 32])
    }

    fn sample_msg(b: u8) -> ProtocolMessage {
        ProtocolMessage::TopologyUpdate(TopologyUpdate {
            origin_pubkey: [b; 32],
            directly_connected_peers: vec![],
            hops_to_relay: 1,
            topology_flags: vec![],
        })
    }

    fn sample_transaction(b: u8) -> ProtocolMessage {
        ProtocolMessage::Transaction(TransactionEnvelope {
            message_id: [b; 32],
            origin_pubkey: [b.wrapping_add(1); 32],
            tx_xdr: format!("tx-{b}"),
            ttl_hops: 4,
            timestamp: 42,
            signature: [0; 64],
        })
    }

    struct MockConnection {
        peer: PeerIdentity,
        recv_calls: Arc<AtomicUsize>,
        sent_messages: Arc<Mutex<Vec<ProtocolMessage>>>,
        inbox: Arc<Mutex<Vec<ProtocolMessage>>>,
    }

    impl MockConnection {
        fn new(
            peer: PeerIdentity,
            recv_calls: Arc<AtomicUsize>,
            sent_messages: Arc<Mutex<Vec<ProtocolMessage>>>,
            inbox: Arc<Mutex<Vec<ProtocolMessage>>>,
        ) -> Self {
            Self {
                peer,
                recv_calls,
                sent_messages,
                inbox,
            }
        }
    }

    #[async_trait]
    impl Connection for MockConnection {
        fn remote_peer(&self) -> PeerIdentity {
            self.peer.clone()
        }

        fn transport_type(&self) -> TransportType {
            TransportType::Ble
        }

        fn state(&self) -> ConnectionState {
            ConnectionState::Connected
        }

        async fn connect(&mut self) -> Result<(), TransportError> {
            Ok(())
        }

        async fn send(&mut self, msg: ProtocolMessage) -> Result<(), TransportError> {
            self.sent_messages.lock().unwrap().push(msg);
            Ok(())
        }

        async fn recv(&mut self) -> Result<ProtocolMessage, TransportError> {
            self.recv_calls.fetch_add(1, Ordering::SeqCst);
            self.inbox
                .lock()
                .unwrap()
                .pop()
                .ok_or(TransportError::BrokenPipe)
        }

        async fn disconnect(&mut self) -> Result<(), TransportError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn auto_mode_uses_wifi_when_addr_provided() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Accept the incoming connection in the background
        let _server = tokio::spawn(async move {
            let _ = crate::transport::wifi_transport::WifiDirectConnection::accept_from(
                &listener,
                peer(0xAA),
            )
            .await;
        });

        let mut mgr = TransportManager::new(TransportPreference::Auto);
        mgr.connect(peer(0xBB), Some(addr)).await.unwrap();
        assert_eq!(mgr.connection_count(), 1);
        mgr.shutdown().await;
        assert_eq!(mgr.connection_count(), 0);
    }

    #[tokio::test]
    async fn auto_mode_falls_back_to_ble_when_no_wifi_addr() {
        let mut mgr = TransportManager::new(TransportPreference::Auto);
        // No WiFi addr → goes straight to BLE stub (scan_and_connect is a no-op stub)
        mgr.connect(peer(0xCC), None).await.unwrap();
        assert_eq!(mgr.connection_count(), 1);
    }

    #[tokio::test]
    async fn wifi_only_fails_without_addr() {
        let mut mgr = TransportManager::new(TransportPreference::WifiOnly);
        let result = mgr.connect(peer(0xDD), None).await;
        assert_eq!(result, Err(TransportError::NotConnected));
    }

    #[tokio::test]
    async fn ble_only_skips_wifi() {
        let mut mgr = TransportManager::new(TransportPreference::BleOnly);
        // BleOnly ignores any WiFi addr and uses BLE stub directly
        mgr.connect(peer(0xEE), None).await.unwrap();
        assert_eq!(mgr.connection_count(), 1);
    }

    #[tokio::test]
    async fn only_one_connection_per_peer() {
        let listener1 = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr1 = listener1.local_addr().unwrap();
        let listener2 = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr2 = listener2.local_addr().unwrap();

        tokio::spawn(async move {
            let _ = crate::transport::wifi_transport::WifiDirectConnection::accept_from(
                &listener1,
                peer(0x01),
            )
            .await;
        });
        tokio::spawn(async move {
            let _ = crate::transport::wifi_transport::WifiDirectConnection::accept_from(
                &listener2,
                peer(0x01),
            )
            .await;
        });

        let mut mgr = TransportManager::new(TransportPreference::Auto);
        mgr.connect(peer(0xFF), Some(addr1)).await.unwrap();
        assert_eq!(mgr.connection_count(), 1);

        // Second connect to same peer replaces the first
        mgr.connect(peer(0xFF), Some(addr2)).await.unwrap();
        assert_eq!(mgr.connection_count(), 1);
    }

    #[tokio::test]
    async fn shutdown_clears_all_connections() {
        let mut mgr = TransportManager::new(TransportPreference::BleOnly);
        mgr.connect(peer(0x01), None).await.unwrap();
        mgr.connect(peer(0x02), None).await.unwrap();
        assert_eq!(mgr.connection_count(), 2);
        mgr.shutdown().await;
        assert_eq!(mgr.connection_count(), 0);
    }

    #[tokio::test]
    async fn send_to_succeeds_when_connected_via_wifi() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_peer = peer(0xAA);
        let server_task = tokio::spawn(async move {
            crate::transport::wifi_transport::WifiDirectConnection::accept_from(
                &listener,
                server_peer,
            )
            .await
            .unwrap()
        });

        let p = peer(0xBB);
        let mut mgr = TransportManager::new(TransportPreference::Auto);
        mgr.connect(p.clone(), Some(addr)).await.unwrap();

        let msg = sample_msg(42);
        mgr.send_to(&p, msg.clone()).await.unwrap();

        let mut server_conn = server_task.await.unwrap();
        let received = server_conn.recv().await.unwrap();
        assert_eq!(received, msg);
    }

    #[tokio::test]
    async fn power_tick_emits_go_to_sleep_flag_after_idle_timeout() {
        let mut mgr = TransportManager::with_power_manager(
            TransportPreference::BleOnly,
            PowerManager::new(Duration::from_secs(0)),
        );

        let tick = mgr.power_tick_at(IDLE_SLEEP_TIMEOUT).await.unwrap();
        assert_eq!(tick.topology_flags, vec![TopologyFlag::GoToSleep]);
        assert!(tick.interface_state.ble_enabled);
    }

    #[tokio::test]
    async fn recv_any_skips_transport_polling_while_interfaces_sleep() {
        let peer = peer(0x21);
        let recv_calls = Arc::new(AtomicUsize::new(0));
        let sent_messages = Arc::new(Mutex::new(Vec::new()));
        let inbox = Arc::new(Mutex::new(vec![sample_msg(7)]));

        let mut mgr = TransportManager::with_power_manager(
            TransportPreference::BleOnly,
            PowerManager::new(Duration::from_secs(0)),
        );
        mgr.active_connections.insert(
            peer.pubkey,
            Box::new(MockConnection::new(
                peer.clone(),
                recv_calls.clone(),
                sent_messages,
                inbox,
            )),
        );

        let sleeping_at = IDLE_SLEEP_TIMEOUT + Duration::from_secs(1);
        assert_eq!(mgr.recv_any_at(sleeping_at).await, None);
        assert_eq!(recv_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn sleeping_transactions_flush_on_next_awake_barrier() {
        let peer = peer(0x44);
        let recv_calls = Arc::new(AtomicUsize::new(0));
        let sent_messages = Arc::new(Mutex::new(Vec::new()));
        let inbox = Arc::new(Mutex::new(Vec::new()));

        let mut mgr = TransportManager::with_power_manager(
            TransportPreference::BleOnly,
            PowerManager::new(Duration::from_secs(0)),
        );
        mgr.active_connections.insert(
            peer.pubkey,
            Box::new(MockConnection::new(
                peer.clone(),
                recv_calls,
                sent_messages.clone(),
                inbox,
            )),
        );

        let sleeping_at = IDLE_SLEEP_TIMEOUT + Duration::from_secs(1);
        let tx = sample_transaction(9);
        mgr.send_to_at(&peer, tx.clone(), sleeping_at)
            .await
            .unwrap();

        assert_eq!(mgr.pending_message_count(), 1);
        assert!(sent_messages.lock().unwrap().is_empty());

        let tick = mgr
            .power_tick_at(IDLE_SLEEP_TIMEOUT + Duration::from_secs(5))
            .await
            .unwrap();

        assert_eq!(tick.flushed_transactions, 1);
        assert_eq!(mgr.pending_message_count(), 0);
        assert_eq!(sent_messages.lock().unwrap().as_slice(), &[tx]);
    }
}
