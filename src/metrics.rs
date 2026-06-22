//! Global metrics registry for protocol-level observability.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// All protocol-level counters. Uses `AtomicU64` with `Relaxed` ordering —
/// these are approximate monitoring metrics, not consistency-critical data.
#[derive(Default)]
pub struct Metrics {
    // Gossip
    pub gossip_rounds_fired: AtomicU64,
    pub envelopes_forwarded: AtomicU64,
    pub envelopes_dropped_ttl: AtomicU64,
    pub envelopes_dropped_bloom: AtomicU64,
    pub envelopes_dropped_dampened: AtomicU64,

    // Rate Limiting
    pub messages_rate_limited: AtomicU64,
    pub peer_violations_recorded: AtomicU64,

    // Anti-Entropy
    pub sync_requests_sent: AtomicU64,
    pub sync_responses_received: AtomicU64,
    pub envelopes_merged_from_sync: AtomicU64,
    pub macro_merge_events: AtomicU64,

    // Routing
    pub routing_table_hits: AtomicU64,
    pub routing_table_misses: AtomicU64,
    pub routing_table_refreshes: AtomicU64,
    pub routing_table_invalidations: AtomicU64,

    // Peer Management
    pub peers_banned: AtomicU64,
    pub peers_unbanned: AtomicU64,
    pub peers_connected: AtomicU64,
    pub peers_disconnected: AtomicU64,

    // Relay
    pub transactions_submitted: AtomicU64,
    pub transactions_rejected: AtomicU64,
    pub relay_submission_retries: AtomicU64,
}

/// Point-in-time snapshot of all counters. Useful for before/after assertions in tests.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MetricsSnapshot {
    // Gossip
    pub gossip_rounds_fired: u64,
    pub envelopes_forwarded: u64,
    pub envelopes_dropped_ttl: u64,
    pub envelopes_dropped_bloom: u64,
    pub envelopes_dropped_dampened: u64,

    // Rate Limiting
    pub messages_rate_limited: u64,
    pub peer_violations_recorded: u64,

    // Anti-Entropy
    pub sync_requests_sent: u64,
    pub sync_responses_received: u64,
    pub envelopes_merged_from_sync: u64,
    pub macro_merge_events: u64,

    // Routing
    pub routing_table_hits: u64,
    pub routing_table_misses: u64,
    pub routing_table_refreshes: u64,
    pub routing_table_invalidations: u64,

    // Peer Management
    pub peers_banned: u64,
    pub peers_unbanned: u64,
    pub peers_connected: u64,
    pub peers_disconnected: u64,

    // Relay
    pub transactions_submitted: u64,
    pub transactions_rejected: u64,
    pub relay_submission_retries: u64,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Capture a consistent (non-atomic) snapshot of all counters.
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            gossip_rounds_fired: self.gossip_rounds_fired.load(Ordering::Relaxed),
            envelopes_forwarded: self.envelopes_forwarded.load(Ordering::Relaxed),
            envelopes_dropped_ttl: self.envelopes_dropped_ttl.load(Ordering::Relaxed),
            envelopes_dropped_bloom: self.envelopes_dropped_bloom.load(Ordering::Relaxed),
            envelopes_dropped_dampened: self.envelopes_dropped_dampened.load(Ordering::Relaxed),
            messages_rate_limited: self.messages_rate_limited.load(Ordering::Relaxed),
            peer_violations_recorded: self.peer_violations_recorded.load(Ordering::Relaxed),
            sync_requests_sent: self.sync_requests_sent.load(Ordering::Relaxed),
            sync_responses_received: self.sync_responses_received.load(Ordering::Relaxed),
            envelopes_merged_from_sync: self.envelopes_merged_from_sync.load(Ordering::Relaxed),
            macro_merge_events: self.macro_merge_events.load(Ordering::Relaxed),
            routing_table_hits: self.routing_table_hits.load(Ordering::Relaxed),
            routing_table_misses: self.routing_table_misses.load(Ordering::Relaxed),
            routing_table_refreshes: self.routing_table_refreshes.load(Ordering::Relaxed),
            routing_table_invalidations: self.routing_table_invalidations.load(Ordering::Relaxed),
            peers_banned: self.peers_banned.load(Ordering::Relaxed),
            peers_unbanned: self.peers_unbanned.load(Ordering::Relaxed),
            peers_connected: self.peers_connected.load(Ordering::Relaxed),
            peers_disconnected: self.peers_disconnected.load(Ordering::Relaxed),
            transactions_submitted: self.transactions_submitted.load(Ordering::Relaxed),
            transactions_rejected: self.transactions_rejected.load(Ordering::Relaxed),
            relay_submission_retries: self.relay_submission_retries.load(Ordering::Relaxed),
        }
    }

    /// Emit all counters at `info!` level. Called periodically from the gossip loop.
    pub fn log_summary(&self) {
        let s = self.snapshot();
        log::info!(
            "Metrics: rounds={} fwd={} drop_ttl={} drop_bloom={} rate_limited={} \
             sync_req={} sync_resp={} merged={} macro_merge={} \
             rt_hits={} rt_miss={} rt_refresh={} \
             banned={} unbanned={} connected={} disconnected={} \
             submitted={} rejected={} retries={}",
            s.gossip_rounds_fired,
            s.envelopes_forwarded,
            s.envelopes_dropped_ttl,
            s.envelopes_dropped_bloom,
            s.messages_rate_limited,
            s.sync_requests_sent,
            s.sync_responses_received,
            s.envelopes_merged_from_sync,
            s.macro_merge_events,
            s.routing_table_hits,
            s.routing_table_misses,
            s.routing_table_refreshes,
            s.peers_banned,
            s.peers_unbanned,
            s.peers_connected,
            s.peers_disconnected,
            s.transactions_submitted,
            s.transactions_rejected,
            s.relay_submission_retries,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_snapshot_captures_all_counters() {
        let m = Metrics::new();
        m.gossip_rounds_fired.fetch_add(5, Ordering::Relaxed);
        m.envelopes_forwarded.fetch_add(20, Ordering::Relaxed);
        m.envelopes_dropped_ttl.fetch_add(3, Ordering::Relaxed);
        m.messages_rate_limited.fetch_add(7, Ordering::Relaxed);
        m.transactions_submitted.fetch_add(2, Ordering::Relaxed);
        m.transactions_rejected.fetch_add(1, Ordering::Relaxed);

        let snap = m.snapshot();
        assert_eq!(snap.gossip_rounds_fired, 5);
        assert_eq!(snap.envelopes_forwarded, 20);
        assert_eq!(snap.envelopes_dropped_ttl, 3);
        assert_eq!(snap.messages_rate_limited, 7);
        assert_eq!(snap.transactions_submitted, 2);
        assert_eq!(snap.transactions_rejected, 1);
    }

    #[test]
    fn test_metrics_snapshot_comparison() {
        let m = Metrics::new();
        let before = m.snapshot();
        m.gossip_rounds_fired.fetch_add(1, Ordering::Relaxed);
        let after = m.snapshot();
        assert_ne!(before, after);
        assert_eq!(after.gossip_rounds_fired, before.gossip_rounds_fired + 1);
    }

    #[test]
    fn test_metrics_default_snapshot_is_zero() {
        let snap = MetricsSnapshot::default();
        assert_eq!(snap.gossip_rounds_fired, 0);
        assert_eq!(snap.envelopes_forwarded, 0);
        assert_eq!(snap.transactions_submitted, 0);
    }
}
