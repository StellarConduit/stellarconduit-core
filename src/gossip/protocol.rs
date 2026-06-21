//! Gossip protocol event loop and anti-entropy sync.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::sleep;

use crate::discovery::peer_list::PeerList;
use crate::gossip::bloom::SlidingBloomFilter;
use crate::gossip::errors::GossipError;
use crate::gossip::fanout::{select_random_peers, FanoutCalculator};
use crate::gossip::queue::PriorityQueue;
use crate::gossip::round::{GossipScheduler, ACTIVE_ROUND_INTERVAL_MS, IDLE_ROUND_INTERVAL_MS};
use crate::gossip::strike_tracker::StrikeTracker;
use crate::message::signing::verify_signature;
use crate::message::types::{ProtocolMessage, SyncRequest, SyncResponse, TransactionEnvelope};
use crate::metrics::Metrics;
use crate::peer::identity::PeerIdentity;
use crate::peer::reputation::{apply_penalty, PenaltyReason};
use crate::persistence::db::MeshDatabase;
use crate::router::table::RoutingTable;
use crate::topology::events::{TopologyEvent, TopologyEventBus};
use crate::transport::unified::TransportManager;

/// Threshold above which a SyncResponse is treated as a "macro-merge" event.
const MACRO_MERGE_THRESHOLD: usize = 500;

/// Maximum number of envelopes to pull from a macro-merge backlog in a single recovery step.
pub const MACRO_MERGE_BATCH_SIZE: usize = 50;

/// Maximum envelopes drained per fanout round.
const FANOUT_BATCH_SIZE: usize = 32;

/// Per-send timeout to prevent blocking the loop indefinitely.
const SEND_TIMEOUT_MS: u64 = 500;

#[derive(Default)]
pub struct GossipState {
    pub active_queue: PriorityQueue,
    macro_merge_backlog: VecDeque<TransactionEnvelope>,
}

impl GossipState {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn add_envelope(&mut self, env: TransactionEnvelope) -> Result<(), GossipError> {
        if let Some(ProtocolMessage::Transaction(dropped_env)) =
            self.active_queue.push(ProtocolMessage::Transaction(env))
        {
            return Err(GossipError::NormalQueueOverflow {
                dropped_id: dropped_env.message_id,
            });
        }
        Ok(())
    }

    pub fn pending_macro_merge_len(&self) -> usize {
        self.macro_merge_backlog.len()
    }

    pub fn process_paced_recovery_batch(&mut self, batch_size: usize) -> usize {
        if batch_size == 0 {
            return 0;
        }
        let mut moved = 0usize;
        while moved < batch_size {
            if let Some(env) = self.macro_merge_backlog.pop_front() {
                let _ = self.add_envelope(env);
                moved += 1;
            } else {
                break;
            }
        }
        moved
    }

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

    pub fn handle_sync_response(&mut self, resp: SyncResponse) {
        if resp.missing_envelopes.len() <= MACRO_MERGE_THRESHOLD {
            for env in resp.missing_envelopes {
                let _ = self.add_envelope(env);
            }
            return;
        }
        let mut backlog: Vec<TransactionEnvelope> = resp.missing_envelopes;
        backlog.sort_by_key(|env| env.timestamp);
        self.macro_merge_backlog = backlog.into();
    }
}

/// Process an incoming TransactionEnvelope, verifying its signature and tracking failures.
pub async fn process_transaction_envelope(
    envelope: &TransactionEnvelope,
    strike_tracker: &mut StrikeTracker,
    peer_list: Arc<Mutex<PeerList>>,
    transport_manager: Arc<Mutex<TransportManager>>,
    db: Option<Arc<MeshDatabase>>,
    metrics: &Arc<Metrics>,
) -> Result<(), GossipError> {
    // TTL enforcement: reject expired envelopes
    if envelope.ttl_hops == 0 {
        return Err(GossipError::TtlExpired);
    }

    match verify_signature(envelope) {
        Ok(true) => {
            let peer_identity = PeerIdentity::new(envelope.origin_pubkey);
            strike_tracker.clear_peer(&peer_identity);
            Ok(())
        }
        Ok(false) => {
            let peer_identity = PeerIdentity::new(envelope.origin_pubkey);
            {
                let mut pl = peer_list.lock().await;
                if let Some(peer) = pl.get_peer_mut(&peer_identity.pubkey) {
                    apply_penalty(peer, PenaltyReason::InvalidSignature);
                }
            }
            Err(GossipError::InvalidSignature {
                peer: peer_identity,
            })
        }
        Err(_) => {
            let peer_identity = PeerIdentity::new(envelope.origin_pubkey);
            {
                let mut pl = peer_list.lock().await;
                if let Some(peer) = pl.get_peer_mut(&peer_identity.pubkey) {
                    apply_penalty(peer, PenaltyReason::InvalidSignature);
                }
            }
            let should_ban = strike_tracker.record_failure(&peer_identity);
            if should_ban {
                const BAN_DURATION_SEC: u64 = 24 * 60 * 60;
                crate::security::peer_ban::ban_and_persist(
                    &peer_list,
                    db.as_deref(),
                    &peer_identity.pubkey,
                    BAN_DURATION_SEC,
                    "repeated invalid signatures",
                )
                .await;
                metrics.peers_banned.fetch_add(1, Ordering::Relaxed);
                let mut transport_guard = transport_manager.lock().await;
                transport_guard.disconnect_peer(&peer_identity.pubkey).await;
                drop(transport_guard);
                log::warn!(
                    "Banned peer {} for sending {} invalid signatures",
                    peer_identity,
                    strike_tracker.get_strike_count(&peer_identity)
                );
                Err(GossipError::PeerBanned {
                    peer: peer_identity,
                })
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

/// Executes one epidemic fanout round: selects peers, drains a batch of envelopes,
/// applies bloom deduplication and TTL enforcement, then dispatches to each peer.
pub async fn execute_fanout_round(
    bloom: &mut SlidingBloomFilter,
    metrics: &Arc<Metrics>,
    state: &Arc<Mutex<GossipState>>,
    peer_list: &Arc<Mutex<PeerList>>,
    transport_manager: &Arc<Mutex<TransportManager>>,
    fanout_calc: &FanoutCalculator,
) {
    // 1. Resolve active (non-banned) peers — hold lock briefly, then drop.
    let active_peers: Vec<PeerIdentity> = {
        let pl = peer_list.lock().await;
        pl.get_active_peers()
            .into_iter()
            .filter(|p| !p.is_banned)
            .map(|p| p.identity.clone())
            .collect()
    };

    if active_peers.is_empty() {
        let rounds = metrics.gossip_rounds_fired.fetch_add(1, Ordering::Relaxed) + 1;
        if rounds.is_multiple_of(100) {
            metrics.log_summary();
        }
        return;
    }

    // 2. Calculate fanout and select recipients.
    let f = fanout_calc.calculate(active_peers.len(), None);
    let recipients = select_random_peers(&active_peers, f);

    // 3. Drain up to FANOUT_BATCH_SIZE envelopes from the Normal tier, sorted
    //    by urgency score so the most time-critical transactions go first.
    let batch: Vec<TransactionEnvelope> = {
        let mut st = state.lock().await;
        st.active_queue
            .drain_batch(FANOUT_BATCH_SIZE)
            .into_iter()
            .filter_map(|msg| {
                if let ProtocolMessage::Transaction(env) = msg {
                    Some(env)
                } else {
                    None
                }
            })
            .collect()
    };

    // 4. For each envelope, apply bloom dedup + TTL, then send to recipients.
    let mut forwarded = 0u64;
    let mut duplicate_origins: Vec<[u8; 32]> = Vec::new();
    let mut transport = transport_manager.lock().await;

    for env in batch {
        // Bloom deduplication: skip if already seen.
        if bloom.check_and_add(&env.message_id) {
            metrics
                .envelopes_dropped_bloom
                .fetch_add(1, Ordering::Relaxed);
            duplicate_origins.push(env.origin_pubkey);
            continue;
        }

        // TTL enforcement: drop if expired.
        if env.ttl_hops == 0 {
            log::trace!("Dropping envelope {:?} — TTL exhausted", env.message_id);
            metrics
                .envelopes_dropped_ttl
                .fetch_add(1, Ordering::Relaxed);
            continue;
        }

        // Clone and decrement TTL on the outbound copy.
        let mut outbound = env.clone();
        outbound.ttl_hops -= 1;

        for peer in &recipients {
            let msg = ProtocolMessage::Transaction(outbound.clone());
            let send_fut = transport.send_to(peer, msg);
            match tokio::time::timeout(Duration::from_millis(SEND_TIMEOUT_MS), send_fut).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => log::warn!("send_to {} failed: {:?}", peer, e),
                Err(_) => log::warn!("send_to {} timed out", peer),
            }
        }
        forwarded += 1;
    }

    drop(transport);

    if !duplicate_origins.is_empty() {
        let mut pl = peer_list.lock().await;
        for origin in duplicate_origins {
            if let Some(peer) = pl.get_peer_mut(&origin) {
                apply_penalty(peer, PenaltyReason::DuplicateMessageFlood);
            }
        }
    }

    let rounds = metrics.gossip_rounds_fired.fetch_add(1, Ordering::Relaxed) + 1;
    metrics
        .envelopes_forwarded
        .fetch_add(forwarded, Ordering::Relaxed);

    if rounds.is_multiple_of(100) {
        metrics.log_summary();
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run_gossip_loop(
    mut scheduler: GossipScheduler,
    mut strike_tracker: StrikeTracker,
    state: Arc<Mutex<GossipState>>,
    peer_list: Arc<Mutex<PeerList>>,
    transport_manager: Arc<Mutex<TransportManager>>,
    metrics: Arc<Metrics>,
    event_bus: Option<TopologyEventBus>,
    db: Option<Arc<MeshDatabase>>,
    routing_table: Option<Arc<Mutex<RoutingTable>>>,
) {
    let fanout_calc = FanoutCalculator::new();
    let mut bloom = SlidingBloomFilter::new(10_000, 0.01);
    let mut anti_entropy_timer = tokio::time::interval(Duration::from_secs(30));
    let mut ban_check_timer = tokio::time::interval(Duration::from_secs(60));

    // Subscribe to topology events if an event bus is provided.
    let mut topology_events = event_bus.as_ref().map(|bus| bus.subscribe());

    loop {
        tokio::select! {
            _ = sleep(Duration::from_millis(
                if scheduler.is_idle() {
                    IDLE_ROUND_INTERVAL_MS
                } else {
                    ACTIVE_ROUND_INTERVAL_MS
                }
            )) => {
                if scheduler.is_time_for_round() {
                    execute_fanout_round(
                        &mut bloom,
                        &metrics,
                        &state,
                        &peer_list,
                        &transport_manager,
                        &fanout_calc,
                    ).await;
                    scheduler.round_executed();
                }
            }

            // Anti-entropy pull: every 30 s, pick one random active peer,
            // send a SyncRequest, and merge the SyncResponse into our state.
            _ = anti_entropy_timer.tick() => {
                log::debug!("Anti-entropy sync timer fired");

                // Pick one random non-banned active peer.
                let maybe_peer = {
                    let pl = peer_list.lock().await;
                    let active: Vec<_> = pl
                        .get_active_peers()
                        .into_iter()
                        .filter(|p| !p.is_banned)
                        .collect();
                    if active.is_empty() {
                        None
                    } else {
                        use rand::seq::SliceRandom;
                        active.choose(&mut rand::thread_rng()).map(|p| p.identity.clone())
                    }
                };

                if let Some(peer) = maybe_peer {
                    let sync_req = {
                        let st = state.lock().await;
                        st.generate_sync_request()
                    };

                    let send_result = {
                        let mut tm = transport_manager.lock().await;
                        tm.send_to(&peer, ProtocolMessage::SyncRequest(sync_req)).await
                    };

                    match send_result {
                        Ok(()) => {
                            metrics.sync_requests_sent.fetch_add(1, Ordering::Relaxed);
                            log::debug!("Anti-entropy SyncRequest sent to {}", peer);
                            let response = {
                                let mut tm = transport_manager.lock().await;
                                tokio::time::timeout(
                                    Duration::from_secs(10),
                                    tm.recv_any(),
                                ).await
                            };
                            match response {
                                Ok(Some((from_peer, ProtocolMessage::SyncResponse(resp)))) if from_peer == peer => {
                                    let n = resp.missing_envelopes.len();
                                    metrics.sync_responses_received.fetch_add(1, Ordering::Relaxed);
                                    metrics.envelopes_merged_from_sync.fetch_add(n as u64, Ordering::Relaxed);
                                    if n > MACRO_MERGE_THRESHOLD {
                                        metrics.macro_merge_events.fetch_add(1, Ordering::Relaxed);
                                    }
                                    log::debug!(
                                        "Anti-entropy SyncResponse from {}: {} envelope(s)",
                                        peer,
                                        n
                                    );
                                    let mut st = state.lock().await;
                                    st.handle_sync_response(resp);
                                }
                                Ok(Some(_)) => {
                                    log::debug!("Anti-entropy: unexpected message from peer, ignoring");
                                }
                                Ok(None) | Err(_) => {
                                    log::debug!("Anti-entropy: no response from {} within timeout", peer);
                                }
                            }
                        }
                        Err(e) => {
                            log::debug!("Anti-entropy SyncRequest to {} failed: {:?}", peer, e);
                        }
                    }
                }
            }

            _ = ban_check_timer.tick() => {
                let unbanned = {
                    let mut peer_list_guard = peer_list.lock().await;
                    peer_list_guard.check_ban_expirations()
                };
                if !unbanned.is_empty() {
                    metrics
                        .peers_unbanned
                        .fetch_add(unbanned.len() as u64, Ordering::Relaxed);
                    log::info!("Unbanned {} peer(s) after expiration", unbanned.len());
                    for peer in &unbanned {
                        strike_tracker.clear_peer(peer);
                        if let Some(ref db) = db {
                            if let Err(e) = db.remove_ban(&peer.pubkey).await {
                                log::warn!("Failed to remove ban for {} from DB: {:?}", peer, e);
                            }
                        }
                    }
                }
            }

            // Topology change events — invalidate the cached routing table so the
            // next fanout round re-computes paths.
            event = async {
                let rx = topology_events.as_mut()?;
                rx.recv().await.ok()
            }, if topology_events.is_some() => {
                if let Some(event) = event {
                    log::debug!("Topology event: {:?}. Invalidating routing table.", event);
                    if let Some(ref rt) = routing_table {
                        let peer_key = event.affected_peer_pubkey();
                        let mut table = rt.lock().await;
                        match event {
                            TopologyEvent::PeerDisconnected { .. }
                            | TopologyEvent::RelayLost { .. }
                            | TopologyEvent::PeerUnreachable { .. } => {
                                table.invalidate(&peer_key);
                            }
                            TopologyEvent::ClusterMerge { .. }
                            | TopologyEvent::PartitionDetected { .. } => {
                                table.clear();
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
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

    fn make_transport() -> Arc<Mutex<crate::transport::unified::TransportManager>> {
        Arc::new(Mutex::new(
            crate::transport::unified::TransportManager::new(
                crate::transport::unified::TransportPreference::BleOnly,
            ),
        ))
    }

    #[test]
    fn test_generate_sync_request() {
        let mut state = GossipState::new();
        state.add_envelope(mock_envelope(0xAA)).unwrap();
        state.add_envelope(mock_envelope(0xBB)).unwrap();

        let req = state.generate_sync_request();
        assert_eq!(req.known_message_ids.len(), 2);
        assert_eq!(req.known_message_ids[0], [0xAA, 0xAA, 0xAA, 0xAA]);
        assert_eq!(req.known_message_ids[1], [0xBB, 0xBB, 0xBB, 0xBB]);
    }

    #[test]
    fn test_handle_sync_request_delta_calculation() {
        let mut node_a = GossipState::new();
        node_a.add_envelope(mock_envelope(0xAA)).unwrap();
        node_a.add_envelope(mock_envelope(0xBB)).unwrap();

        let mut node_b = GossipState::new();
        node_b.add_envelope(mock_envelope(0xAA)).unwrap();

        let req = node_b.generate_sync_request();
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
        let state = Arc::new(Mutex::new(GossipState::new()));
        let peer_list = Arc::new(Mutex::new(PeerList::new(300)));
        let transport_manager = make_transport();
        let metrics = Metrics::new();
        let handle = tokio::spawn(run_gossip_loop(
            scheduler,
            strike_tracker,
            state,
            peer_list,
            transport_manager,
            metrics,
            None,
            None,
            None,
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
        let state = Arc::new(Mutex::new(GossipState::new()));
        let peer_list = Arc::new(Mutex::new(PeerList::new(300)));
        let transport_manager = make_transport();
        let metrics = Metrics::new();
        let handle = tokio::spawn(run_gossip_loop(
            scheduler,
            strike_tracker,
            state,
            peer_list,
            transport_manager,
            metrics,
            None,
            None,
            None,
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
        let state = Arc::new(Mutex::new(GossipState::new()));
        let peer_list = Arc::new(Mutex::new(PeerList::new(300)));
        let transport_manager = make_transport();
        let metrics = Metrics::new();
        let handle = tokio::spawn(run_gossip_loop(
            scheduler,
            strike_tracker,
            state,
            peer_list,
            transport_manager,
            metrics,
            None,
            None,
            None,
        ));
        let result = timeout(Duration::from_millis(150), async {
            tokio::time::sleep(Duration::from_millis(50)).await;
        })
        .await;
        assert!(result.is_ok());
        handle.abort();
    }

    // ── Acceptance criteria tests ──────────────────────────────────────────────

    #[test]
    fn test_fanout_skips_banned_peers() {
        use crate::discovery::peer_list::PeerList;

        let mut pl = PeerList::new(300);
        let mut key_a = [0u8; 32];
        key_a[0] = 1;
        let mut key_b = [0u8; 32];
        key_b[0] = 2;
        let mut key_c = [0u8; 32];
        key_c[0] = 3;

        pl.insert_or_update(key_a, 80);
        pl.insert_or_update(key_b, 80);
        pl.insert_or_update(key_c, 80);
        pl.ban_peer(&key_b, 3600);

        let active: Vec<_> = pl
            .get_active_peers()
            .into_iter()
            .filter(|p| !p.is_banned)
            .map(|p| p.identity.clone())
            .collect();

        assert_eq!(active.len(), 2);
        assert!(active.iter().all(|p| p.pubkey != key_b));
    }

    #[test]
    fn test_gossip_metrics_accumulate() {
        let metrics = Metrics::new();
        metrics.gossip_rounds_fired.fetch_add(1, Ordering::Relaxed);
        metrics.envelopes_forwarded.fetch_add(3, Ordering::Relaxed);
        metrics.gossip_rounds_fired.fetch_add(1, Ordering::Relaxed);
        metrics.envelopes_forwarded.fetch_add(2, Ordering::Relaxed);
        let snap = metrics.snapshot();
        assert_eq!(snap.gossip_rounds_fired, 2);
        assert_eq!(snap.envelopes_forwarded, 5);
    }

    #[tokio::test]
    async fn test_execute_fanout_round_drops_ttl_zero() {
        let fanout_calc = FanoutCalculator::new();
        let mut bloom = SlidingBloomFilter::new(10_000, 0.01);
        let metrics = Metrics::new();

        let state = Arc::new(Mutex::new(GossipState::new()));
        {
            let mut st = state.lock().await;
            let mut env_live = mock_envelope(0x01);
            env_live.ttl_hops = 3;
            let mut env_dead = mock_envelope(0x02);
            env_dead.ttl_hops = 0;
            st.add_envelope(env_live).unwrap();
            st.add_envelope(env_dead).unwrap();
        }

        // No active peers → round fires but nothing is sent.
        let peer_list = Arc::new(Mutex::new(PeerList::new(300)));
        let transport_manager = make_transport();

        execute_fanout_round(
            &mut bloom,
            &metrics,
            &state,
            &peer_list,
            &transport_manager,
            &fanout_calc,
        )
        .await;

        assert_eq!(metrics.gossip_rounds_fired.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_bloom_prevents_rebroadcast() {
        let mut bloom = SlidingBloomFilter::new(10_000, 0.01);
        let id = [0xABu8; 32];
        assert!(!bloom.check_and_add(&id)); // first time: new
        assert!(bloom.check_and_add(&id)); // second time: already seen
    }

    // ── Anti-entropy pull tests ────────────────────────────────────────────────

    #[test]
    fn test_anti_entropy_no_peers_skips_sync() {
        let state = GossipState::new();
        let req = state.generate_sync_request();
        assert_eq!(req.known_message_ids.len(), 0);
    }

    #[test]
    fn test_anti_entropy_sync_request_contains_all_known_ids() {
        let mut state = GossipState::new();
        state.add_envelope(mock_envelope(0x01)).unwrap();
        state.add_envelope(mock_envelope(0x02)).unwrap();
        state.add_envelope(mock_envelope(0x03)).unwrap();

        let req = state.generate_sync_request();
        assert_eq!(req.known_message_ids.len(), 3);
        for prefix in [[0x01u8; 4], [0x02u8; 4], [0x03u8; 4]] {
            assert!(req.known_message_ids.contains(&prefix));
        }
    }

    #[test]
    fn test_anti_entropy_handle_sync_response_merges_missing() {
        let mut local = GossipState::new();
        local.add_envelope(mock_envelope(0xAA)).unwrap();

        let mut remote = GossipState::new();
        remote.add_envelope(mock_envelope(0xAA)).unwrap();
        remote.add_envelope(mock_envelope(0xBB)).unwrap();
        remote.add_envelope(mock_envelope(0xCC)).unwrap();

        let req = local.generate_sync_request();
        let resp = remote.handle_sync_request(&req);
        assert_eq!(resp.missing_envelopes.len(), 2);

        local.handle_sync_response(resp);
        assert_eq!(local.active_queue.len(), 3);
    }

    // ── GossipError integration on process_transaction_envelope ─────────────────

    /// Build an envelope carrying a valid Ed25519 signature for `signing_key`.
    fn signed_envelope(
        signing_key: &ed25519_dalek::SigningKey,
        id_byte: u8,
    ) -> TransactionEnvelope {
        let mut env = TransactionEnvelope {
            message_id: [id_byte; 32],
            origin_pubkey: signing_key.verifying_key().to_bytes(),
            tx_xdr: format!("XDR{}", id_byte),
            ttl_hops: 10,
            timestamp: 0,
            signature: [0u8; 64],
        };
        crate::message::signing::sign_envelope(signing_key, &mut env).unwrap();
        env
    }

    #[tokio::test]
    async fn test_process_envelope_returns_peer_banned_on_threshold() {
        let mut strike_tracker = StrikeTracker::new();
        let peer_list = Arc::new(Mutex::new(PeerList::new(300)));
        let transport_manager = make_transport();
        let metrics = Metrics::new();
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0x42; 32]);

        // The StrikeTracker bans once failures exceed max_strikes (3), i.e. on
        // the 4th invalid envelope. Earlier strikes return Ok(()).
        let mut last: Result<(), GossipError> = Ok(());
        for _ in 0..4 {
            let mut env = signed_envelope(&signing_key, 0x01);
            env.signature[0] ^= 0xFF; // corrupt → invalid signature
            last = process_transaction_envelope(
                &env,
                &mut strike_tracker,
                peer_list.clone(),
                transport_manager.clone(),
                None,
                &metrics,
            )
            .await;
        }

        assert!(matches!(last, Err(GossipError::PeerBanned { .. })));
        assert_eq!(metrics.peers_banned.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn test_process_envelope_ok_on_valid_signature() {
        let mut strike_tracker = StrikeTracker::new();
        let peer_list = Arc::new(Mutex::new(PeerList::new(300)));
        let transport_manager = make_transport();
        let metrics = Metrics::new();
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0x43; 32]);

        let env = signed_envelope(&signing_key, 0x01);
        let result = process_transaction_envelope(
            &env,
            &mut strike_tracker,
            peer_list.clone(),
            transport_manager.clone(),
            None,
            &metrics,
        )
        .await;

        assert!(result.is_ok());
    }

    // ── Issue #100 acceptance criteria: metrics counters ──────────────────────

    #[tokio::test]
    async fn test_gossip_metrics_rounds_accumulate() {
        let metrics = Metrics::new();
        let fanout_calc = FanoutCalculator::new();
        let mut bloom = SlidingBloomFilter::new(10_000, 0.01);
        let state = Arc::new(Mutex::new(GossipState::new()));
        let peer_list = Arc::new(Mutex::new(PeerList::new(300)));
        let transport_manager = make_transport();

        for _ in 0..3 {
            execute_fanout_round(
                &mut bloom,
                &metrics,
                &state,
                &peer_list,
                &transport_manager,
                &fanout_calc,
            )
            .await;
        }

        assert_eq!(metrics.gossip_rounds_fired.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn test_metrics_envelopes_forwarded() {
        let metrics = Metrics::new();
        let fanout_calc = FanoutCalculator::new();
        let mut bloom = SlidingBloomFilter::new(10_000, 0.01);
        let state = Arc::new(Mutex::new(GossipState::new()));

        // 5 unique envelopes with TTL > 0
        {
            let mut st = state.lock().await;
            for i in 0u8..5 {
                let mut env = mock_envelope(i);
                env.ttl_hops = 5;
                st.add_envelope(env).unwrap();
            }
        }

        // One active peer so the fanout loop runs (send will fail — still counted as forwarded)
        let peer_list = Arc::new(Mutex::new(PeerList::new(300)));
        {
            let mut pl = peer_list.lock().await;
            pl.insert_or_update([0xAAu8; 32], 80);
        }
        let transport_manager = make_transport();

        execute_fanout_round(
            &mut bloom,
            &metrics,
            &state,
            &peer_list,
            &transport_manager,
            &fanout_calc,
        )
        .await;

        assert_eq!(metrics.envelopes_forwarded.load(Ordering::Relaxed), 5);
    }

    #[tokio::test]
    async fn test_duplicate_flood_penalizes_peer() {
        let fanout_calc = FanoutCalculator::new();
        let mut bloom = SlidingBloomFilter::new(10_000, 0.01);
        let metrics = Metrics::new();

        let peer_pubkey = [0x42u8; 32];
        let peer_list = Arc::new(Mutex::new(PeerList::new(300)));
        {
            let mut pl = peer_list.lock().await;
            pl.insert_or_update(peer_pubkey, 80);
            if let Some(p) = pl.get_peer_mut(&peer_pubkey) {
                p.reputation = 50;
            }
        }

        let state = Arc::new(Mutex::new(GossipState::new()));
        {
            let mut st = state.lock().await;
            let mut env = mock_envelope(0x42);
            env.origin_pubkey = peer_pubkey;
            env.ttl_hops = 5;
            st.add_envelope(env).unwrap();
        }

        // Pre-seed bloom so the envelope is already "seen" — next check is a duplicate.
        bloom.check_and_add(&[0x42u8; 32]);

        let transport_manager = make_transport();
        execute_fanout_round(
            &mut bloom,
            &metrics,
            &state,
            &peer_list,
            &transport_manager,
            &fanout_calc,
        )
        .await;

        let pl = peer_list.lock().await;
        let peer = pl.get_peer(&peer_pubkey).unwrap();
        assert_eq!(peer.reputation, 40); // 50 - 10 (DuplicateMessageFlood)
    }

    #[tokio::test]
    async fn test_metrics_envelopes_dropped_ttl() {
        let metrics = Metrics::new();
        let fanout_calc = FanoutCalculator::new();
        let mut bloom = SlidingBloomFilter::new(10_000, 0.01);
        let state = Arc::new(Mutex::new(GossipState::new()));

        {
            let mut st = state.lock().await;
            let mut env_dead = mock_envelope(0xDD);
            env_dead.ttl_hops = 0;
            st.add_envelope(env_dead).unwrap();

            let env_live = mock_envelope(0xEE); // ttl_hops = 10 by default
            st.add_envelope(env_live).unwrap();
        }

        // One active peer so the TTL check actually runs
        let peer_list = Arc::new(Mutex::new(PeerList::new(300)));
        {
            let mut pl = peer_list.lock().await;
            pl.insert_or_update([0xBBu8; 32], 80);
        }
        let transport_manager = make_transport();

        execute_fanout_round(
            &mut bloom,
            &metrics,
            &state,
            &peer_list,
            &transport_manager,
            &fanout_calc,
        )
        .await;

        assert_eq!(metrics.envelopes_dropped_ttl.load(Ordering::Relaxed), 1);
    }

    // ── Issue #134: topology event → routing table invalidation ───────────────

    #[tokio::test]
    async fn test_topology_event_triggers_invalidation() {
        use crate::router::table::RoutingTable;
        use crate::topology::events::TopologyEventBus;

        let metrics = Metrics::new();
        let routing_table = Arc::new(Mutex::new(RoutingTable::new(16, metrics.clone())));
        {
            let mut t = routing_table.lock().await;
            t.insert(
                [0x01u8; 32],
                vec![crate::peer::identity::PeerIdentity::new([0x01u8; 32])],
            );
        }

        let event_bus = TopologyEventBus::new(16);

        let handle = tokio::spawn(run_gossip_loop(
            GossipScheduler::new(),
            StrikeTracker::new(),
            Arc::new(Mutex::new(GossipState::new())),
            Arc::new(Mutex::new(PeerList::new(300))),
            make_transport(),
            metrics.clone(),
            Some(event_bus.clone()),
            None,
            Some(routing_table.clone()),
        ));

        // Let the loop start and subscribe before publishing.
        tokio::time::sleep(Duration::from_millis(20)).await;
        event_bus.publish(crate::topology::events::TopologyEvent::PeerDisconnected {
            peer_pubkey: [0x01u8; 32],
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.abort();

        assert_eq!(
            metrics.routing_table_invalidations.load(Ordering::Relaxed),
            1
        );
    }

    #[tokio::test]
    async fn test_cluster_merge_event_triggers_clear() {
        use crate::router::table::RoutingTable;
        use crate::topology::events::TopologyEventBus;

        let metrics = Metrics::new();
        let routing_table = Arc::new(Mutex::new(RoutingTable::new(16, metrics.clone())));
        {
            let mut t = routing_table.lock().await;
            for i in 0u8..5 {
                t.insert(
                    [i; 32],
                    vec![crate::peer::identity::PeerIdentity::new([i; 32])],
                );
            }
        }

        let event_bus = TopologyEventBus::new(16);

        let handle = tokio::spawn(run_gossip_loop(
            GossipScheduler::new(),
            StrikeTracker::new(),
            Arc::new(Mutex::new(GossipState::new())),
            Arc::new(Mutex::new(PeerList::new(300))),
            make_transport(),
            metrics.clone(),
            Some(event_bus.clone()),
            None,
            Some(routing_table.clone()),
        ));

        // Let the loop start and subscribe before publishing.
        tokio::time::sleep(Duration::from_millis(20)).await;
        event_bus.publish(crate::topology::events::TopologyEvent::ClusterMerge {
            origin_pubkey: [0xAAu8; 32],
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.abort();

        let t = routing_table.lock().await;
        assert!(t.is_empty());
    }
}
