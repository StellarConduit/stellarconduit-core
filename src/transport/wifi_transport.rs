//! WiFi-Direct (TCP fallback) transport backend for StellarConduit.
//!
//! Assumes the WiFi-Direct P2P group has already been established externally.
//! This module operates purely over a Tokio `TcpStream`, handling:
//!   - 4-byte LE length-prefixed framing
//!   - `MessageChunker` / `MessageReassembler` for large `ProtocolMessage`s
//!   - Exponential reconnect backoff (up to `MAX_RECONNECT_ATTEMPTS`)

use std::net::SocketAddr;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::message::types::ProtocolMessage;
use crate::peer::identity::PeerIdentity;
use crate::transport::connection::{Connection, ConnectionState, TransportType};
use crate::transport::errors::TransportError;
use crate::transport::unified::{MessageChunker, MessageReassembler};

// ─── Constants ────────────────────────────────────────────────────────────────

/// TCP MTU for chunking — Ethernet MTU minus IP/TCP headers ≈ 1448 bytes.
pub const WIFI_TCP_MTU: usize = 1448;

/// Starting delay for exponential backoff in milliseconds.
pub const RECONNECT_BASE_DELAY_MS: u64 = 100;

/// Maximum number of reconnection attempts before giving up.
pub const MAX_RECONNECT_ATTEMPTS: u32 = 5;

// ─── WifiDirectConnection ─────────────────────────────────────────────────────

/// A WiFi-Direct connection backed by a Tokio TCP stream.
///
/// For `connect_to`: given an IP address of a peer in the established P2P group,
/// creates an outbound `TcpStream`.
///
/// For `accept_from`: wraps an inbound `TcpStream` accepted from a `TcpListener`.
pub struct WifiDirectConnection {
    remote_peer: PeerIdentity,
    state: ConnectionState,
    stream: Option<TcpStream>,
    addr: Option<SocketAddr>,
    chunker: MessageChunker,
    reassembler: MessageReassembler,
}

impl WifiDirectConnection {
    /// Creates and initiates a TCP connection to a known peer.
    pub async fn connect_to(peer: PeerIdentity, addr: SocketAddr) -> Result<Self, TransportError> {
        let stream = TcpStream::connect(addr)
            .await
            .map_err(|_| TransportError::ConnectionRefused)?;
        Ok(Self {
            remote_peer: peer,
            state: ConnectionState::Connected,
            stream: Some(stream),
            addr: Some(addr),
            chunker: MessageChunker { mtu: WIFI_TCP_MTU },
            reassembler: MessageReassembler::new(),
        })
    }

    /// Accepts an incoming TCP connection from a `TcpListener`.
    ///
    /// The caller is responsible for identifying `remote_peer` from the handshake
    /// (e.g., via the first message after connect). For now a placeholder identity
    /// is used and the caller should update it.
    pub async fn accept_from(
        listener: &TcpListener,
        remote_peer: PeerIdentity,
    ) -> Result<Self, TransportError> {
        let (stream, _addr) = listener
            .accept()
            .await
            .map_err(|_| TransportError::BrokenPipe)?;
        Ok(Self {
            remote_peer,
            state: ConnectionState::Connected,
            stream: Some(stream),
            addr: None,
            chunker: MessageChunker { mtu: WIFI_TCP_MTU },
            reassembler: MessageReassembler::new(),
        })
    }

    /// Attempt to reconnect using exponential backoff.
    ///
    /// Tries up to `MAX_RECONNECT_ATTEMPTS`. On each failure the delay doubles
    /// from `RECONNECT_BASE_DELAY_MS`. Returns `Err` if all attempts fail.
    async fn reconnect(&mut self) -> Result<(), TransportError> {
        let addr = self.addr.ok_or(TransportError::NotConnected)?;
        let mut delay_ms = RECONNECT_BASE_DELAY_MS;

        for attempt in 1..=MAX_RECONNECT_ATTEMPTS {
            log::debug!(
                "WiFi reconnect attempt {}/{}",
                attempt,
                MAX_RECONNECT_ATTEMPTS
            );
            tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;

            match TcpStream::connect(addr).await {
                Ok(stream) => {
                    self.stream = Some(stream);
                    self.state = ConnectionState::Connected;
                    return Ok(());
                }
                Err(_) => {
                    delay_ms *= 2;
                }
            }
        }

        self.state = ConnectionState::Disconnected;
        Err(TransportError::ConnectionRefused)
    }

    /// Write a framed chunk to the TCP stream: 4-byte LE length prefix + payload.
    async fn write_frame(stream: &mut TcpStream, data: &[u8]) -> Result<(), TransportError> {
        let len = data.len() as u32;
        stream
            .write_all(&len.to_le_bytes())
            .await
            .map_err(|_| TransportError::BrokenPipe)?;
        stream
            .write_all(data)
            .await
            .map_err(|_| TransportError::BrokenPipe)?;
        Ok(())
    }

    /// Read one framed message from the TCP stream (4-byte length prefix + payload).
    async fn read_frame(stream: &mut TcpStream) -> Result<Vec<u8>, TransportError> {
        let mut len_buf = [0u8; 4];
        stream
            .read_exact(&mut len_buf)
            .await
            .map_err(|_| TransportError::BrokenPipe)?;
        let len = u32::from_le_bytes(len_buf) as usize;

        if len == 0 || len > 4 * 1024 * 1024 {
            return Err(TransportError::PayloadTooLarge);
        }

        let mut buf = vec![0u8; len];
        stream
            .read_exact(&mut buf)
            .await
            .map_err(|_| TransportError::BrokenPipe)?;
        Ok(buf)
    }
}

// ─── Connection trait ─────────────────────────────────────────────────────────

#[async_trait]
impl Connection for WifiDirectConnection {
    fn remote_peer(&self) -> PeerIdentity {
        self.remote_peer.clone()
    }

    fn transport_type(&self) -> TransportType {
        TransportType::WifiDirect
    }

    fn state(&self) -> ConnectionState {
        self.state
    }

    async fn connect(&mut self) -> Result<(), TransportError> {
        if self.state == ConnectionState::Connected {
            return Ok(());
        }
        self.reconnect().await
    }

    async fn send(&mut self, msg: ProtocolMessage) -> Result<(), TransportError> {
        if self.state != ConnectionState::Connected {
            return Err(TransportError::NotConnected);
        }

        let bytes = rmp_serde::to_vec(&msg).map_err(|_| TransportError::BrokenPipe)?;
        let frames = self.chunker.chunk(&bytes);

        let stream = self.stream.as_mut().ok_or(TransportError::NotConnected)?;

        for frame in &frames {
            // Encode chunk frame header + payload as the "data" to write
            let mut frame_bytes = Vec::with_capacity(14 + frame.payload.len());
            frame_bytes.extend_from_slice(&frame.message_id.to_le_bytes());
            frame_bytes.extend_from_slice(&frame.total_length.to_le_bytes());
            frame_bytes.extend_from_slice(&frame.offset.to_le_bytes());
            frame_bytes.extend_from_slice(&frame.payload_size.to_le_bytes());
            frame_bytes.extend_from_slice(&frame.payload);

            Self::write_frame(stream, &frame_bytes).await?;
        }

        Ok(())
    }

    async fn recv(&mut self) -> Result<ProtocolMessage, TransportError> {
        if self.state != ConnectionState::Connected {
            return Err(TransportError::NotConnected);
        }

        let stream = self.stream.as_mut().ok_or(TransportError::NotConnected)?;

        loop {
            let raw = Self::read_frame(stream).await?;

            // Decode the chunk frame from the raw bytes
            if raw.len() < 14 {
                return Err(TransportError::BrokenPipe);
            }
            let message_id = u32::from_le_bytes(
                raw[0..4]
                    .try_into()
                    .map_err(|_| TransportError::BrokenPipe)?,
            );
            let total_length = u32::from_le_bytes(
                raw[4..8]
                    .try_into()
                    .map_err(|_| TransportError::BrokenPipe)?,
            );
            let offset = u32::from_le_bytes(
                raw[8..12]
                    .try_into()
                    .map_err(|_| TransportError::BrokenPipe)?,
            );
            let payload_size = u16::from_le_bytes(
                raw[12..14]
                    .try_into()
                    .map_err(|_| TransportError::BrokenPipe)?,
            );
            let payload = raw[14..].to_vec();

            let chunk = crate::transport::unified::ChunkFrame {
                message_id,
                total_length,
                offset,
                payload_size,
                payload,
            };

            if let Some(assembled_bytes) = self.reassembler.receive_chunk(chunk) {
                let msg = rmp_serde::from_slice(&assembled_bytes)
                    .map_err(|_| TransportError::BrokenPipe)?;
                return Ok(msg);
            }
        }
    }

    async fn disconnect(&mut self) -> Result<(), TransportError> {
        if let Some(stream) = self.stream.take() {
            let _ = stream.into_std(); // Drops and closes the stream
        }
        self.state = ConnectionState::Disconnected;
        Ok(())
    }
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::types::{ProtocolMessage, TopologyUpdate};
    use std::net::{IpAddr, Ipv4Addr};

    fn make_peer(b: u8) -> PeerIdentity {
        PeerIdentity::new([b; 32])
    }

    fn topo_msg(b: u8) -> ProtocolMessage {
        ProtocolMessage::TopologyUpdate(TopologyUpdate {
            origin_pubkey: [b; 32],
            directly_connected_peers: vec![[0u8; 32], [1u8; 32]],
            hops_to_relay: 3,
            topology_flags: vec![],
        })
    }

    // ── connect_to / accept_from round-trip ───────────────────────────────────

    #[tokio::test]
    async fn connect_and_accept_roundtrip() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let peer_a = make_peer(0xAA);
        let peer_b = make_peer(0xBB);

        let server_peer = peer_a.clone();
        let server_task = tokio::spawn(async move {
            WifiDirectConnection::accept_from(&listener, server_peer)
                .await
                .unwrap()
        });

        let mut client = WifiDirectConnection::connect_to(peer_b.clone(), addr)
            .await
            .unwrap();
        let mut server = server_task.await.unwrap();

        assert_eq!(client.state(), ConnectionState::Connected);
        assert_eq!(server.state(), ConnectionState::Connected);
        assert_eq!(client.transport_type(), TransportType::WifiDirect);

        // Send from client → server
        let msg = topo_msg(1);
        client.send(msg.clone()).await.unwrap();
        let received = server.recv().await.unwrap();
        assert_eq!(received, msg);

        client.disconnect().await.unwrap();
        assert_eq!(client.state(), ConnectionState::Disconnected);
    }

    // ── bidirectional round-trip ───────────────────────────────────────────────

    #[tokio::test]
    async fn bidirectional_send_recv() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_task = tokio::spawn(async move {
            WifiDirectConnection::accept_from(&listener, make_peer(0xAA))
                .await
                .unwrap()
        });

        let mut client = WifiDirectConnection::connect_to(make_peer(0xBB), addr)
            .await
            .unwrap();
        let mut server = server_task.await.unwrap();

        // client → server
        let msg1 = topo_msg(10);
        client.send(msg1.clone()).await.unwrap();
        assert_eq!(server.recv().await.unwrap(), msg1);

        // server → client
        let msg2 = topo_msg(20);
        server.send(msg2.clone()).await.unwrap();
        assert_eq!(client.recv().await.unwrap(), msg2);
    }

    // ── state guard: send while disconnected ──────────────────────────────────

    #[tokio::test]
    async fn send_fails_when_disconnected() {
        let mut conn = WifiDirectConnection {
            remote_peer: make_peer(1),
            state: ConnectionState::Disconnected,
            stream: None,
            addr: None,
            chunker: MessageChunker { mtu: WIFI_TCP_MTU },
            reassembler: MessageReassembler::new(),
        };
        let result = conn.send(topo_msg(1)).await;
        assert_eq!(result, Err(TransportError::NotConnected));
    }

    // ── exponential backoff reconnect ─────────────────────────────────────────

    #[tokio::test]
    async fn reconnect_fails_after_max_attempts() {
        // Port 1 is reserved and will refuse connections everywhere.
        let unreachable_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1);
        let mut conn = WifiDirectConnection {
            remote_peer: make_peer(2),
            state: ConnectionState::Disconnected,
            stream: None,
            addr: Some(unreachable_addr),
            chunker: MessageChunker { mtu: WIFI_TCP_MTU },
            reassembler: MessageReassembler::new(),
        };

        let result = conn.reconnect().await;
        assert_eq!(result, Err(TransportError::ConnectionRefused));
        assert_eq!(conn.state(), ConnectionState::Disconnected);
    }

    // ── disconnect clears state ────────────────────────────────────────────────

    #[tokio::test]
    async fn disconnect_transitions_to_disconnected() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let _server_task = tokio::spawn(async move {
            let _ = WifiDirectConnection::accept_from(&listener, make_peer(0xAA)).await;
        });

        let mut client = WifiDirectConnection::connect_to(make_peer(0xBB), addr)
            .await
            .unwrap();
        client.disconnect().await.unwrap();
        assert_eq!(client.state(), ConnectionState::Disconnected);
    }
}
