pub struct RelayRouter;

impl RelayRouter {
    pub fn new() -> Self {
        Self
    }

    /// Selects `target_fanout` peers from a pre-ranked slice.
    /// `ranked_peers` is expected to be sorted by PathFinder (closest relay first).
    /// Falls back to the full slice when target exceeds available peers.
    pub fn select_next_hops(
        &self,
        target_fanout: usize,
        ranked_peers: &[[u8; 32]],
    ) -> Vec<[u8; 32]> {
        if ranked_peers.is_empty() {
            return Vec::new();
        }
        if target_fanout >= ranked_peers.len() {
            return ranked_peers.to_vec();
        }
        ranked_peers[..target_fanout].to_vec()
    }
}

impl Default for RelayRouter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn pk(b: u8) -> [u8; 32] {
        [b; 32]
    }

    #[test]
    fn select_next_hops_returns_empty_when_no_peers() {
        let router = RelayRouter::new();
        assert!(router.select_next_hops(5, &[]).is_empty());
    }

    #[test]
    fn select_next_hops_returns_all_when_target_exceeds_peers() {
        let router = RelayRouter::new();
        let peers = vec![pk(1), pk(2)];
        let selected = router.select_next_hops(5, &peers);
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0], pk(1));
        assert_eq!(selected[1], pk(2));
    }

    #[test]
    fn select_next_hops_truncates_to_target_fanout() {
        let router = RelayRouter::new();
        let peers = vec![pk(1), pk(2), pk(3), pk(4)];
        let selected = router.select_next_hops(2, &peers);
        assert_eq!(selected.len(), 2);

        // It should pick the top ones deterministically if we just truncate
        let set: HashSet<_> = selected.into_iter().collect();
        assert!(set.contains(&pk(1)));
        assert!(set.contains(&pk(2)));
    }
}
