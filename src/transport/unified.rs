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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::{mpsc, watch};

use crate::message::types::ProtocolMessage;
use crate::peer::identity::PeerIdentity;
use crate::transport::ble_transport::BleCentral;
#[cfg(feature = "ble")]
use crate::transport::ble_transport::{BlePeripheral, decode_chunk};
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
    /// Maximum number of simultaneously connected peers.
    pub max_peers: usize,
    /// Atomic mirror of `active_connections.len()` shared with listener tasks.
    peer_count: Arc<AtomicUsize>,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
    /// Sender for injecting raw characteristic write data into the BLE listener.
    /// Used in tests to simulate peripheral data reception.
    #[cfg(feature = "ble")]
    ble_inject_tx: Option<mpsc::Sender<Vec<u8>>>,
    /// Receiver for inbound BLE connections that have completed the handshake.
    #[cfg(feature = "ble")]
    ble_conn_rx: Option<mpsc::Receiver<(PeerIdentity, Box<dyn Connection>)>>,
    /// Delay between BLE advertising retry attempts when advertising fails.
    #[cfg(feature = "ble")]
    ble_advertise_retry_delay: Duration,
    /// BleCentral instance for MAC address randomization (privacy).
    /// Used to track and rotate the local device's BLE scanning address.
    ble_central_scanner: Option<BleCentral>,
}

impl TransportManager {
    pub fn new(preference: TransportPreference) -> Self {
        Self::with_power_manager(preference, PowerManager::new(current_unix_duration()))
    }

    pub fn with_power_manager(
        preference: TransportPreference,
        power_manager: PowerManager,
    ) -> Self {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        Self {
            preference,
            active_connections: HashMap::new(),
            power_manager,
            pending_messages: VecDeque::new(),
            max_peers: 300,
            peer_count: Arc::new(AtomicUsize::new(0)),
            shutdown_tx,
            shutdown_rx,
            #[cfg(feature = "ble")]
            ble_inject_tx: None,
            #[cfg(feature = "ble")]
            ble_conn_rx: None,
            #[cfg(feature = "ble")]
            ble_advertise_retry_delay: Duration::from_secs(30),
            ble_central_scanner: None,
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

        if self.active_connections.insert(peer.pubkey, conn).is_none() {
            self.peer_count.fetch_add(1, Ordering::SeqCst);
        }
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
                    if self.active_connections.remove(&pubkey).is_some() {
                        self.peer_count.fetch_sub(1, Ordering::SeqCst);
                    }
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
            self.peer_count.fetch_sub(1, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    /// Disconnect all active connections and clear the map.
    pub async fn shutdown(&mut self) {
        let _ = self.shutdown_tx.send(true);
        for (_, mut conn) in self.active_connections.drain() {
            let _ = conn.disconnect().await;
        }
        self.peer_count.store(0, Ordering::SeqCst);
        self.pending_messages.clear();
    }

    /// Bind a TCP listener on `0.0.0.0:{port}` and spawn a background task that
    /// accepts inbound WiFi-Direct (TCP) connections, wraps each as a
    /// `WifiDirectConnection`, and delivers `(PeerIdentity, Box<dyn Connection>)`
    /// tuples over `incoming_tx`.
    ///
    /// Pass `port = 0` to let the OS pick an ephemeral port; the actual bound
    /// address is returned so callers can advertise it via mDNS.
    ///
    /// Returns `Err(TransportError::ConnectionRefused)` if the port is already in use.
    /// The background task stops when `shutdown()` is called.
    pub async fn start_wifi_listener(
        &self,
        port: u16,
        incoming_tx: mpsc::Sender<(PeerIdentity, Box<dyn Connection>)>,
    ) -> Result<SocketAddr, TransportError> {
        let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
            .await
            .map_err(|_| TransportError::ConnectionRefused)?;
        let bound_addr = listener
            .local_addr()
            .map_err(|_| TransportError::BrokenPipe)?;

        let peer_count = Arc::clone(&self.peer_count);
        let max_peers = self.max_peers;
        let mut shutdown_rx = self.shutdown_rx.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown_rx.changed() => { break; }
                    result = listener.accept() => {
                        match result {
                            Ok((stream, _)) => {
                                if peer_count.load(Ordering::SeqCst) >= max_peers {
                                    log::warn!(
                                        "max_peers ({max_peers}) reached, dropping inbound connection"
                                    );
                                    drop(stream);
                                    continue;
                                }
                                let placeholder = PeerIdentity::new(rand::random());
                                let conn = WifiDirectConnection::from_stream(stream, placeholder.clone());
                                let _ = incoming_tx.send((placeholder, Box::new(conn))).await;
                            }
                            Err(_) => break,
                        }
                    }
                }
            }
        });

        Ok(bound_addr)
    }

    /// Start accepting inbound BLE connections from Central devices.
    ///
    /// This method starts a background task that:
    /// 1. Instantiates a `BlePeripheral` and calls `start_advertising()`.
    /// 2. Waits for a Central to connect and send a `ProtocolMessage::Handshake`.
    /// 3. Registers the connection in `self.connections` keyed by the Central's pubkey.
    /// 4. Loops to accept the next inbound connection.
    ///
    /// Returns immediately after spawning the accept task.
    #[cfg(feature = "ble")]
    pub async fn start_ble_listener(
        &mut self,
        local_peer: PeerIdentity,
    ) -> Result<(), TransportError> {
        let (data_tx, mut data_rx) = mpsc::channel::<Vec<u8>>(256);
        let (conn_tx, conn_rx) = mpsc::channel::<(PeerIdentity, Box<dyn Connection>)>(64);

        self.ble_inject_tx = Some(data_tx);
        self.ble_conn_rx = Some(conn_rx);

        let mut shutdown_rx = self.shutdown_rx.clone();
        let peer_count = Arc::clone(&self.peer_count);
        let max_peers = self.max_peers;
        let retry_delay = self.ble_advertise_retry_delay;

        tokio::spawn(async move {
            'outer: loop {
                // Retry advertising on failure with configurable delay.
                loop {
                    let mut peripheral = BlePeripheral::new(local_peer.clone());
                    match peripheral.start_advertising().await {
                        Ok(()) => break,
                        Err(e) => {
                            log::error!(
                                "BLE advertising failed: {e:?}; retrying in ~{delay}s",
                                delay = retry_delay.as_secs(),
                            );
                            tokio::time::sleep(retry_delay).await;
                        }
                    }
                }

                // Accept loop — reassemble one connection at a time.
                let mut reassembler = MessageReassembler::new();

                loop {
                    tokio::select! {
                        biased;
                        _ = shutdown_rx.changed() => break 'outer,
                        data = data_rx.recv() => {
                            match data {
                                Some(bytes) => {
                                    if peer_count.load(Ordering::SeqCst) >= max_peers {
                                        log::warn!(
                                            "max_peers ({max_peers}) reached, dropping inbound BLE"
                                        );
                                        continue;
                                    }

                                    // Decode chunk frame and try to complete a message.
                                    let Some(frame) = decode_chunk(&bytes) else {
                                        continue;
                                    };
                                    let Some(complete) = reassembler.receive_chunk(frame)
                                        else {
                                        continue;
                                    };

                                    match ProtocolMessage::from_bytes(&complete) {
                                        Ok(ProtocolMessage::Handshake { pubkey }) => {
                                            let central_peer = PeerIdentity::new(pubkey);
                                            let conn: Box<dyn Connection> =
                                                Box::new(BlePeripheral::new(central_peer.clone()));
                                            let _ = conn_tx.send((central_peer, conn)).await;
                                            // Reset for next connection.
                                            reassembler = MessageReassembler::new();
                                        }
                                        _ => {
                                            log::warn!(
                                                "First BLE message was not a Handshake; \
                                                 dropping connection"
                                            );
                                            reassembler = MessageReassembler::new();
                                        }
                                    }
                                }
                                None => break 'outer,
                            }
                        }
                    }
                }
            }
        });

        Ok(())
    }

    /// Register an inbound connection accepted externally (e.g. from the gossip loop).
    ///
    /// Inserts `conn` into `active_connections` keyed by `peer.pubkey`, replacing
    /// any existing stale connection.  If a new slot is consumed, `peer_count` is
    /// incremented so the listener's backpressure check stays accurate.
    pub fn register_inbound(&mut self, peer: PeerIdentity, conn: Box<dyn Connection>) {
        if self.active_connections.insert(peer.pubkey, conn).is_none() {
            self.peer_count.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Drain the BLE listener's completed-handshake channel and register any
    /// ready-to-use connections into `active_connections`.
    #[cfg(feature = "ble")]
    fn process_ble_incoming(&mut self) {
        let mut entries = Vec::new();
        if let Some(rx) = &mut self.ble_conn_rx {
            loop {
                match rx.try_recv() {
                    Ok((peer, conn)) => entries.push((peer, conn)),
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        self.ble_conn_rx = None;
                        break;
                    }
                }
            }
        }
        for (peer, conn) in entries {
            self.register_inbound(peer, conn);
        }
    }

    /// Return a clone of the BLE data injection sender, if the listener is running.
    /// Used in tests to simulate characteristic writes from a Central.
    #[cfg(feature = "ble")]
    pub fn ble_inject(&self) -> Option<mpsc::Sender<Vec<u8>>> {
        self.ble_inject_tx.clone()
    }

    pub async fn power_tick(&mut self) -> Result<PowerTickOutcome, TransportError> {
        self.power_tick_at(current_unix_duration()).await
    }

    // ── Internal ─────────────────────────────────────────────────────────────

    async fn power_tick_at(&mut self, now: Duration) -> Result<PowerTickOutcome, TransportError> {
        #[cfg(feature = "ble")]
        self.process_ble_incoming();

        let decision = self.power_manager.tick(now);
        let mut flushed_transactions = 0usize;
        let mut topology_flags = decision.topology_flags;

        // Check for BLE MAC address rotation
        if let Some(ble_central) = &mut self.ble_central_scanner {
            if let Some(new_mac) = ble_central.rotate_mac_if_due() {
                topology_flags.push(crate::message::types::TopologyFlag::MacRotated);
                self.notify_mac_rotated(new_mac).await;
            }
        }

        if decision.wake_network {
            while let Some(pending) = self.pending_messages.pop_front() {
                self.send_now(&pending.peer, pending.message).await?;
                flushed_transactions += 1;
            }
        }

        Ok(PowerTickOutcome {
            interface_state: decision.interface_state,
            topology_flags,
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
                    if self.active_connections.remove(&peer.pubkey).is_some() {
                        self.peer_count.fetch_sub(1, Ordering::SeqCst);
                    }
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

    /// Notify all connections that a BLE MAC address rotation has occurred.
    /// 
    /// This method drops all existing BLE connections (as they reference the old MAC).
    /// WiFi-Direct connections are unaffected and persist across the rotation.
    pub async fn notify_mac_rotated(&mut self, new_mac: [u8; 6]) {
        let ble_peers: Vec<[u8; 32]> = self
            .active_connections
            .iter()
            .filter(|(_, c)| c.transport_type() == crate::transport::connection::TransportType::Ble)
            .map(|(k, _)| *k)
            .collect();

        for pubkey in ble_peers {
            if let Some(mut conn) = self.active_connections.remove(&pubkey) {
                let _ = conn.disconnect().await;
                self.peer_count.fetch_sub(1, Ordering::SeqCst);
            }
        }

        log::info!(
            "MAC rotated to {:02X?}; dropped {} BLE connections",
            new_mac,
            ble_peers.len()
        );
    }

    async fn ble_fallback(&mut self, peer: PeerIdentity) -> Result<(), TransportError> {
        log::debug!("ble_fallback: connecting via BLE for peer");
        let mut c = BleCentral::new(peer.clone());
        c.connect().await?;
        if self
            .active_connections
            .insert(peer.pubkey, Box::new(c))
            .is_none()
        {
            self.peer_count.fetch_add(1, Ordering::SeqCst);
        }
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
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::mpsc;

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

    // ── WiFi listener tests ───────────────────────────────────────────────────

    #[tokio::test]
    async fn test_wifi_listener_binds_and_accepts() {
        let (tx, mut rx) = mpsc::channel(8);
        let mgr = TransportManager::new(TransportPreference::BleOnly);
        let addr = mgr.start_wifi_listener(0, tx).await.unwrap();

        tokio::spawn(async move {
            let _ = TcpStream::connect(addr).await;
        });

        let result = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await;
        assert!(result.is_ok(), "timed out waiting for inbound connection");
        assert!(result.unwrap().is_some());
    }

    #[tokio::test]
    async fn test_register_inbound_enables_send_to() {
        let p = peer(0x55);
        let recv_calls = Arc::new(AtomicUsize::new(0));
        let sent_messages = Arc::new(Mutex::new(Vec::new()));
        let inbox = Arc::new(Mutex::new(Vec::new()));

        let mut mgr = TransportManager::new(TransportPreference::BleOnly);
        mgr.register_inbound(
            p.clone(),
            Box::new(MockConnection::new(
                p.clone(),
                recv_calls,
                sent_messages.clone(),
                inbox,
            )),
        );
        assert_eq!(mgr.connection_count(), 1);

        let msg = sample_msg(42);
        mgr.send_to(&p, msg.clone()).await.unwrap();
        assert_eq!(sent_messages.lock().unwrap().as_slice(), &[msg]);
    }

    #[tokio::test]
    async fn test_listener_rejects_beyond_max_peers() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut mgr = TransportManager::new(TransportPreference::BleOnly);
        mgr.max_peers = 1;

        let p = peer(0x77);
        let recv_calls = Arc::new(AtomicUsize::new(0));
        let sent_messages = Arc::new(Mutex::new(Vec::new()));
        let inbox = Arc::new(Mutex::new(Vec::new()));
        mgr.register_inbound(
            p.clone(),
            Box::new(MockConnection::new(
                p.clone(),
                recv_calls,
                sent_messages,
                inbox,
            )),
        );
        assert_eq!(mgr.peer_count.load(Ordering::SeqCst), 1);

        let addr = mgr.start_wifi_listener(0, tx).await.unwrap();

        let _ = TcpStream::connect(addr).await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(
            rx.try_recv().is_err(),
            "connection should have been dropped"
        );
    }

    // ── BLE listener tests ───────────────────────────────────────────────────

    #[cfg(feature = "ble")]
    #[tokio::test]
    async fn test_ble_listener_registers_connection_after_handshake() {
        let local = peer(0xAA);
        let central_pubkey = [0xBB; 32];
        let mut mgr = TransportManager::new(TransportPreference::BleOnly);
        mgr.ble_advertise_retry_delay = Duration::from_millis(5);
        mgr.start_ble_listener(local).await.unwrap();

        // Give the listener task time to start advertising.
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Inject a Handshake — chunk it like a real BLE characteristic write.
        let handshake = ProtocolMessage::Handshake {
            pubkey: central_pubkey,
        };
        let bytes = rmp_serde::to_vec(&handshake).unwrap();
        let chunker = MessageChunker { mtu: 4096 };
        for frame in chunker.chunk(&bytes) {
            let raw = crate::transport::ble_transport::encode_chunk(&frame);
            mgr.ble_inject().unwrap().send(raw).await.unwrap();
        }

        // Give the listener time to process and register.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // process_ble_incoming is called inside power_tick_at, which recv_any_at
        // invokes.  Call recv_any once to trigger the drain.
        let _ = mgr.recv_any_at(Duration::from_secs(0)).await;

        assert!(
            mgr.active_connections.contains_key(&central_pubkey),
            "Central's BlePeripheral should have been registered"
        );
        assert_eq!(mgr.connection_count(), 1);
    }

    #[cfg(feature = "ble")]
    #[tokio::test]
    async fn test_ble_listener_rejects_non_handshake_first_message() {
        let local = peer(0xAA);
        let mut mgr = TransportManager::new(TransportPreference::BleOnly);
        mgr.ble_advertise_retry_delay = Duration::from_millis(5);
        mgr.start_ble_listener(local).await.unwrap();

        tokio::time::sleep(Duration::from_millis(20)).await;

        // Inject a Transaction as the first message (not a Handshake).
        let tx = sample_transaction(1);
        let bytes = rmp_serde::to_vec(&tx).unwrap();
        let chunker = MessageChunker { mtu: 4096 };
        for frame in chunker.chunk(&bytes) {
            let raw = crate::transport::ble_transport::encode_chunk(&frame);
            mgr.ble_inject().unwrap().send(raw).await.unwrap();
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = mgr.recv_any_at(Duration::from_secs(0)).await;

        // No connection should have been registered.
        assert_eq!(mgr.connection_count(), 0);
    }

    #[cfg(feature = "ble")]
    #[tokio::test]
    async fn test_ble_listener_retries_on_advertising_failure() {
        // Make start_advertising fail the first two calls, then succeed.
        crate::transport::ble_transport::set_advertise_fail_count(2);

        let local = peer(0xAA);
        let central_pubkey = [0xCC; 32];
        let mut mgr = TransportManager::new(TransportPreference::BleOnly);
        mgr.ble_advertise_retry_delay = Duration::from_millis(5);
        mgr.start_ble_listener(local).await.unwrap();

        // Wait long enough for the two failures + retries + success.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Now inject a Handshake — the listener should be advertising.
        let handshake = ProtocolMessage::Handshake {
            pubkey: central_pubkey,
        };
        let bytes = rmp_serde::to_vec(&handshake).unwrap();
        let chunker = MessageChunker { mtu: 4096 };
        for frame in chunker.chunk(&bytes) {
            let raw = crate::transport::ble_transport::encode_chunk(&frame);
            mgr.ble_inject().unwrap().send(raw).await.unwrap();
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = mgr.recv_any_at(Duration::from_secs(0)).await;

        assert!(
            mgr.active_connections.contains_key(&central_pubkey),
            "Connection should be registered after advertising succeeds"
        );

        // Reset for other tests.
        crate::transport::ble_transport::set_advertise_fail_count(0);
    }

    #[tokio::test]
    async fn test_notify_mac_rotated_drops_ble_connections() {
        use crate::transport::ble_transport::BleCentral;

        let mut mgr = TransportManager::new(TransportPreference::Auto);
        let peer1 = peer(0x11);
        let peer2 = peer(0x22);

        // Create mock BLE connections
        let ble_conn = Box::new(BleCentral::new(peer1.clone()));
        mgr.active_connections.insert(peer1.pubkey, ble_conn);
        mgr.peer_count.fetch_add(1, Ordering::SeqCst);

        let ble_conn2 = Box::new(BleCentral::new(peer2.clone()));
        mgr.active_connections.insert(peer2.pubkey, ble_conn2);
        mgr.peer_count.fetch_add(1, Ordering::SeqCst);

        assert_eq!(mgr.connection_count(), 2);

        // Call notify_mac_rotated
        let new_mac = [0xAA, 0x02, 0x00, 0x00, 0x00, 0x00];
        mgr.notify_mac_rotated(new_mac).await;

        // All BLE connections should be dropped
        assert_eq!(
            mgr.connection_count(),
            0,
            "All BLE connections should be dropped after MAC rotation"
        );
        assert_eq!(mgr.peer_count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn test_power_tick_includes_mac_rotated_flag() {
        use crate::transport::ble_transport::{BleCentral, RandomMacProvider};

        let mut mgr = TransportManager::new(TransportPreference::Auto);

        // Create a BLE central with a very short rotation interval
        let mut ble_central = BleCentral::new(peer(0x99));
        ble_central = ble_central.with_rotation_interval(Duration::from_millis(1));
        mgr.ble_central_scanner = Some(ble_central);

        // Wait for the interval to elapse
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Call power_tick_at
        let outcome = mgr.power_tick_at(Duration::from_secs(1)).await.unwrap();

        // The outcome should include MacRotated flag
        assert!(
            outcome
                .topology_flags
                .contains(&crate::message::types::TopologyFlag::MacRotated),
            "topology_flags should contain MacRotated after rotation"
        );
    }

    #[tokio::test]
    async fn test_mac_rotated_flag_not_included_before_interval() {
        use crate::transport::ble_transport::BleCentral;

        let mut mgr = TransportManager::new(TransportPreference::Auto);

        // Create a BLE central with a long rotation interval (default 15 minutes)
        let ble_central = BleCentral::new(peer(0x88));
        mgr.ble_central_scanner = Some(ble_central);

        // Call power_tick_at immediately (interval has not elapsed)
        let outcome = mgr.power_tick_at(Duration::from_secs(1)).await.unwrap();

        // The outcome should NOT include MacRotated flag
        assert!(
            !outcome
                .topology_flags
                .contains(&crate::message::types::TopologyFlag::MacRotated),
            "topology_flags should not contain MacRotated before interval elapses"
        );
    }

    #[tokio::test]
    async fn test_mac_rotation_drops_existing_ble_connections() {
        use crate::transport::ble_transport::BleCentral;

        let mut mgr = TransportManager::new(TransportPreference::Auto);

        // Add a BLE connection
        let peer1 = peer(0x77);
        let ble_conn = Box::new(BleCentral::new(peer1.clone()));
        mgr.active_connections.insert(peer1.pubkey, ble_conn);
        mgr.peer_count.fetch_add(1, Ordering::SeqCst);

        // Create a BLE central with a very short rotation interval
        let mut ble_central = BleCentral::new(peer(0x66));
        ble_central = ble_central.with_rotation_interval(Duration::from_millis(1));
        mgr.ble_central_scanner = Some(ble_central);

        // Wait for the interval to elapse
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Call power_tick_at
        let outcome = mgr.power_tick_at(Duration::from_secs(1)).await.unwrap();

        // Verify the flag is present and connection was dropped
        assert!(
            outcome
                .topology_flags
                .contains(&crate::message::types::TopologyFlag::MacRotated),
            "topology_flags should contain MacRotated"
        );
        assert_eq!(
            mgr.connection_count(),
            0,
            "BLE connections should be dropped after MAC rotation"
        );
    }
}
