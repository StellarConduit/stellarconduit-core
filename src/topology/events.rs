use tokio::sync::broadcast;

#[derive(Debug, Clone)]
pub enum TopologyEvent {
    PeerUpdated {
        pubkey: [u8; 32],
        hops_to_relay: u8,
        neighbor_count: usize,
    },
    PeerPruned {
        pubkey: [u8; 32],
    },
    RelayReachabilityGained {
        via_peer: [u8; 32],
        hops: u8,
    },
    RelayReachabilityLost,
    MeshSizeChanged {
        previous: usize,
        current: usize,
    },
    PeerDisconnected {
        peer_pubkey: [u8; 32],
    },
    RelayLost {
        relay_pubkey: [u8; 32],
    },
    PeerUnreachable {
        peer_pubkey: [u8; 32],
    },
    ClusterMerge {
        origin_pubkey: [u8; 32],
    },
    PartitionDetected {
        origin_pubkey: [u8; 32],
    },
}

impl TopologyEvent {
    pub fn affected_peer_pubkey(&self) -> [u8; 32] {
        match self {
            TopologyEvent::PeerDisconnected { peer_pubkey } => *peer_pubkey,
            TopologyEvent::RelayLost { relay_pubkey } => *relay_pubkey,
            TopologyEvent::PeerUnreachable { peer_pubkey } => *peer_pubkey,
            TopologyEvent::ClusterMerge { origin_pubkey } => *origin_pubkey,
            TopologyEvent::PartitionDetected { origin_pubkey } => *origin_pubkey,
            TopologyEvent::PeerUpdated { pubkey, .. } => *pubkey,
            TopologyEvent::PeerPruned { pubkey } => *pubkey,
            TopologyEvent::RelayReachabilityGained { via_peer, .. } => *via_peer,
            _ => [0u8; 32],
        }
    }
}

#[derive(Clone)]
pub struct TopologyEventBus {
    sender: broadcast::Sender<TopologyEvent>,
}

impl TopologyEventBus {
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<TopologyEvent> {
        self.sender.subscribe()
    }

    pub fn publish(&self, event: TopologyEvent) {
        let _ = self.sender.send(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::types::TopologyUpdate;
    use crate::topology::graph::MeshGraph;
    use crate::topology::hop_counter::HopCounter;
    use std::time::{Duration, Instant};

    fn pk(b: u8) -> [u8; 32] {
        [b; 32]
    }

    #[test]
    fn test_peer_updated_event_fires() {
        let bus = TopologyEventBus::new(64);
        let mut rx = bus.subscribe();
        let mut graph = MeshGraph::new();

        graph.apply_update(
            &TopologyUpdate {
                origin_pubkey: pk(1),
                directly_connected_peers: vec![pk(2), pk(3)],
                hops_to_relay: 3,
                topology_flags: vec![],
            },
            Some(&bus),
        );

        let event = rx.try_recv().expect("should receive event");
        match event {
            TopologyEvent::PeerUpdated {
                pubkey,
                hops_to_relay,
                neighbor_count,
            } => {
                assert_eq!(pubkey, pk(1));
                assert_eq!(hops_to_relay, 3);
                assert_eq!(neighbor_count, 2);
            }
            other => panic!("expected PeerUpdated, got {:?}", other),
        }
    }

    #[test]
    fn test_peer_pruned_event_fires() {
        let bus = TopologyEventBus::new(64);
        let mut rx = bus.subscribe();
        let mut graph = MeshGraph::new();

        graph.apply_update(
            &TopologyUpdate {
                origin_pubkey: pk(1),
                directly_connected_peers: vec![pk(2)],
                hops_to_relay: 1,
                topology_flags: vec![],
            },
            Some(&bus),
        );
        // consume the PeerUpdated event so we only see PeerPruned
        let _ = rx.try_recv();

        graph.backdate_edge(&pk(1), Duration::from_secs(7200));
        graph.prune_stale_edges(Duration::from_secs(3600), Some(&bus));

        let event = rx.try_recv().expect("should receive event");
        match event {
            TopologyEvent::PeerPruned { pubkey } => {
                assert_eq!(pubkey, pk(1));
            }
            other => panic!("expected PeerPruned, got {:?}", other),
        }
    }

    #[test]
    fn test_relay_reachability_gained() {
        let bus = TopologyEventBus::new(64);
        let mut rx = bus.subscribe();
        let mut hc = HopCounter::new();

        // First call with hops=255 — no event
        hc.update_distance(pk(1), 255, Some(&bus));
        assert!(rx.try_recv().is_err(), "no event expected on 255");

        // Second call with hops=3 — RelayReachabilityGained should fire
        hc.update_distance(pk(1), 3, Some(&bus));

        let event = rx.try_recv().expect("should receive event");
        match event {
            TopologyEvent::RelayReachabilityGained { via_peer, hops } => {
                assert_eq!(via_peer, pk(1));
                assert_eq!(hops, 3);
            }
            other => panic!("expected RelayReachabilityGained, got {:?}", other),
        }
    }

    #[test]
    fn test_relay_reachability_lost() {
        let bus = TopologyEventBus::new(64);
        let mut rx = bus.subscribe();
        let mut hc = HopCounter::new();

        // Set up one reachable peer
        hc.update_distance(pk(1), 3, Some(&bus));
        // consume any event from setup
        let _ = rx.try_recv();

        // Change the only reachable peer to 255 — RelayReachabilityLost
        hc.update_distance(pk(1), 255, Some(&bus));

        let event = rx.try_recv().expect("should receive event");
        match event {
            TopologyEvent::RelayReachabilityLost => {}
            other => panic!("expected RelayReachabilityLost, got {:?}", other),
        }
    }

    #[test]
    fn test_lagged_subscriber_does_not_block_publisher() {
        let bus = TopologyEventBus::new(64);
        let _rx = bus.subscribe();

        let start = Instant::now();
        for i in 0..100u8 {
            bus.publish(TopologyEvent::PeerUpdated {
                pubkey: [i; 32],
                hops_to_relay: 0,
                neighbor_count: 0,
            });
        }
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_millis(1),
            "publishing 100 events took {:?}",
            elapsed
        );
    }
}
