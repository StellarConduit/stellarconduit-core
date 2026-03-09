use rand::seq::SliceRandom;

use crate::peer::identity::PeerIdentity;

pub const MIN_FANOUT: usize = 2;
pub const MAX_FANOUT: usize = 6;
pub const FALLBACK_FANOUT: usize = 3;

pub struct FanoutCalculator {
    pub min_fanout: usize,
    pub max_fanout: usize,
}

impl FanoutCalculator {
    pub fn new() -> Self {
        Self {
            min_fanout: MIN_FANOUT,
            max_fanout: MAX_FANOUT,
        }
    }

    pub fn calculate(
        &self,
        active_connections: usize,
        mesh_estimated_size: Option<usize>,
    ) -> usize {
        if active_connections <= self.min_fanout {
            return active_connections;
        }

        if mesh_estimated_size.is_none() && active_connections > self.max_fanout {
            return FALLBACK_FANOUT.min(active_connections);
        }

        let target = match mesh_estimated_size {
            Some(estimated_size) => {
                let n = estimated_size.max(1) as f64;
                n.ln().ceil() as usize
            }
            None => active_connections,
        };

        target
            .clamp(self.min_fanout, self.max_fanout)
            .min(active_connections)
    }
}

impl Default for FanoutCalculator {
    fn default() -> Self {
        Self::new()
    }
}

pub fn select_random_peers(peers: &[PeerIdentity], f: usize) -> Vec<PeerIdentity> {
    if peers.is_empty() || f == 0 {
        return Vec::new();
    }

    let mut rng = rand::thread_rng();
    peers
        .choose_multiple(&mut rng, f.min(peers.len()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn returns_all_when_active_is_below_min() {
        let calc = FanoutCalculator::new();
        assert_eq!(calc.calculate(0, None), 0);
        assert_eq!(calc.calculate(1, None), 1);
        assert_eq!(calc.calculate(2, None), 2);
    }

    #[test]
    fn uses_fallback_when_mesh_unknown_and_active_is_large() {
        let calc = FanoutCalculator::new();
        assert_eq!(calc.calculate(7, None), FALLBACK_FANOUT);
        assert_eq!(calc.calculate(100, None), FALLBACK_FANOUT);
    }

    #[test]
    fn mesh_known_uses_logarithmic_scaling_with_bounds() {
        let calc = FanoutCalculator::new();

        assert_eq!(calc.calculate(3, Some(2)), 2); // ceil(ln(2)) => 1, clamped to min=2
        assert_eq!(calc.calculate(6, Some(8)), 3); // ceil(ln(8)) => 3
        assert_eq!(calc.calculate(6, Some(100)), 5); // ceil(ln(100)) => 5
        assert_eq!(calc.calculate(6, Some(1_000_000)), 6); // capped by max
    }

    #[test]
    fn mesh_known_respects_active_connection_cap() {
        let calc = FanoutCalculator::new();
        assert_eq!(calc.calculate(4, Some(1_000_000)), 4);
    }

    #[test]
    fn unknown_mesh_mid_range_returns_active_connections() {
        let calc = FanoutCalculator::new();
        assert_eq!(calc.calculate(3, None), 3);
        assert_eq!(calc.calculate(6, None), 6);
    }

    #[test]
    fn random_selection_returns_empty_for_zero_or_empty_input() {
        let peers = vec![PeerIdentity::new(1), PeerIdentity::new(2)];
        assert!(select_random_peers(&peers, 0).is_empty());
        assert!(select_random_peers(&[], 3).is_empty());
    }

    #[test]
    fn random_selection_is_unique_and_bounded() {
        let peers: Vec<PeerIdentity> = (0..10).map(PeerIdentity::new).collect();

        let selected = select_random_peers(&peers, 6);
        assert_eq!(selected.len(), 6);

        let set: HashSet<PeerIdentity> = selected.iter().cloned().collect();
        assert_eq!(set.len(), selected.len());

        for peer in selected {
            assert!(peers.contains(&peer));
        }
    }

    #[test]
    fn random_selection_with_f_above_peer_count_returns_all_unique_peers() {
        let peers: Vec<PeerIdentity> = (0..5).map(PeerIdentity::new).collect();
        let selected = select_random_peers(&peers, 99);

        assert_eq!(selected.len(), peers.len());
        let set: HashSet<PeerIdentity> = selected.iter().cloned().collect();
        assert_eq!(set.len(), peers.len());
    }
}
