//! Background garbage collection for the StellarConduit mesh engine.
//!
//! `StatePruner` periodically removes stale peers, dead topology edges, and
//! expired pending messages from both in-memory data structures and the
//! persistent `MeshDatabase`.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use crate::discovery::peer_list::PeerList;
use crate::persistence::db::MeshDatabase;
use crate::topology::graph::MeshGraph;

// ─── Pruning thresholds ───────────────────────────────────────────────────────

/// Remove peers not seen in the last 30 minutes.
pub const PEER_TIMEOUT: Duration = Duration::from_secs(60 * 30);

/// Remove topology edges not refreshed in the last hour.
pub const EDGE_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// Drop pending messages older than 24 hours.
pub const MSG_MAX_AGE: Duration = Duration::from_secs(60 * 60 * 24);

/// How often the pruner wakes up to sweep.
pub const PRUNE_INTERVAL: Duration = Duration::from_secs(60 * 5);

// ─── StatePruner ─────────────────────────────────────────────────────────────

pub struct StatePruner {
    graph: Arc<Mutex<MeshGraph>>,
    peer_list: Arc<Mutex<PeerList>>,
    db: Arc<MeshDatabase>,
}

impl StatePruner {
    pub fn new(
        graph: Arc<Mutex<MeshGraph>>,
        peer_list: Arc<Mutex<PeerList>>,
        db: Arc<MeshDatabase>,
    ) -> Self {
        Self {
            graph,
            peer_list,
            db,
        }
    }

    /// Spawn a Tokio task that runs the pruning loop indefinitely.
    ///
    /// The task sleeps for `PRUNE_INTERVAL` between each sweep, acquiring
    /// locks only long enough to prune and then releasing them immediately.
    pub async fn start_background_task(self) {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(PRUNE_INTERVAL).await;
                self.prune_peers().await;
                self.prune_graph_edges().await;
                self.prune_pending_messages().await;
            }
        });
    }

    // ── Internal pruning routines ─────────────────────────────────────────────

    /// Prune stale peers from the `PeerList` and mark them offline in the DB.
    pub async fn prune_peers(&self) {
        let lost = {
            let mut list = self.peer_list.lock().await;
            list.prune_stale_peers()
        };

        for event in lost {
            use crate::discovery::events::DiscoveryEvent;
            if let DiscoveryEvent::PeerLost(identity) = event {
                let _ = self.db.mark_peer_offline(&identity.pubkey).await;
                log::debug!("Pruner: marked peer {:?} offline", &identity.pubkey[..4]);
            }
        }
    }

    /// Prune topology edges that haven't been refreshed within `EDGE_TIMEOUT`.
    pub async fn prune_graph_edges(&self) {
        let pruned = {
            let mut graph = self.graph.lock().await;
            graph.prune_stale_edges(EDGE_TIMEOUT)
        };
        if pruned > 0 {
            log::debug!("Pruner: removed {} stale graph edge(s)", pruned);
        }
    }

    /// Delete pending messages older than `MSG_MAX_AGE` from the database.
    pub async fn prune_pending_messages(&self) {
        let cutoff = {
            use std::time::{SystemTime, UNIX_EPOCH};
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            now.saturating_sub(MSG_MAX_AGE.as_secs())
        };

        let deleted = self
            .db
            .delete_messages_older_than(cutoff)
            .await
            .unwrap_or(0);
        if deleted > 0 {
            log::debug!("Pruner: deleted {} expired message(s)", deleted);
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::types::TopologyUpdate;
    use std::time::Duration;

    fn pk(b: u8) -> [u8; 32] {
        [b; 32]
    }

    fn make_pruner(
        graph: Arc<Mutex<MeshGraph>>,
        peer_list: Arc<Mutex<PeerList>>,
        db: Arc<MeshDatabase>,
    ) -> StatePruner {
        StatePruner::new(graph, peer_list, db)
    }

    // ── prune_peers ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn prune_peers_removes_stale_peer() {
        let peer_list = Arc::new(Mutex::new(PeerList::new(30 * 60)));
        let db = Arc::new(MeshDatabase::new_stub());
        let graph = Arc::new(Mutex::new(MeshGraph::new()));

        // Insert a peer then backdate it past the expiry window
        {
            let mut list = peer_list.lock().await;
            list.insert_or_update(pk(0xAA), 80);
            // Set last_seen to 2 hours ago (well past 30-min expiry)
            list.set_last_seen(&pk(0xAA), 0);
        }

        // Must exist in DB to be marked offline
        let mut p = crate::peer::peer_node::Peer::new(pk(0xAA));
        p.is_banned = false;
        db.save_peer(&p).await.unwrap();

        let pruner = make_pruner(graph, peer_list.clone(), db.clone());
        pruner.prune_peers().await;

        let active = peer_list.lock().await.get_active_peers().len();
        assert_eq!(active, 0, "stale peer should be removed");
        assert_eq!(
            db.offline_peer_count().await,
            1,
            "DB should record offline peer"
        );
    }

    #[tokio::test]
    async fn prune_peers_keeps_fresh_peer() {
        let peer_list = Arc::new(Mutex::new(PeerList::new(30 * 60)));
        let db = Arc::new(MeshDatabase::new_stub());
        let graph = Arc::new(Mutex::new(MeshGraph::new()));

        {
            let mut list = peer_list.lock().await;
            list.insert_or_update(pk(0xBB), 70); // just seen
        }

        let pruner = make_pruner(graph, peer_list.clone(), db.clone());
        pruner.prune_peers().await;

        let active = peer_list.lock().await.get_active_peers().len();
        assert_eq!(active, 1, "fresh peer should be kept");
        assert_eq!(db.offline_peer_count().await, 0);
    }

    // ── prune_graph_edges ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn prune_graph_edges_removes_old_edge() {
        let graph = Arc::new(Mutex::new(MeshGraph::new()));
        let peer_list = Arc::new(Mutex::new(PeerList::new(30 * 60)));
        let db = Arc::new(MeshDatabase::new_stub());

        {
            let mut g = graph.lock().await;
            g.apply_update(&TopologyUpdate {
                origin_pubkey: pk(1),
                directly_connected_peers: vec![pk(2)],
                hops_to_relay: 1,
                topology_flags: vec![],
            });
            // Backdate the edge to 2 hours ago
            g.backdate_edge(&pk(1), Duration::from_secs(7200));
        }

        let pruner = make_pruner(graph.clone(), peer_list, db);
        pruner.prune_graph_edges().await;

        assert_eq!(
            graph.lock().await.node_count(),
            0,
            "stale edge should be pruned"
        );
    }

    #[tokio::test]
    async fn prune_graph_edges_keeps_fresh_edge() {
        let graph = Arc::new(Mutex::new(MeshGraph::new()));
        let peer_list = Arc::new(Mutex::new(PeerList::new(30 * 60)));
        let db = Arc::new(MeshDatabase::new_stub());

        {
            let mut g = graph.lock().await;
            g.apply_update(&TopologyUpdate {
                origin_pubkey: pk(3),
                directly_connected_peers: vec![pk(4)],
                hops_to_relay: 2,
                topology_flags: vec![],
            });
            // No backdating — edge is fresh
        }

        let pruner = make_pruner(graph.clone(), peer_list, db);
        pruner.prune_graph_edges().await;

        assert_eq!(
            graph.lock().await.node_count(),
            1,
            "fresh edge should be kept"
        );
    }

    // ── prune_pending_messages ────────────────────────────────────────────────

    #[tokio::test]
    async fn prune_pending_messages_removes_old_messages() {
        let graph = Arc::new(Mutex::new(MeshGraph::new()));
        let peer_list = Arc::new(Mutex::new(PeerList::new(30 * 60)));
        let db = Arc::new(MeshDatabase::new_stub());

        let future_ts = {
            use std::time::{SystemTime, UNIX_EPOCH};
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
                + 100_000
        };

        // Insert two messages: one very old, one fresh
        db.insert_pending_message(pk(0xA1), 1000).await; // ancient
        db.insert_pending_message(pk(0xA2), future_ts).await; // future
        assert_eq!(db.pending_message_count().await, 2);

        let pruner = make_pruner(graph, peer_list, db.clone());
        pruner.prune_pending_messages().await;

        assert_eq!(
            db.pending_message_count().await,
            1,
            "old message should be deleted"
        );
    }

    #[tokio::test]
    async fn prune_pending_messages_keeps_fresh_messages() {
        let graph = Arc::new(Mutex::new(MeshGraph::new()));
        let peer_list = Arc::new(Mutex::new(PeerList::new(30 * 60)));
        let db = Arc::new(MeshDatabase::new_stub());

        let future_ts = {
            use std::time::{SystemTime, UNIX_EPOCH};
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
                + 100_000
        };

        // Insert only a fresh message
        db.insert_pending_message(pk(0xB1), future_ts).await;
        assert_eq!(db.pending_message_count().await, 1);

        let pruner = make_pruner(graph, peer_list, db.clone());
        pruner.prune_pending_messages().await;

        assert_eq!(
            db.pending_message_count().await,
            1,
            "fresh message should be kept"
        );
    }

    // ── constant sanity ───────────────────────────────────────────────────────

    #[test]
    fn pruning_constants_are_reasonable() {
        assert!(
            PEER_TIMEOUT < EDGE_TIMEOUT,
            "edges should outlast peer timeouts"
        );
        assert!(
            EDGE_TIMEOUT < MSG_MAX_AGE,
            "messages live longer than edges"
        );
        assert!(
            PRUNE_INTERVAL < PEER_TIMEOUT,
            "pruner runs before first expiry"
        );
    }
}
