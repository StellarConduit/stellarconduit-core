//! Routing table with LRU cache and observability counters.

use std::num::NonZeroUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use lru::LruCache;

use crate::metrics::Metrics;
use crate::peer::identity::PeerIdentity;

/// LRU-backed routing table that tracks cache hits, misses, and refresh events.
pub struct RoutingTable {
    cache: LruCache<[u8; 32], Vec<PeerIdentity>>,
    metrics: Arc<Metrics>,
}

impl RoutingTable {
    pub fn new(capacity: usize, metrics: Arc<Metrics>) -> Self {
        let cap = NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::new(128).unwrap());
        Self {
            cache: LruCache::new(cap),
            metrics,
        }
    }

    /// Look up next hops for `dest`. Increments `routing_table_hits` or `routing_table_misses`.
    pub fn lookup(&mut self, dest: &[u8; 32]) -> Option<Vec<PeerIdentity>> {
        if let Some(hops) = self.cache.get(dest) {
            self.metrics
                .routing_table_hits
                .fetch_add(1, Ordering::Relaxed);
            Some(hops.clone())
        } else {
            self.metrics
                .routing_table_misses
                .fetch_add(1, Ordering::Relaxed);
            None
        }
    }

    /// Insert or overwrite next hops for `dest`.
    pub fn insert(&mut self, dest: [u8; 32], next_hops: Vec<PeerIdentity>) {
        self.cache.put(dest, next_hops);
    }

    /// Bulk-refresh the table from a new set of entries.
    /// Increments `routing_table_refreshes` once per call.
    pub fn refresh(&mut self, entries: Vec<([u8; 32], Vec<PeerIdentity>)>) {
        for (dest, hops) in entries {
            self.cache.put(dest, hops);
        }
        self.metrics
            .routing_table_refreshes
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn len(&self) -> usize {
        self.cache.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn peer(b: u8) -> PeerIdentity {
        PeerIdentity::new([b; 32])
    }

    fn dest(b: u8) -> [u8; 32] {
        [b; 32]
    }

    #[test]
    fn test_routing_table_miss_on_empty() {
        let metrics = Metrics::new();
        let mut table = RoutingTable::new(16, metrics.clone());
        assert!(table.lookup(&dest(0xAA)).is_none());
        assert_eq!(metrics.routing_table_misses.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.routing_table_hits.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_routing_table_hit_after_insert() {
        let metrics = Metrics::new();
        let mut table = RoutingTable::new(16, metrics.clone());
        table.insert(dest(0xBB), vec![peer(0x01), peer(0x02)]);
        let hops = table.lookup(&dest(0xBB)).unwrap();
        assert_eq!(hops.len(), 2);
        assert_eq!(metrics.routing_table_hits.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.routing_table_misses.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_routing_table_refresh_increments_counter() {
        let metrics = Metrics::new();
        let mut table = RoutingTable::new(16, metrics.clone());
        table.refresh(vec![
            (dest(0x01), vec![peer(0xAA)]),
            (dest(0x02), vec![peer(0xBB)]),
        ]);
        assert_eq!(metrics.routing_table_refreshes.load(Ordering::Relaxed), 1);
        assert_eq!(table.len(), 2);
        table.refresh(vec![(dest(0x03), vec![peer(0xCC)])]);
        assert_eq!(metrics.routing_table_refreshes.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn test_routing_table_lru_eviction() {
        let metrics = Metrics::new();
        let mut table = RoutingTable::new(2, metrics.clone());
        table.insert(dest(0x01), vec![peer(0xAA)]);
        table.insert(dest(0x02), vec![peer(0xBB)]);
        // Access 0x01 to keep it hot
        let _ = table.lookup(&dest(0x01));
        // Insert 0x03 — should evict the LRU entry (0x02)
        table.insert(dest(0x03), vec![peer(0xCC)]);
        assert!(table.lookup(&dest(0x01)).is_some());
        assert!(table.lookup(&dest(0x02)).is_none());
    }
}
