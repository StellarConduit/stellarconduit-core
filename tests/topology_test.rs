use std::collections::HashSet;

// Updated to match your exact submodule structure
use stellarconduit_core::topology::graph::MeshGraph; 
use stellarconduit_core::topology::hop_counter::HopCounter;

// ==========================================
// Mesh Graph Tests
// ==========================================

#[test]
fn test_graph_apply_update_stores_edges() {
    let mut graph = MeshGraph::new();
    graph.apply_update("node_A", "node_B");
    
    let neighbors = graph.get_neighbors("node_A").expect("node_A should exist in graph");
    assert!(neighbors.contains("node_B"), "Graph should store the directed edge to node_B");
}

#[test]
fn test_graph_get_neighbors_for_unknown_peer_returns_none() {
    let graph = MeshGraph::new();
    assert!(graph.get_neighbors("unknown_node").is_none());
}

#[test]
fn test_graph_ignores_self_loop_edges() {
    let mut graph = MeshGraph::new();
    graph.apply_update("node_A", "node_A");
    
    let neighbors = graph.get_neighbors("node_A");
    if let Some(list) = neighbors {
        assert!(!list.contains("node_A"), "Self-loops must be ignored");
    } else {
        assert!(neighbors.is_none());
    }
}

// ==========================================
// Hop Counter Tests
// ==========================================

#[test]
fn test_hop_counter_returns_highest_when_no_connections() {
    let counter = HopCounter::new();
    assert_eq!(counter.get_my_hops(), 255);
}

#[test]
fn test_hop_counter_calculates_min_plus_one() {
    let mut counter = HopCounter::new();
    counter.update_peer("peer_B", 2);
    assert_eq!(counter.get_my_hops(), 3);
    
    counter.update_peer("peer_C", 5);
    assert_eq!(counter.get_my_hops(), 3);
}

#[test]
fn test_hop_counter_recognizes_direct_relay() {
    let mut counter = HopCounter::new();
    counter.update_peer("peer_C", 3);
    counter.update_peer("peer_B", 0);
    assert_eq!(counter.get_my_hops(), 1);
}