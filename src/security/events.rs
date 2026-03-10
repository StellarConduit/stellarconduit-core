use crate::peer::identity::PeerIdentity;

/// Security-related events emitted when peers violate rate limits or other security policies.
#[derive(Clone, Debug)]
pub enum SecurityEvent {
    /// A peer has exceeded rate limits consistently and should be banned.
    /// Contains the peer identity and the reason for the violation.
    PeerViolation {
        peer: PeerIdentity,
        reason: ViolationReason,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ViolationReason {
    /// Peer exceeded the rate limit threshold for their transport type
    RateLimitExceeded,
}
