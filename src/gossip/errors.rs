//! Typed error hierarchy for the gossip subsystem.
//!
//! Prior to this module, gossip-layer failures were represented as raw
//! `String`s or by overloading `Result<(), PeerIdentity>` to signal a banned
//! peer. `GossipError` replaces those ad-hoc conventions with a single,
//! structured error type that callers can match on and that can grow to
//! describe future failure modes (queue overflow, codec errors, …) without
//! breaking the API.

use crate::peer::identity::PeerIdentity;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum GossipError {
    /// The signature on an incoming envelope failed verification.
    /// Contains the identity of the offending peer.
    #[error("invalid envelope signature from peer {peer}")]
    InvalidSignature { peer: PeerIdentity },

    /// The peer has exceeded the strike threshold and must be banned.
    #[error("peer {peer} exceeded strike limit and was banned")]
    PeerBanned { peer: PeerIdentity },

    /// The Normal-tier queue has reached its capacity cap.
    /// The oldest envelope was dropped to make room.
    #[error("normal queue overflow: dropped envelope {dropped_id:?}")]
    NormalQueueOverflow { dropped_id: [u8; 32] },

    /// The Low-tier queue has reached its capacity cap.
    #[error("low queue overflow: dropped envelope {dropped_id:?}")]
    LowQueueOverflow { dropped_id: [u8; 32] },

    /// An envelope failed structural validation before being enqueued.
    #[error("envelope validation failed: {reason}")]
    InvalidEnvelope { reason: String },

    /// A serialization or deserialization error in the gossip codec.
    #[error("codec error: {0}")]
    CodecError(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gossip_error_display() {
        let peer = PeerIdentity::new([0xAB; 32]);
        let err = GossipError::PeerBanned { peer };
        assert!(err.to_string().contains("banned"));
    }

    #[test]
    fn test_gossip_error_invalid_signature_display() {
        let peer = PeerIdentity::new([0xCD; 32]);
        let err = GossipError::InvalidSignature { peer };
        assert!(err.to_string().contains("signature"));
    }
}
