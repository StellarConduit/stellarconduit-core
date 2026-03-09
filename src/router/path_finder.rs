use std::collections::{HashMap, HashSet, VecDeque};

use crate::topology::graph::MeshGraph;
use crate::topology::hop_counter::HopCounter;

pub struct PathFinder {}

impl PathFinder {
    pub fn new() -> Self {
        Self {}
    }

    /// Evaluates the graph. Returns an ordered list of connected peers
    /// ranked by their shortest path to a known relay.
    pub fn rank_next_hops(
        &self,
        graph: &MeshGraph,
        hop_counter: &HopCounter,
        active_connections: &[[u8; 32]],
    ) -> Vec<[u8; 32]> {
        if active_connections.is_empty() {
            return Vec::new();
        }

        // Run BFS multi-source from all active connections to find shortest paths to any known node with a non-zero distance to a relay
        let mut shortest_paths: HashMap<[u8; 32], usize> = HashMap::new();

        for &start_node in active_connections {
            // BFS state for this specific start node
            let mut queue = VecDeque::new();
            let mut visited = HashSet::new();

            queue.push_back((start_node, 0));
            visited.insert(start_node);

            let mut min_distance = usize::MAX;

            while let Some((current_node, depth)) = queue.pop_front() {
                // If we know the distance from current_node to a relay, we have a path!
                if let Some(&relay_dist) = hop_counter.peer_distances.get(&current_node) {
                    let total_distance = depth + (relay_dist as usize);
                    min_distance = min_distance.min(total_distance);
                    // Don't break, there might be a shorter path via another node if this relay_dist is large.
                }

                if let Some(neighbors) = graph.get_neighbors(&current_node) {
                    for &neighbor in neighbors {
                        if !visited.contains(&neighbor) {
                            visited.insert(neighbor);
                            queue.push_back((neighbor, depth + 1));
                        }
                    }
                }
            }

            if min_distance != usize::MAX {
                shortest_paths.insert(start_node, min_distance);
            }
        }

        // Sort active connections based on shortest paths.
        // Unreachable peers go to the end.
        let mut ranked: Vec<[u8; 32]> = active_connections.to_vec();
        ranked.sort_by_key(|peer| shortest_paths.get(peer).copied().unwrap_or(usize::MAX));

        ranked
    }
}

impl Default for PathFinder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use crate::message::types::TopologyUpdate;

    use super::*;

    fn pk(b: u8) -> [u8; 32] {
        [b; 32]
    }

    #[test]
    fn ranks_peers_with_known_relays_higher() {
        let pf = PathFinder::new();
        let graph = MeshGraph::new();
        let mut hc = HopCounter::new();

        // peer 1 is 10 hops away, peer 2 is 2 hops away, peer 3 is unknown
        hc.update_distance(pk(1), 10);
        hc.update_distance(pk(2), 2);

        let active = vec![pk(1), pk(2), pk(3)];

        // They are directly connected, so distance is just their hop count
        let ranked = pf.rank_next_hops(&graph, &hc, &active);

        assert_eq!(ranked[0], pk(2)); // dist 2
        assert_eq!(ranked[1], pk(1)); // dist 10
        assert_eq!(ranked[2], pk(3)); // unknown (usize::MAX)
    }

    #[test]
    fn calculates_multi_hop_distances() {
        let pf = PathFinder::new();
        let mut graph = MeshGraph::new();
        let mut hc = HopCounter::new();

        // active connections: pk(1), pk(2)
        // pk(1) has no path to relay
        // pk(2) -> pk(3) -> pk(4) (which is 1 hop from relay)

        // build graph
        graph.apply_update(&TopologyUpdate {
            origin_pubkey: pk(2),
            directly_connected_peers: vec![pk(3)],
            hops_to_relay: 255, // unknown at origin
        });
        graph.apply_update(&TopologyUpdate {
            origin_pubkey: pk(3),
            directly_connected_peers: vec![pk(4)],
            hops_to_relay: 255,
        });

        hc.update_distance(pk(4), 1); // pk(4) is 1 hop away

        let active = vec![pk(1), pk(2)];
        let ranked = pf.rank_next_hops(&graph, &hc, &active);

        // pk(2) distance: depth to pk(4) is 2, pk(4) dist is 1 => total 3
        // pk(1) distance: unreachable
        assert_eq!(ranked[0], pk(2));
        assert_eq!(ranked[1], pk(1));
    }
}
