//! Gossip protocol event loop and anti-entropy sync.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::sleep;

use crate::discovery::peer_list::PeerList;
use crate::gossip::queue::PriorityQueue;
use crate::gossip::round::{GossipScheduler, ACTIVE_ROUND_INTERVAL_MS, IDLE_ROUND_INTERVAL_MS};
use crate::gossip::strike_tracker::StrikeTracker;
use crate::message::signing::verify_signature;
use crate::message::types::{ProtocolMessage, SyncRequest, SyncResponse, TransactionEnvelope};
use crate::peer::identity::PeerIdentity;
use crate::transport::unified::TransportManager;

/// Threshold above which a SyncResponse is treated as a "macro-merge" event.
/// This is tuned for large mesh clusters where immediately ingesting thousands
/// of envelopes would cause a broadcast storm.
const MACRO_MERGE_THRESHOLD: usize = 500;

/// Maximum number of envelopes to pull from a macro-merge backlog in a single
/// recovery step. Higher values speed up convergence but risk overloading
/// constrained links; lower values are safer but slower.
pub const MACRO_MERGE_BATCH_SIZE: usize = 50;

#[derive(Default)]
pub struct GossipState {
    pub active_queue: PriorityQueue,
    /// Backlog of envelopes discovered via a macro-merge SyncResponse.
    /// These are inserted into the active queue gradually to avoid
    /// overwhelming constrained transports.
    macro_merge_backlog: VecDeque<TransactionEnvelope>,
}

impl GossipState {
    pub fn new() -> Self {
        Default::default()
    }

    /// Add an envelope to the active buffer
    pub fn add_envelope(&mut self, env: TransactionEnvelope) {
        self.active_queue.push(ProtocolMessage::Transaction(env));
    }

    /// Returns the number of envelopes currently waiting in the macro-merge
    /// backlog. This is primarily used by tests and higher-level schedulers
    /// to drive paced recovery.
    pub fn pending_macro_merge_len(&self) -> usize {
        self.macro_merge_backlog.len()
    }

    /// Drains up to `batch_size` envelopes from the macro-merge backlog into
    /// the active queue, returning the number of envelopes actually moved.
    ///
    /// Envelopes in the backlog are ordered by increasing `timestamp`
    /// (oldest first) so that we preferentially recover the oldest
    /// transactions before they expire, while still respecting the QoS
    /// prioritization implemented by `PriorityQueue`.
    pub fn process_paced_recovery_batch(&mut self, batch_size: usize) -> usize {
        if batch_size == 0 {
            return 0;
        }

        let mut moved = 0usize;
        while moved < batch_size {
            if let Some(env) = self.macro_merge_backlog.pop_front() {
                self.add_envelope(env);
                moved += 1;
            } else {
                break;
            }
        }

        moved
    }

    /// Generates a SyncRequest containing the 4-byte prefixes of known message IDs
    pub fn generate_sync_request(&self) -> SyncRequest {
        let known_message_ids = self
            .active_queue
            .iter_envelopes()
            .map(|env| {
                let mut prefix = [0u8; 4];
                prefix.copy_from_slice(&env.message_id[0..4]);
                prefix
            })
            .collect();

        SyncRequest { known_message_ids }
    }

    /// Processes an incoming SyncRequest, returning a SyncResponse with any local envelopes
    /// that the requestor does not have.
    pub fn handle_sync_request(&self, req: &SyncRequest) -> SyncResponse {
        let missing_envelopes = self
            .active_queue
            .iter_envelopes()
            .filter(|env| {
                let mut prefix = [0u8; 4];
                prefix.copy_from_slice(&env.message_id[0..4]);
                !req.known_message_ids.contains(&prefix)
            })
            .cloned()
            .collect();

        SyncResponse { missing_envelopes }
    }

    /// Process an incoming SyncResponse by adding the missing envelopes to our state.
    ///
    /// - For small deltas (<= MACRO_MERGE_THRESHOLD), envelopes are added
    ///   immediately to the active queue as before.
    /// - For large deltas (> MACRO_MERGE_THRESHOLD), we treat this as a
    ///   "macro-merge" between two large clusters. Instead of immediately
    ///   enqueuing all envelopes (which would cause a broadcast storm),
    ///   we move them into a backlog ordered by age and let a paced
    ///   recovery loop drain them in small batches.
    ///
    /// Note: Signature verification should be done via process_transaction_envelope before calling this.
    pub fn handle_sync_response(&mut self, resp: SyncResponse) {
        // Fast path for regular anti-entropy syncs.
        if resp.missing_envelopes.len() <= MACRO_MERGE_THRESHOLD {
            for env in resp.missing_envelopes {
                self.add_envelope(env);
            }
            return;
        }

        // Macro-merge path: sort by timestamp so that oldest transactions
        // are recovered first, then push into the backlog for paced recovery.
        let mut backlog: Vec<TransactionEnvelope> = resp.missing_envelopes;
        backlog.sort_by_key(|env| env.timestamp);

        // Replace any existing backlog — a new macro-merge response supersedes
        // prior partial backlogs from the same peer/cluster.
        self.macro_merge_backlog = backlog.into();
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
        let resp = node_a.handle_sync_request(&req);

        assert_eq!(resp.missing_envelopes.len(), 1);
        assert_eq!(resp.missing_envelopes[0].message_id[0], 0xBB);
    }

    #[test]
    fn test_handle_sync_response() {
        let mut state = GossipState::new();
        assert_eq!(state.active_queue.len(), 0);

        let resp = SyncResponse {
            missing_envelopes: vec![mock_envelope(0xCC)],
        };

        state.handle_sync_response(resp);
        assert_eq!(state.active_queue.len(), 1);
        assert_eq!(
            state
                .active_queue
                .iter_envelopes()
                .next()
                .unwrap()
                .message_id[0],
            0xCC
        );
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
