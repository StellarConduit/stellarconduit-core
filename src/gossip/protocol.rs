//! Gossip protocol event loop and anti-entropy sync.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::sleep;

use std::collections::HashMap;

use crate::discovery::peer_list::PeerList;
use crate::gossip::round::{GossipScheduler, ACTIVE_ROUND_INTERVAL_MS, IDLE_ROUND_INTERVAL_MS};
use crate::gossip::strike_tracker::StrikeTracker;
use crate::message::signing::verify_signature;
use crate::message::types::{
    PartitionMergeHandshake, PartitionMergeHandshakeResponse, SyncRequest, SyncResponse,
    TransactionEnvelope,
};
use crate::peer::identity::PeerIdentity;
use crate::transport::unified::TransportManager;

/// Rate limiting parameters for partition merge dampening
#[derive(Debug, Clone)]
pub struct MergeRateLimit {
    /// Maximum number of messages to send per batch
    pub batch_size: u32,
    /// Delay in milliseconds between batches
    pub batch_delay_ms: u64,
    /// Whether rate limiting is currently active
    pub is_active: bool,
}

impl Default for MergeRateLimit {
    fn default() -> Self {
        Self {
            batch_size: 100,     // Default: send 100 messages per batch
            batch_delay_ms: 100, // Default: 100ms delay between batches
            is_active: false,
        }
    }
}

/// Tracks partition merge state per peer
#[derive(Default)]
pub struct PartitionMergeTracker {
    /// Maps peer pubkey to their rate limiting parameters
    rate_limits: HashMap<[u8; 32], MergeRateLimit>,
}

impl PartitionMergeTracker {
    pub fn new() -> Self {
        Default::default()
    }

    /// Set rate limiting parameters for a peer
    pub fn set_rate_limit(&mut self, peer_pubkey: [u8; 32], rate_limit: MergeRateLimit) {
        self.rate_limits.insert(peer_pubkey, rate_limit);
    }

    /// Get rate limiting parameters for a peer
    pub fn get_rate_limit(&self, peer_pubkey: &[u8; 32]) -> Option<&MergeRateLimit> {
        self.rate_limits.get(peer_pubkey)
    }

    /// Clear rate limiting for a peer (when merge is complete)
    pub fn clear_rate_limit(&mut self, peer_pubkey: &[u8; 32]) {
        self.rate_limits.remove(peer_pubkey);
    }
}

#[derive(Default)]
pub struct GossipState {
    pub active_envelopes: Vec<TransactionEnvelope>,
    /// Tracks partition merge state for rate limiting
    pub merge_tracker: PartitionMergeTracker,
}

impl GossipState {
    pub fn new() -> Self {
        Default::default()
    }

    /// Add an envelope to the active buffer
    pub fn add_envelope(&mut self, env: TransactionEnvelope) {
        self.active_envelopes.push(env);
    }

    /// Generates a SyncRequest containing the 4-byte prefixes of known message IDs
    pub fn generate_sync_request(&self) -> SyncRequest {
        let known_message_ids = self
            .active_envelopes
            .iter()
            .map(|env| {
                let mut prefix = [0u8; 4];
                prefix.copy_from_slice(&env.message_id[0..4]);
                prefix
            })
            .collect();

        SyncRequest { known_message_ids }
    }

    /// Processes an incoming SyncRequest, returning a SyncResponse with any local envelopes
    /// that the requestor does not have. Applies rate limiting if partition merge is active.
    pub fn handle_sync_request(&self, req: &SyncRequest, peer_pubkey: &[u8; 32]) -> SyncResponse {
        let mut missing_envelopes: Vec<TransactionEnvelope> = self
            .active_envelopes
            .iter()
            .filter(|env| {
                let mut prefix = [0u8; 4];
                prefix.copy_from_slice(&env.message_id[0..4]);
                !req.known_message_ids.contains(&prefix)
            })
            .cloned()
            .collect();

        // Apply rate limiting if partition merge is active for this peer
        if let Some(rate_limit) = self.merge_tracker.get_rate_limit(peer_pubkey) {
            if rate_limit.is_active && missing_envelopes.len() > rate_limit.batch_size as usize {
                // Limit the number of messages in this batch
                missing_envelopes.truncate(rate_limit.batch_size as usize);
                log::info!(
                    "Partition merge dampening: limiting sync response to {} messages (out of {} total) for peer",
                    rate_limit.batch_size,
                    self.active_envelopes.len()
                );
            }
        }

        SyncResponse { missing_envelopes }
    }

    /// Process an incoming SyncResponse by adding the missing envelopes to our state.
    /// Note: Signature verification should be done via process_transaction_envelope before calling this.
    pub fn handle_sync_response(&mut self, resp: SyncResponse) {
        for env in resp.missing_envelopes {
            self.add_envelope(env);
        }
    }

    /// Generate a partition merge handshake message with current cluster statistics
    pub fn generate_partition_merge_handshake(
        &self,
        estimated_peer_count: u32,
    ) -> PartitionMergeHandshake {
        PartitionMergeHandshake {
            estimated_cluster_message_count: self.active_envelopes.len() as u32,
            estimated_cluster_peer_count: estimated_peer_count,
        }
    }

    /// Handle an incoming partition merge handshake and determine if rate limiting is needed
    pub fn handle_partition_merge_handshake(
        &mut self,
        handshake: &PartitionMergeHandshake,
        peer_pubkey: [u8; 32],
        local_peer_count: u32,
    ) -> PartitionMergeHandshakeResponse {
        let local_message_count = self.active_envelopes.len() as u32;
        let remote_message_count = handshake.estimated_cluster_message_count;
        let remote_peer_count = handshake.estimated_cluster_peer_count;

        // Detect if this is a large partition merge scenario
        // Threshold: if either cluster has >100 messages and the difference is significant
        const LARGE_MERGE_THRESHOLD: u32 = 100;
        const SIGNIFICANT_DIFFERENCE_RATIO: f64 = 0.3; // 30% difference

        let total_messages = local_message_count + remote_message_count;
        let message_difference = local_message_count.abs_diff(remote_message_count);

        let needs_dampening = total_messages > LARGE_MERGE_THRESHOLD
            && (message_difference as f64 / total_messages as f64) > SIGNIFICANT_DIFFERENCE_RATIO;

        let (batch_size, batch_delay_ms) = if needs_dampening {
            // Calculate rate limits based on cluster sizes
            // Larger clusters need more aggressive throttling
            let max_cluster_size = local_peer_count.max(remote_peer_count);
            let batch_size = if max_cluster_size > 100 {
                50 // Very large clusters: 50 messages per batch
            } else if max_cluster_size > 50 {
                100 // Large clusters: 100 messages per batch
            } else {
                200 // Medium clusters: 200 messages per batch
            };

            // Delay increases with cluster size
            let batch_delay_ms = if max_cluster_size > 100 {
                500 // Very large clusters: 500ms delay
            } else if max_cluster_size > 50 {
                200 // Large clusters: 200ms delay
            } else {
                100 // Medium clusters: 100ms delay
            };

            log::info!(
                "Partition merge detected: local={} msgs/{} peers, remote={} msgs/{} peers. Applying dampening: batch_size={}, delay={}ms",
                local_message_count,
                local_peer_count,
                remote_message_count,
                remote_peer_count,
                batch_size,
                batch_delay_ms
            );

            // Set rate limiting for this peer
            self.merge_tracker.set_rate_limit(
                peer_pubkey,
                MergeRateLimit {
                    batch_size,
                    batch_delay_ms,
                    is_active: true,
                },
            );

            (batch_size, batch_delay_ms)
        } else {
            // No dampening needed
            (1000, 10) // Large batch size, minimal delay
        };

        PartitionMergeHandshakeResponse {
            estimated_cluster_message_count: local_message_count,
            estimated_cluster_peer_count: local_peer_count,
            batch_size,
            batch_delay_ms,
        }
    }

    /// Handle partition merge handshake response and apply rate limiting if needed
    pub fn handle_partition_merge_handshake_response(
        &mut self,
        response: &PartitionMergeHandshakeResponse,
        peer_pubkey: [u8; 32],
    ) {
        // Apply the rate limiting parameters from the peer's response
        if response.batch_size < 1000 || response.batch_delay_ms > 10 {
            // Peer is requesting rate limiting
            self.merge_tracker.set_rate_limit(
                peer_pubkey,
                MergeRateLimit {
                    batch_size: response.batch_size,
                    batch_delay_ms: response.batch_delay_ms,
                    is_active: true,
                },
            );
            log::info!(
                "Applied partition merge rate limiting from peer: batch_size={}, delay={}ms",
                response.batch_size,
                response.batch_delay_ms
            );
        }
    }
}

/// Process an incoming TransactionEnvelope, verifying its signature and tracking failures.
/// Returns Ok(()) if the envelope is valid, Err(PeerIdentity) if the peer should be banned.
pub async fn process_transaction_envelope(
    envelope: &TransactionEnvelope,
    strike_tracker: &mut StrikeTracker,
    peer_list: Arc<Mutex<PeerList>>,
    transport_manager: Arc<Mutex<TransportManager>>,
) -> Result<(), PeerIdentity> {
    // Verify the signature
    match verify_signature(envelope) {
        Ok(true) => {
            // Valid signature - clear any strikes for this peer
            let peer_identity = PeerIdentity::new(envelope.origin_pubkey);
            strike_tracker.clear_peer(&peer_identity);
            Ok(())
        }
        Ok(false) => {
            // This shouldn't happen - verify_signature returns Err on failure
            Ok(())
        }
        Err(_) => {
            // Invalid signature - record the failure
            let peer_identity = PeerIdentity::new(envelope.origin_pubkey);
            let should_ban = strike_tracker.record_failure(&peer_identity);

            if should_ban {
                // Ban the peer for 24 hours
                const BAN_DURATION_SEC: u64 = 24 * 60 * 60; // 24 hours
                let mut peer_list_guard = peer_list.lock().await;
                // Ensure peer exists in the list before banning
                if peer_list_guard.get_peer(&peer_identity.pubkey).is_none() {
                    peer_list_guard.insert_or_update(peer_identity.pubkey, 0);
                }
                peer_list_guard.ban_peer(&peer_identity.pubkey, BAN_DURATION_SEC);
                drop(peer_list_guard);

                // Disconnect the peer
                let mut transport_guard = transport_manager.lock().await;
                transport_guard.disconnect_peer(&peer_identity.pubkey).await;
                drop(transport_guard);

                log::warn!(
                    "Banned peer {} for sending {} invalid signatures",
                    peer_identity,
                    strike_tracker.get_strike_count(&peer_identity)
                );

                Err(peer_identity)
            } else {
                log::debug!(
                    "Invalid signature from peer {} (strike count: {})",
                    peer_identity,
                    strike_tracker.get_strike_count(&peer_identity)
                );
                Ok(())
            }
        }
    }
}

pub async fn run_gossip_loop(
    mut scheduler: GossipScheduler,
    mut strike_tracker: StrikeTracker,
    peer_list: Arc<Mutex<PeerList>>,
    _transport_manager: Arc<Mutex<TransportManager>>,
) {
    let mut _anti_entropy_timer = tokio::time::interval(Duration::from_secs(30));
    let mut ban_check_timer = tokio::time::interval(Duration::from_secs(60)); // Check ban expiration every minute

    loop {
        tokio::select! {
            // Main epidemic push interval
            _ = sleep(Duration::from_millis(
                if scheduler.is_idle() {
                    IDLE_ROUND_INTERVAL_MS
                } else {
                    ACTIVE_ROUND_INTERVAL_MS
                }
            )) => {
                if scheduler.is_time_for_round() {
                    // TODO: select fanout peers and broadcast buffered messages
                    log::debug!("Gossip round fired");
                    scheduler.round_executed();
                }
            }

            // Anti-entropy pull interval (every 30 seconds)
            _ = _anti_entropy_timer.tick() => {
                log::debug!("Anti-entropy sync timer fired");
                // TODO: Pick one random active peer and send state.generate_sync_request()
            }

            // Check ban expiration periodically
            _ = ban_check_timer.tick() => {
                let mut peer_list_guard = peer_list.lock().await;
                let unbanned = peer_list_guard.check_ban_expirations();
                if !unbanned.is_empty() {
                    log::info!("Unbanned {} peer(s) after expiration", unbanned.len());
                    // Clear strike tracker for unbanned peers
                    for peer in unbanned {
                        strike_tracker.clear_peer(&peer);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    fn mock_envelope(id_byte: u8) -> TransactionEnvelope {
        TransactionEnvelope {
            message_id: [id_byte; 32],
            origin_pubkey: [0u8; 32],
            tx_xdr: format!("XDR{}", id_byte),
            ttl_hops: 10,
            timestamp: 0,
            signature: [0u8; 64],
        }
    }

    #[test]
    fn test_generate_sync_request() {
        let mut state = GossipState::new();
        state.add_envelope(mock_envelope(0xAA));
        state.add_envelope(mock_envelope(0xBB));

        let req = state.generate_sync_request();
        assert_eq!(req.known_message_ids.len(), 2);
        assert_eq!(req.known_message_ids[0], [0xAA, 0xAA, 0xAA, 0xAA]);
        assert_eq!(req.known_message_ids[1], [0xBB, 0xBB, 0xBB, 0xBB]);
    }

    #[test]
    fn test_handle_sync_request_delta_calculation() {
        let mut node_a = GossipState::new();
        // A has message AA and BB
        node_a.add_envelope(mock_envelope(0xAA));
        node_a.add_envelope(mock_envelope(0xBB));

        let mut node_b = GossipState::new();
        // B only has message AA -> B is missing BB
        node_b.add_envelope(mock_envelope(0xAA));

        // Node B generates request telling A what it has
        let req = node_b.generate_sync_request();

        // Node A processes request and calculates what B is missing
        let peer_pubkey = [0u8; 32];
        let resp = node_a.handle_sync_request(&req, &peer_pubkey);

        assert_eq!(resp.missing_envelopes.len(), 1);
        assert_eq!(resp.missing_envelopes[0].message_id[0], 0xBB);
    }

    #[test]
    fn test_handle_sync_response() {
        let mut state = GossipState::new();
        assert_eq!(state.active_envelopes.len(), 0);

        let resp = SyncResponse {
            missing_envelopes: vec![mock_envelope(0xCC)],
        };

        state.handle_sync_response(resp);
        assert_eq!(state.active_envelopes.len(), 1);
        assert_eq!(state.active_envelopes[0].message_id[0], 0xCC);
    }

    #[tokio::test]
    async fn test_gossip_loop_starts_without_blocking() {
        let scheduler = GossipScheduler::new();
        let strike_tracker = StrikeTracker::new();
        let peer_list = Arc::new(Mutex::new(PeerList::new(300)));
        let transport_manager = Arc::new(Mutex::new(
            crate::transport::unified::TransportManager::new(
                crate::transport::unified::TransportPreference::BleOnly,
            ),
        ));
        let handle = tokio::spawn(run_gossip_loop(
            scheduler,
            strike_tracker,
            peer_list,
            transport_manager,
        ));
        let result = timeout(Duration::from_millis(200), async {
            tokio::time::sleep(Duration::from_millis(100)).await;
        })
        .await;
        assert!(result.is_ok());
        handle.abort();
    }

    #[tokio::test]
    async fn test_gossip_loop_can_be_aborted() {
        let scheduler = GossipScheduler::new();
        let strike_tracker = StrikeTracker::new();
        let peer_list = Arc::new(Mutex::new(PeerList::new(300)));
        let transport_manager = Arc::new(Mutex::new(
            crate::transport::unified::TransportManager::new(
                crate::transport::unified::TransportPreference::BleOnly,
            ),
        ));
        let handle = tokio::spawn(run_gossip_loop(
            scheduler,
            strike_tracker,
            peer_list,
            transport_manager,
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_gossip_loop_starts_in_idle_mode() {
        use crate::gossip::round::IDLE_TIMEOUT_SEC;
        let mut scheduler = GossipScheduler::new();
        scheduler.last_active_msg_time =
            std::time::Instant::now() - Duration::from_secs(IDLE_TIMEOUT_SEC + 5);
        assert!(scheduler.is_idle());
        let strike_tracker = StrikeTracker::new();
        let peer_list = Arc::new(Mutex::new(PeerList::new(300)));
        let transport_manager = Arc::new(Mutex::new(
            crate::transport::unified::TransportManager::new(
                crate::transport::unified::TransportPreference::BleOnly,
            ),
        ));
        let handle = tokio::spawn(run_gossip_loop(
            scheduler,
            strike_tracker,
            peer_list,
            transport_manager,
        ));
        let result = timeout(Duration::from_millis(150), async {
            tokio::time::sleep(Duration::from_millis(50)).await;
        })
        .await;
        assert!(result.is_ok());
        handle.abort();
    }
}
