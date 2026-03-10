use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::discovery::events::DiscoveryEvent;
use crate::peer::peer_node::Peer;

pub struct PeerList {
    /// Maps public key to the Peer struct
    peers: HashMap<[u8; 32], Peer>,
    /// How many seconds before a peer is considered offline
    expiry_seconds: u64,
}

fn now_unix_sec() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl PeerList {
    pub fn new(expiry_seconds: u64) -> Self {
        Self {
            peers: HashMap::new(),
            expiry_seconds,
        }
    }

    /// Insert a new peer or update the last-seen timestamp for an existing one.
    /// Returns `PeerDiscovered` for first contact, `PeerUpdated` for subsequent contacts.
    pub fn insert_or_update(
        &mut self,
        pubkey: [u8; 32],
        signal_strength: u8,
    ) -> Option<DiscoveryEvent> {
        let now = now_unix_sec();

        if let Some(peer) = self.peers.get_mut(&pubkey) {
            peer.last_seen_unix_sec = now;
            Some(DiscoveryEvent::PeerUpdated(
                peer.identity.clone(),
                signal_strength,
            ))
        } else {
            let mut peer = Peer::new(pubkey);
            peer.last_seen_unix_sec = now;
            let identity = peer.identity.clone();
            self.peers.insert(pubkey, peer);
            Some(DiscoveryEvent::PeerDiscovered(identity))
        }
    }

    /// Returns only peers whose last-seen timestamp is within the expiry window.
    pub fn get_active_peers(&self) -> Vec<&Peer> {
        let now = now_unix_sec();
        self.peers
            .values()
            .filter(|p| now.saturating_sub(p.last_seen_unix_sec) <= self.expiry_seconds)
            .collect()
    }

    /// Removes stale peers (beyond expiry window) and returns a `PeerLost` event for each.
    pub fn prune_stale_peers(&mut self) -> Vec<DiscoveryEvent> {
        let now = now_unix_sec();
        let expiry = self.expiry_seconds;

        let stale_keys: Vec<[u8; 32]> = self
            .peers
            .iter()
            .filter(|(_, p)| now.saturating_sub(p.last_seen_unix_sec) > expiry)
            .map(|(k, _)| *k)
            .collect();

        stale_keys
            .into_iter()
            .filter_map(|key| {
                self.peers
                    .remove(&key)
                    .map(|p| DiscoveryEvent::PeerLost(p.identity))
            })
            .collect()
    }

    /// Returns total number of tracked peers (including stale ones not yet pruned).
    pub fn len(&self) -> usize {
        self.peers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    /// Test helper: directly set last_seen_unix_sec for a peer by pubkey.
    pub fn set_last_seen(&mut self, pubkey: &[u8; 32], ts: u64) {
        if let Some(p) = self.peers.get_mut(pubkey) {
            p.last_seen_unix_sec = ts;
        }
    }

    /// Ban a peer for the specified duration (in seconds).
    /// Returns true if the peer was found and banned.
    pub fn ban_peer(&mut self, pubkey: &[u8; 32], duration_sec: u64) -> bool {
        if let Some(peer) = self.peers.get_mut(pubkey) {
            peer.ban(duration_sec);
            true
        } else {
            false
        }
    }

    /// Check if a peer is currently banned.
    /// Note: This does not check expiration. Use check_ban_expirations() to update expired bans.
    pub fn is_peer_banned(&self, pubkey: &[u8; 32]) -> bool {
        self.peers.get(pubkey).map(|p| p.is_banned).unwrap_or(false)
    }

    /// Unban a peer immediately.
    pub fn unban_peer(&mut self, pubkey: &[u8; 32]) -> bool {
        if let Some(peer) = self.peers.get_mut(pubkey) {
            peer.is_banned = false;
            peer.ban_expires_at_unix_sec = 0;
            true
        } else {
            false
        }
    }

    /// Check and update ban expiration for all peers.
    /// Returns a list of peer identities that were unbanned.
    pub fn check_ban_expirations(&mut self) -> Vec<crate::peer::identity::PeerIdentity> {
        let mut unbanned = Vec::new();
        for peer in self.peers.values_mut() {
            if peer.check_ban_expiration() {
                unbanned.push(peer.identity.clone());
            }
        }
        unbanned
    }

    /// Get a peer by pubkey (mutable).
    pub fn get_peer_mut(&mut self, pubkey: &[u8; 32]) -> Option<&mut Peer> {
        self.peers.get_mut(pubkey)
    }

    /// Get a peer by pubkey.
    pub fn get_peer(&self, pubkey: &[u8; 32]) -> Option<&Peer> {
        self.peers.get(pubkey)
    }
}

/// Background pruning stub — call this on a Tokio task to auto-prune every `interval_secs`.
/// The caller is responsible for wrapping `peer_list` in an `Arc<tokio::sync::Mutex<PeerList>>`.
pub async fn background_pruning_loop(
    peer_list: std::sync::Arc<tokio::sync::Mutex<PeerList>>,
    interval_secs: u64,
) {
    let interval = std::time::Duration::from_secs(interval_secs);
    loop {
        tokio::time::sleep(interval).await;
        let mut list = peer_list.lock().await;
        let lost = list.prune_stale_peers();
        if !lost.is_empty() {
            log::debug!("Pruned {} stale peer(s) from PeerList", lost.len());
        }
    }
}
