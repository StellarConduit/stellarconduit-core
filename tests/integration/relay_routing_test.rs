/// Integration test: relay-aware routing across a 4-node linear chain.
/// Run with: cargo test --test relay_routing_test
///
/// Acceptance criteria (Issue #47):
///   A message injected at one end of a Node0 – Node1 – Node2 – Node3(Relay)
///   chain must be deliverable to the relay node when routing is performed
///   hop-by-hop using PathFinder + RelayRouter.
///
/// Note on MeshSimulator / MeshBuilder (Issue #20):
///   Issue #20 is a prerequisite that has not yet been merged.  Rather than
///   block this PR or wire up non-existent infrastructure, we implement a
///   self-contained `MeshNode` / `MeshBuilder` harness here.  When Issue #20
///   lands, the harness can be replaced with the canonical `MeshSimulator`
///   and these test assertions remain valid.
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use stellarconduit_core::message::types::TopologyUpdate;
use stellarconduit_core::router::path_finder::PathFinder;
use stellarconduit_core::router::relay_router::RelayRouter;
use stellarconduit_core::topology::graph::MeshGraph;
use stellarconduit_core::topology::hop_counter::HopCounter;

// ── deterministic peer key helper ─────────────────────────────────────────

fn pk(b: u8) -> [u8; 32] {
    [b; 32]
}

// ══════════════════════════════════════════════════════════════════════════
// Minimal mesh simulation harness
// (replaces MeshSimulator from Issue #20 until that branch is merged)
// ══════════════════════════════════════════════════════════════════════════

/// Shared message store — records every (msg_id, recipient) delivery event.
type DeliveryLog = Arc<Mutex<HashSet<([u8; 32], [u8; 32])>>>;

/// A single simulated mesh node.
struct MeshNode {
    pub pubkey: [u8; 32],
    pub is_relay: bool,
    graph: MeshGraph,
    hop_counter: HopCounter,
    path_finder: PathFinder,
    router: RelayRouter,
    /// Direct peer connections (pubkeys we can forward to).
    connections: Vec<[u8; 32]>,
    /// Shared delivery log — every node writes here on receipt.
    delivery_log: DeliveryLog,
}

impl MeshNode {
    fn new(pubkey: [u8; 32], is_relay: bool, delivery_log: DeliveryLog) -> Self {
        Self {
            pubkey,
            is_relay,
            graph: MeshGraph::new(),
            hop_counter: HopCounter::new(),
            path_finder: PathFinder::new(),
            router: RelayRouter::new(),
            connections: Vec::new(),
            delivery_log,
        }
    }

    /// Connect this node to a neighbour (one-directional; caller must also
    /// call connect on the neighbour's side for bidirectional links).
    fn add_connection(&mut self, peer: [u8; 32]) {
        if !self.connections.contains(&peer) {
            self.connections.push(peer);
        }
    }

    /// Record knowledge of a neighbour's hop distance to the relay.
    fn learn_distance(&mut self, peer: [u8; 32], hops: u8) {
        self.hop_counter.update_distance(peer, hops);
    }

    /// Announce this node's own topology into its local graph.
    fn announce_topology(&mut self) {
        self.graph.apply_update(&TopologyUpdate {
            origin_pubkey: self.pubkey,
            directly_connected_peers: self.connections.clone(),
            hops_to_relay: if self.is_relay { 0 } else { 255 },
        });
    }

    /// Return the ordered next-hop list for a given message, using
    /// PathFinder + RelayRouter with fanout = 1 (forward to best single hop).
    fn select_next_hops(&self, fanout: usize) -> Vec<[u8; 32]> {
        let ranked =
            self.path_finder
                .rank_next_hops(&self.graph, &self.hop_counter, &self.connections);
        self.router.select_next_hops(fanout, &ranked)
    }
}

/// Builder that constructs a network of `MeshNode`s and orchestrates
/// the routing simulation.
struct MeshBuilder {
    nodes: Vec<MeshNode>,
    delivery_log: DeliveryLog,
}

impl MeshBuilder {
    fn new() -> Self {
        Self {
            nodes: Vec::new(),
            delivery_log: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    fn add_node(&mut self, pubkey: [u8; 32], is_relay: bool) {
        self.nodes.push(MeshNode::new(
            pubkey,
            is_relay,
            Arc::clone(&self.delivery_log),
        ));
    }

    /// Connect node at index `a` ↔ node at index `b` (bidirectional).
    fn connect(&mut self, a: usize, b: usize) {
        let pk_a = self.nodes[a].pubkey;
        let pk_b = self.nodes[b].pubkey;
        self.nodes[a].add_connection(pk_b);
        self.nodes[b].add_connection(pk_a);
    }

    /// Propagate topology knowledge so each node learns hop distances.
    ///
    /// Strategy: work backwards from the relay.  The relay is distance 0;
    /// its direct neighbour is distance 1; the next is distance 2; etc.
    /// This mirrors the real gossip-based hop-count propagation.
    fn propagate_topology(&mut self) {
        // Each node announces its own edges into its local graph.
        for node in self.nodes.iter_mut() {
            node.announce_topology();
        }

        // Build a pubkey → index lookup.
        let index_map: HashMap<[u8; 32], usize> = self
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.pubkey, i))
            .collect();

        // Find the relay node(s) and BFS outward to assign distances.
        let relay_pubkeys: Vec<[u8; 32]> = self
            .nodes
            .iter()
            .filter(|n| n.is_relay)
            .map(|n| n.pubkey)
            .collect();

        // BFS: (node_index, distance_to_relay)
        let mut visited: HashSet<usize> = HashSet::new();
        let mut queue: std::collections::VecDeque<(usize, u8)> = std::collections::VecDeque::new();

        for relay_pk in &relay_pubkeys {
            if let Some(&idx) = index_map.get(relay_pk) {
                queue.push_back((idx, 0));
                visited.insert(idx);
            }
        }

        // Collect (node_idx, peer_idx, distance) assignments so we can apply
        // them after the immutable borrow of `nodes` in the BFS.
        let mut distance_assignments: Vec<(usize, [u8; 32], u8)> = Vec::new();

        while let Some((node_idx, dist)) = queue.pop_front() {
            let connections = self.nodes[node_idx].connections.clone();
            for peer_pk in connections {
                if let Some(&peer_idx) = index_map.get(&peer_pk) {
                    if !visited.contains(&peer_idx) {
                        visited.insert(peer_idx);
                        let peer_dist = dist.saturating_add(1);
                        // node `peer_idx` should learn that `node_idx.pubkey` is `dist` hops from relay
                        let node_pk = self.nodes[node_idx].pubkey;
                        distance_assignments.push((peer_idx, node_pk, dist));
                        queue.push_back((peer_idx, peer_dist));
                    }
                }
            }
        }

        for (node_idx, known_peer_pk, dist) in distance_assignments {
            self.nodes[node_idx].learn_distance(known_peer_pk, dist);
        }
    }

    /// Simulate routing a message with `msg_id` starting from `origin_idx`.
    ///
    /// Each node that receives the message records receipt in `delivery_log`,
    /// then selects its best next-hop and forwards until no new next-hop is
    /// found or the message has been seen at a node already.
    fn simulate_routing(&self, origin_idx: usize, msg_id: [u8; 32]) {
        let index_map: HashMap<[u8; 32], usize> = self
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.pubkey, i))
            .collect();

        let mut seen: HashSet<[u8; 32]> = HashSet::new(); // nodes that have handled the msg
        let mut work_queue: std::collections::VecDeque<usize> = std::collections::VecDeque::new();

        // Origin node "receives" the message it is injecting.
        let origin_pk = self.nodes[origin_idx].pubkey;
        self.delivery_log
            .lock()
            .unwrap()
            .insert((msg_id, origin_pk));
        seen.insert(origin_pk);
        work_queue.push_back(origin_idx);

        while let Some(current_idx) = work_queue.pop_front() {
            // Use fanout=1 — forward to the single best next-hop (relay-adjacent).
            let next_hops = self.nodes[current_idx].select_next_hops(1);

            for next_pk in next_hops {
                if seen.contains(&next_pk) {
                    continue;
                }
                seen.insert(next_pk);

                // Record delivery at next_pk.
                self.delivery_log.lock().unwrap().insert((msg_id, next_pk));

                if let Some(&next_idx) = index_map.get(&next_pk) {
                    work_queue.push_back(next_idx);
                }
            }
        }
    }

    /// Returns true if `node_idx` has received a message with `msg_id`.
    fn node_has_received(&self, node_idx: usize, msg_id: [u8; 32]) -> bool {
        let node_pk = self.nodes[node_idx].pubkey;
        self.delivery_log
            .lock()
            .unwrap()
            .contains(&(msg_id, node_pk))
    }
}

// ══════════════════════════════════════════════════════════════════════════
// Integration tests
// ══════════════════════════════════════════════════════════════════════════

/// Issue #47 Task 2 — relay routing integration test.
///
/// Chain: Node0 – Node1 – Node2 – Node3(Relay)
/// Node0 injects a message.  The message must reach Node3 (the relay).
#[tokio::test]
async fn test_message_routes_to_relay_node() {
    // ── build topology ────────────────────────────────────────────────────
    let mut builder = MeshBuilder::new();

    // Indices:  0          1          2          3 (relay)
    builder.add_node(pk(0x00), false);
    builder.add_node(pk(0x01), false);
    builder.add_node(pk(0x02), false);
    builder.add_node(pk(0x03), true); // relay

    // Linear chain: 0 – 1 – 2 – 3
    builder.connect(0, 1);
    builder.connect(1, 2);
    builder.connect(2, 3);

    // Propagate hop distances (relay = 0, node2 = 1, node1 = 2, node0 = 3)
    builder.propagate_topology();

    // ── inject message at Node0 ───────────────────────────────────────────
    let msg_id = [0xDE; 32];
    builder.simulate_routing(0, msg_id);

    // ── allow async propagation (matches the spirit of the sleep in the issue)
    tokio::time::sleep(Duration::from_millis(50)).await;

    // ── assertions ────────────────────────────────────────────────────────

    // The relay (node 3) must have received the message.
    assert!(
        builder.node_has_received(3, msg_id),
        "relay node (index 3) must receive the message injected at node 0"
    );

    // Intermediate nodes must also have forwarded it (chain is linear).
    assert!(
        builder.node_has_received(1, msg_id),
        "node 1 must have relayed the message"
    );
    assert!(
        builder.node_has_received(2, msg_id),
        "node 2 must have relayed the message"
    );
}

/// Additional: message injected at the relay itself is recorded at the relay.
#[tokio::test]
async fn test_message_injected_at_relay_is_received() {
    let mut builder = MeshBuilder::new();

    builder.add_node(pk(0x10), false);
    builder.add_node(pk(0x11), true); // relay

    builder.connect(0, 1);
    builder.propagate_topology();

    let msg_id = [0xAB; 32];
    builder.simulate_routing(1, msg_id); // inject AT the relay

    tokio::time::sleep(Duration::from_millis(50)).await;

    assert!(
        builder.node_has_received(1, msg_id),
        "relay must record receipt when it is the origin"
    );
}

/// Additional: isolated node (no connections) — message stays at origin.
#[tokio::test]
async fn test_isolated_node_message_stays_at_origin() {
    let mut builder = MeshBuilder::new();

    builder.add_node(pk(0x20), false); // no connections
    builder.propagate_topology();

    let msg_id = [0xCC; 32];
    builder.simulate_routing(0, msg_id);

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Origin received it.
    assert!(builder.node_has_received(0, msg_id));
}
