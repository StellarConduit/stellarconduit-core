use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::message::types::TopologyUpdate;

pub struct MeshGraph {
    edges: HashMap<[u8; 32], Vec<[u8; 32]>>,
    /// Tracks when each node's edge list was last updated (for stale pruning).
    last_updated: HashMap<[u8; 32], Instant>,
}

impl MeshGraph {
    pub fn new() -> Self {
        Self {
            edges: HashMap::new(),
            last_updated: HashMap::new(),
        }
    }

    pub fn apply_update(&mut self, update: &TopologyUpdate) {
        let origin = update.origin_pubkey;
        let mut set: HashSet<[u8; 32]> = HashSet::new();
        for peer in update.directly_connected_peers.iter() {
            if *peer != origin {
                set.insert(*peer);
            }
        }
        let mut list: Vec<[u8; 32]> = set.into_iter().collect();
        list.sort_unstable();
        self.edges.insert(origin, list);
        self.last_updated.insert(origin, Instant::now());
    }

    pub fn get_neighbors(&self, target: &[u8; 32]) -> Option<&Vec<[u8; 32]>> {
        self.edges.get(target)
    }

    /// Total number of nodes tracked in the graph.
    pub fn node_count(&self) -> usize {
        self.edges.len()
    }

    /// Remove all edges whose last update is older than `timeout`.
    /// Returns the number of nodes pruned.
    pub fn prune_stale_edges(&mut self, timeout: Duration) -> usize {
        let now = Instant::now();
        let stale_keys: Vec<[u8; 32]> = self
            .last_updated
            .iter()
            .filter(|(_, ts)| now.duration_since(**ts) > timeout)
            .map(|(k, _)| *k)
            .collect();

        let pruned = stale_keys.len();
        for key in stale_keys {
            self.edges.remove(&key);
            self.last_updated.remove(&key);
        }
        pruned
    }

    /// Back-date a node's last_updated timestamp by `age` (test helper).
    #[cfg(test)]
    pub fn backdate_edge(&mut self, pubkey: &[u8; 32], age: Duration) {
        if let Some(ts) = self.last_updated.get_mut(pubkey) {
            *ts = Instant::now() - age;
        }
    }
}

impl Default for MeshGraph {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(b: u8) -> [u8; 32] {
        [b; 32]
    }

    #[test]
    fn apply_update_filters_self_edges() {
        let origin = pk(1);
        let mut g = MeshGraph::new();
        let update = TopologyUpdate {
            origin_pubkey: origin,
            directly_connected_peers: vec![pk(2), origin, pk(2)],
            hops_to_relay: 5,
            topology_flags: vec![],
        };
        g.apply_update(&update);
        let neighbors = g.get_neighbors(&origin).cloned().unwrap();
        assert!(neighbors.iter().all(|p| *p != origin));
        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0], pk(2));
    }

    #[test]
    fn prune_stale_edges_removes_old_entries() {
        let mut g = MeshGraph::new();
        let update = TopologyUpdate {
            origin_pubkey: pk(1),
            directly_connected_peers: vec![pk(2)],
            hops_to_relay: 1,
            topology_flags: vec![],
        };
        g.apply_update(&update);
        g.backdate_edge(&pk(1), Duration::from_secs(3700));
        let pruned = g.prune_stale_edges(Duration::from_secs(3600));
        assert_eq!(pruned, 1);
        assert_eq!(g.node_count(), 0);
    }

    #[test]
    fn prune_stale_edges_keeps_fresh_entries() {
        let mut g = MeshGraph::new();
        let update = TopologyUpdate {
            origin_pubkey: pk(3),
            directly_connected_peers: vec![pk(4)],
            hops_to_relay: 1,
            topology_flags: vec![],
        };
        g.apply_update(&update);
        // Edge is fresh — should NOT be pruned
        let pruned = g.prune_stale_edges(Duration::from_secs(3600));
        assert_eq!(pruned, 0);
        assert_eq!(g.node_count(), 1);
    }
}
