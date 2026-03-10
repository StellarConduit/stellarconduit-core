use stellarconduit_core::message::types::TopologyUpdate;
/// tests/router_test.rs
///
/// External unit tests for `PathFinder` and `RelayRouter`.
/// Run with: cargo test --test router_test
///
/// These tests cover the four cases specified in Issue #47:
///   - PathFinder ranks a relay-adjacent peer before a distant one
///   - PathFinder returns an empty list when no connections exist
///   - RelayRouter respects `target_fanout` and returns exactly N peers
///   - RelayRouter falls back to returning peers even when all hop distances are 255 (unknown)
use stellarconduit_core::router::path_finder::PathFinder;
use stellarconduit_core::router::relay_router::RelayRouter;
use stellarconduit_core::topology::graph::MeshGraph;
use stellarconduit_core::topology::hop_counter::HopCounter;

// ── helpers ────────────────────────────────────────────────────────────────

/// Build a deterministic 32-byte peer key from a single byte.
fn pk(b: u8) -> [u8; 32] {
    [b; 32]
}

// ══════════════════════════════════════════════════════════════════════════
// PathFinder tests
// ══════════════════════════════════════════════════════════════════════════

/// Issue #47 Task 1 — PathFinder: rank relay-adjacent peer first.
///
/// Topology (no graph edges needed — peers are directly connected to us):
///   peer_a  is 1 hop from a relay  → total distance = 1
///   peer_b  is 5 hops from a relay → total distance = 5
///
/// Expected: ranked[0] == peer_a, ranked[1] == peer_b
#[test]
fn test_pathfinder_ranks_relay_adjacent_peer_first() {
    let pf = PathFinder::new();
    let graph = MeshGraph::new(); // no edges; peers are direct connections
    let mut hc = HopCounter::new();

    let peer_a = pk(0xAA); // 1 hop to relay
    let peer_b = pk(0xBB); // 5 hops to relay

    hc.update_distance(peer_a, 1);
    hc.update_distance(peer_b, 5);

    let active = vec![peer_b, peer_a]; // deliberately reversed to prove sorting
    let ranked = pf.rank_next_hops(&graph, &hc, &active);

    assert_eq!(ranked.len(), 2, "ranked list must contain all active peers");
    assert_eq!(
        ranked[0], peer_a,
        "peer_a (1 hop) must rank before peer_b (5 hops)"
    );
    assert_eq!(ranked[1], peer_b);
}

/// Issue #47 Task 1 — PathFinder: empty active_connections → empty result.
#[test]
fn test_pathfinder_handles_empty_connections() {
    let pf = PathFinder::new();
    let graph = MeshGraph::new();
    let hc = HopCounter::new();

    let ranked = pf.rank_next_hops(&graph, &hc, &[]);

    assert!(
        ranked.is_empty(),
        "with no connections the ranked list must be empty"
    );
}

/// Additional: peers with no hop data are sorted behind peers with known paths.
///
/// Topology:
///   peer_known   has a hop distance of 3 → ranked first
///   peer_unknown has no entry in HopCounter → ranked last (usize::MAX)
#[test]
fn test_pathfinder_unknown_peers_ranked_last() {
    let pf = PathFinder::new();
    let graph = MeshGraph::new();
    let mut hc = HopCounter::new();

    let peer_known = pk(0x01);
    let peer_unknown = pk(0x02);

    hc.update_distance(peer_known, 3);

    // unknown peer comes first in the input slice — PathFinder must re-sort it
    let active = vec![peer_unknown, peer_known];
    let ranked = pf.rank_next_hops(&graph, &hc, &active);

    assert_eq!(ranked[0], peer_known);
    assert_eq!(ranked[1], peer_unknown);
}

/// Additional: PathFinder follows graph edges to reach a relay-aware node.
///
/// Graph topology (not direct connections):
///   active: [peer_a, peer_b]
///   peer_a  has no path to any relay-aware node
///   peer_b → intermediate → relay_aware_node (distance 1 from relay)
///             (depth 0→1→2 through the graph, +1 relay dist = total 3)
///
/// Expected: ranked[0] == peer_b (total 3), ranked[1] == peer_a (usize::MAX)
#[test]
fn test_pathfinder_follows_graph_edges_to_relay() {
    let pf = PathFinder::new();
    let mut graph = MeshGraph::new();
    let mut hc = HopCounter::new();

    let peer_a = pk(0x01);
    let peer_b = pk(0x02);
    let intermediate = pk(0x03);
    let relay_aware = pk(0x04);

    // peer_b → intermediate → relay_aware
    graph.apply_update(&TopologyUpdate {
        origin_pubkey: peer_b,
        directly_connected_peers: vec![intermediate],
        hops_to_relay: 255,
    });
    graph.apply_update(&TopologyUpdate {
        origin_pubkey: intermediate,
        directly_connected_peers: vec![relay_aware],
        hops_to_relay: 255,
    });

    hc.update_distance(relay_aware, 1); // relay_aware is 1 hop from relay

    let active = vec![peer_a, peer_b];
    let ranked = pf.rank_next_hops(&graph, &hc, &active);

    assert_eq!(
        ranked[0], peer_b,
        "peer_b has graph path to relay; must rank first"
    );
    assert_eq!(
        ranked[1], peer_a,
        "peer_a has no relay path; must rank last"
    );
}

// ══════════════════════════════════════════════════════════════════════════
// RelayRouter tests
// ══════════════════════════════════════════════════════════════════════════

/// Issue #47 Task 1 — RelayRouter: target_fanout is respected.
///
/// 10 available peers, target_fanout = 3 → returns exactly 3.
#[test]
fn test_relay_router_respects_target_fanout() {
    let router = RelayRouter::new();

    // Simulate a pre-ranked slice of 10 peers (as PathFinder would produce).
    // Ranks don't matter here — we're testing that RelayRouter truncates correctly.
    let ranked_peers: Vec<[u8; 32]> = (0u8..10).map(pk).collect();

    let selected = router.select_next_hops(3, &ranked_peers);

    assert_eq!(
        selected.len(),
        3,
        "RelayRouter must return exactly target_fanout peers when more are available"
    );
}

/// Issue #47 Task 1 — RelayRouter: fallback when no relay path is known.
///
/// PathFinder assigns usize::MAX to all peers (hop distance = 255 for all).
/// RelayRouter must still return `target_fanout` peers — it cannot return zero.
///
/// We simulate this by building a ranked list where PathFinder would have sorted
/// everything to the back (all 255), then confirm RelayRouter still selects.
#[test]
fn test_relay_router_falls_back_to_random_when_no_relay_path() {
    let pf = PathFinder::new();
    let graph = MeshGraph::new();
    let mut hc = HopCounter::new();

    // Mark ALL peers as 255 — "no relay path known"
    let peers: Vec<[u8; 32]> = (0u8..6).map(pk).collect();
    for &peer in &peers {
        hc.update_distance(peer, 255);
    }

    // PathFinder still returns all peers (all tied at usize::MAX equivalent)
    let ranked = pf.rank_next_hops(&graph, &hc, &peers);

    // RelayRouter must still pick target_fanout peers from the ranked list
    let router = RelayRouter::new();
    let selected = router.select_next_hops(3, &ranked);

    assert_eq!(
        selected.len(),
        3,
        "RelayRouter must still return target_fanout peers even when all hop distances are 255"
    );

    // Every selected peer must be one of the original peers (no phantom entries)
    for peer in &selected {
        assert!(
            peers.contains(peer),
            "selected peer must come from the active peer list"
        );
    }
}

/// Edge: fanout larger than available peers → return all of them.
#[test]
fn test_relay_router_returns_all_when_fanout_exceeds_peer_count() {
    let router = RelayRouter::new();
    let ranked_peers: Vec<[u8; 32]> = vec![pk(1), pk(2)];

    let selected = router.select_next_hops(10, &ranked_peers);

    assert_eq!(
        selected.len(),
        2,
        "must return all peers when fanout > peer count"
    );
}

/// Edge: empty ranked list → empty result, no panic.
#[test]
fn test_relay_router_empty_input_returns_empty() {
    let router = RelayRouter::new();
    let selected = router.select_next_hops(5, &[]);
    assert!(selected.is_empty());
}

/// Edge: fanout of 0 → empty result.
#[test]
fn test_relay_router_zero_fanout_returns_empty() {
    let router = RelayRouter::new();
    let ranked_peers: Vec<[u8; 32]> = vec![pk(1), pk(2), pk(3)];
    let selected = router.select_next_hops(0, &ranked_peers);
    assert!(selected.is_empty(), "fanout=0 must return zero peers");
}
