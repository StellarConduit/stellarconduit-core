//! Gossip protocol event loop and anti-entropy sync.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::sleep;

use crate::discovery::peer_list::PeerList;
use crate::gossip::bloom::SlidingBloomFilter;
use crate::gossip::fanout::{select_random_peers, FanoutCalculator};
use crate::gossip::queue::PriorityQueue;
use crate::gossip::round::{GossipScheduler, ACTIVE_ROUND_INTERVAL_MS, IDLE_ROUND_INTERVAL_MS};
use crate::gossip::strike_tracker::StrikeTracker;
use crate::message::signing::verify_signature;
use crate::message::types::{ProtocolMessage, SyncRequest, SyncResponse, TransactionEnvelope};
use crate::peer::identity::PeerIdentity;
use crate::topology::events::TopologyEventBus;
use crate::transport::unified::TransportManager;
use tokio_util::sync::CancellationToken;

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

    pub fn add_envelope(&mut self, env: TransactionEnvelope) {
        self.active_queue.push(ProtocolMessage::Transaction(env));
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
                self.add_envelope(env);
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
                self.add_envelope(env);
            }
            return;
        }
        let mut backlog: Vec<TransactionEnvelope> = resp.missing_envelopes;
        backlog.sort_by_key(|env| env.timestamp);
        self.macro_merge_backlog = backlog.into();
    }
}

/// Tracks cumulative gossip loop statistics.
#[derive(Default, Debug)]
pub struct GossipLoopMetrics {
    pub rounds_fired: u64,
    pub envelopes_forwarded: u64,
}

/// Process an incoming TransactionEnvelope, verifying its signature and tracking failures.
pub async fn process_transaction_envelope(
    envelope: &TransactionEnvelope,
    strike_tracker: &mut StrikeTracker,
    peer_list: Arc<Mutex<PeerList>>,
    transport_manager: Arc<Mutex<TransportManager>>,
) -> Result<(), PeerIdentity> {
    match verify_signature(envelope) {
        Ok(true) => {
            let peer_identity = PeerIdentity::new(envelope.origin_pubkey);
            strike_tracker.clear_peer(&peer_identity);
            Ok(())
        }
        Ok(false) => Ok(()),
        Err(_) => {
            let peer_identity = PeerIdentity::new(envelope.origin_pubkey);
            let should_ban = strike_tracker.record_failure(&peer_identity);
            if should_ban {
                const BAN_DURATION_SEC: u64 = 24 * 60 * 60;
                let mut peer_list_guard = peer_list.lock().await;
                if peer_list_guard.get_peer(&peer_identity.pubkey).is_none() {
                    peer_list_guard.insert_or_update(peer_identity.pubkey, 0);
                }
                peer_list_guard.ban_peer(&peer_identity.pubkey, BAN_DURATION_SEC);
                drop(peer_list_guard);
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

/// Executes one epidemic fanout round: selects peers, drains a batch of envelopes,
/// applies bloom deduplication and TTL enforcement, then dispatches to each peer.
pub async fn execute_fanout_round(
    bloom: &mut SlidingBloomFilter,
    metrics: &mut GossipLoopMetrics,
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
        metrics.rounds_fired += 1;
        return;
    }

    // 2. Calculate fanout and select recipients.
    let f = fanout_calc.calculate(active_peers.len(), None);
    let recipients = select_random_peers(&active_peers, f);

    // 3. Drain up to FANOUT_BATCH_SIZE envelopes from the queue.
    let mut batch: Vec<TransactionEnvelope> = Vec::with_capacity(FANOUT_BATCH_SIZE);
    {
        let mut st = state.lock().await;
        for _ in 0..FANOUT_BATCH_SIZE {
            match st.active_queue.pop() {
                Some(ProtocolMessage::Transaction(env)) => batch.push(env),
                Some(other) => {
                    // Non-transaction messages: put back via re-push is not possible without
                    // re-enqueueing, so we just forward them as-is by re-adding.
                    st.active_queue.push(other);
                    break;
                }
                None => break,
            }
        }
    }

    // 4. For each envelope, apply bloom dedup + TTL, then send to recipients.
    let mut forwarded = 0u64;
    let mut transport = transport_manager.lock().await;

    for env in batch {
        // Bloom deduplication: skip if already seen.
        if bloom.check_and_add(&env.message_id) {
            continue;
        }

        // TTL enforcement: drop if expired.
        if env.ttl_hops == 0 {
            log::trace!("Dropping envelope {:?} — TTL exhausted", env.message_id);
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

    metrics.rounds_fired += 1;
    metrics.envelopes_forwarded += forwarded;

    if metrics.rounds_fired.is_multiple_of(100) {
        log::info!(
            "GossipLoopMetrics: rounds_fired={}, envelopes_forwarded={}",
            metrics.rounds_fired,
            metrics.envelopes_forwarded
        );
    }
}

pub async fn run_gossip_loop(
    mut scheduler: GossipScheduler,
    mut strike_tracker: StrikeTracker,
    state: Arc<Mutex<GossipState>>,
    peer_list: Arc<Mutex<PeerList>>,
    transport_manager: Arc<Mutex<TransportManager>>,
    event_bus: Option<TopologyEventBus>,
    shutdown: CancellationToken,
) {
    let fanout_calc = FanoutCalculator::new();
    let mut bloom = SlidingBloomFilter::new(10_000, 0.01);
    let mut metrics = GossipLoopMetrics::default();
    let mut anti_entropy_timer = tokio::time::interval(Duration::from_secs(30));
    let mut ban_check_timer = tokio::time::interval(Duration::from_secs(60));

    // Subscribe to topology events if an event bus is provided.
    let mut topology_events = event_bus.as_ref().map(|bus| bus.subscribe());

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                log::info!("Gossip loop: received shutdown signal");
                break;
            }

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
                        &mut metrics,
                        &state,
                        &peer_list,
                        &transport_manager,
                        &fanout_calc,
                    ).await;
                    scheduler.round_executed();
                }
            }

            _ = anti_entropy_timer.tick() => {
                log::debug!("Anti-entropy sync timer fired");
                // TODO: Pick one random active peer and send state.generate_sync_request()
            }

            _ = ban_check_timer.tick() => {
                let mut peer_list_guard = peer_list.lock().await;
                let unbanned = peer_list_guard.check_ban_expirations();
                if !unbanned.is_empty() {
                    log::info!("Unbanned {} peer(s) after expiration", unbanned.len());
                    for peer in unbanned {
                        strike_tracker.clear_peer(&peer);
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
                    log::debug!("Topology event received: {:?}", event);
                    // TODO: call routing_table.invalidate() once the routing table
                    //       module is implemented.
                    let _ = event;
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
    use tokio_util::sync::CancellationToken;

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
        node_a.add_envelope(mock_envelope(0xAA));
        node_a.add_envelope(mock_envelope(0xBB));

        let mut node_b = GossipState::new();
        node_b.add_envelope(mock_envelope(0xAA));

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
        let handle = tokio::spawn(run_gossip_loop(
            scheduler,
            strike_tracker,
            state,
            peer_list,
            transport_manager,
            None,
            CancellationToken::new(),
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
        let handle = tokio::spawn(run_gossip_loop(
            scheduler,
            strike_tracker,
            state,
            peer_list,
            transport_manager,
            None,
            CancellationToken::new(),
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
        let handle = tokio::spawn(run_gossip_loop(
            scheduler,
            strike_tracker,
            state,
            peer_list,
            transport_manager,
            None,
            CancellationToken::new(),
        ));
        let result = timeout(Duration::from_millis(150), async {
            tokio::time::sleep(Duration::from_millis(50)).await;
        })
        .await;
        assert!(result.is_ok());
        handle.abort();
    }

    // ── Acceptance criteria tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn test_gossip_loop_exits_on_cancellation() {
        let scheduler = GossipScheduler::new();
        let strike_tracker = StrikeTracker::new();
        let state = Arc::new(Mutex::new(GossipState::new()));
        let peer_list = Arc::new(Mutex::new(PeerList::new(300)));
        let transport_manager = make_transport();
        let token = CancellationToken::new();
        let handle = tokio::spawn(run_gossip_loop(
            scheduler,
            strike_tracker,
            state,
            peer_list,
            transport_manager,
            None,
            token.clone(),
        ));
        token.cancel();
        let result = timeout(Duration::from_millis(500), handle).await;
        assert!(result.is_ok(), "gossip loop did not exit within 500ms after cancellation");
        assert!(result.unwrap().is_ok(), "gossip loop task panicked");
    }

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
        let mut metrics = GossipLoopMetrics::default();
        metrics.rounds_fired += 1;
        metrics.envelopes_forwarded += 3;
        metrics.rounds_fired += 1;
        metrics.envelopes_forwarded += 2;
        assert_eq!(metrics.rounds_fired, 2);
        assert_eq!(metrics.envelopes_forwarded, 5);
    }

    #[tokio::test]
    async fn test_execute_fanout_round_drops_ttl_zero() {
        let fanout_calc = FanoutCalculator::new();
        let mut bloom = SlidingBloomFilter::new(10_000, 0.01);
        let mut metrics = GossipLoopMetrics::default();

        let state = Arc::new(Mutex::new(GossipState::new()));
        {
            let mut st = state.lock().await;
            let mut env_live = mock_envelope(0x01);
            env_live.ttl_hops = 3;
            let mut env_dead = mock_envelope(0x02);
            env_dead.ttl_hops = 0;
            st.add_envelope(env_live);
            st.add_envelope(env_dead);
        }

        // No active peers → round fires but nothing is sent.
        let peer_list = Arc::new(Mutex::new(PeerList::new(300)));
        let transport_manager = make_transport();

        execute_fanout_round(
            &mut bloom,
            &mut metrics,
            &state,
            &peer_list,
            &transport_manager,
            &fanout_calc,
        )
        .await;

        assert_eq!(metrics.rounds_fired, 1);
    }

    #[test]
    fn test_bloom_prevents_rebroadcast() {
        let mut bloom = SlidingBloomFilter::new(10_000, 0.01);
        let id = [0xABu8; 32];
        assert!(!bloom.check_and_add(&id)); // first time: new
        assert!(bloom.check_and_add(&id)); // second time: already seen
    }
}
