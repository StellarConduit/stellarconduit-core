use crate::peer::peer_node::Peer;

pub enum PenaltyReason {
    InvalidSignature,
    DuplicateMessageFlood,
    ConnectionDropped,
}

pub enum RewardReason {
    SuccessfullyRoutedTx,
    ValidNewGossipEnvelope,
}

impl PenaltyReason {
    pub fn points(&self) -> u32 {
        match self {
            PenaltyReason::InvalidSignature => 20,
            PenaltyReason::DuplicateMessageFlood => 10,
            PenaltyReason::ConnectionDropped => 2,
        }
    }
}

impl RewardReason {
    pub fn points(&self) -> u32 {
        match self {
            RewardReason::SuccessfullyRoutedTx => 5,
            RewardReason::ValidNewGossipEnvelope => 2,
        }
    }
}

/// Subtract penalty points from a peer's reputation; ban the peer when it hits 0.
pub fn apply_penalty(peer: &mut Peer, reason: PenaltyReason) {
    let penalty = reason.points();
    peer.reputation = peer.reputation.saturating_sub(penalty);
    if peer.reputation == 0 {
        peer.is_banned = true;
    }
}

/// Add reward points to a peer's reputation, capped at 100.
pub fn apply_reward(peer: &mut Peer, reason: RewardReason) {
    let reward = reason.points();
    peer.reputation = (peer.reputation + reward).min(100);
}

/// Sort a peer slice in-place by reputation, descending.
/// Call before passing to `select_next_hops` if reputation-based routing is desired.
pub fn rank_by_reputation(peers: &mut [([u8; 32], u32)]) {
    peers.sort_unstable_by_key(|b| std::cmp::Reverse(b.1));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer::peer_node::Peer;

    #[test]
    fn test_penalty_degrades_reputation() {
        let mut peer = Peer::new([1u8; 32]);
        peer.reputation = 50;
        apply_penalty(&mut peer, PenaltyReason::InvalidSignature);
        assert_eq!(peer.reputation, 30);
    }

    #[test]
    fn test_penalty_bans_at_zero() {
        let mut peer = Peer::new([2u8; 32]);
        peer.reputation = 20;
        apply_penalty(&mut peer, PenaltyReason::InvalidSignature);
        assert!(peer.is_banned);
    }

    #[test]
    fn test_reward_increases_reputation_capped_at_100() {
        let mut peer = Peer::new([3u8; 32]);
        peer.reputation = 98;
        apply_reward(&mut peer, RewardReason::ValidNewGossipEnvelope);
        assert_eq!(peer.reputation, 100);
    }

    #[test]
    fn test_rank_by_reputation_sorts_descending() {
        let mut peers: Vec<([u8; 32], u32)> =
            vec![([1u8; 32], 30), ([2u8; 32], 80), ([3u8; 32], 50)];
        rank_by_reputation(&mut peers);
        assert_eq!(peers[0].1, 80);
        assert_eq!(peers[1].1, 50);
        assert_eq!(peers[2].1, 30);
    }
}
