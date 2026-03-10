use crate::peer::identity::PeerIdentity;

pub struct Peer {
    pub identity: PeerIdentity,
    /// Reputation score: 0–100. Drops to 0 triggers a ban.
    pub reputation: u32,
    pub is_banned: bool,
    /// Unix timestamp when the ban expires (0 if not banned)
    pub ban_expires_at_unix_sec: u64,
    /// Unix timestamp of the last observed activity from this peer
    pub last_seen_unix_sec: u64,
    /// Bitmask: 0x01 = BLE, 0x02 = WiFi-Direct
    pub supported_transports: u8,
    pub is_relay_node: bool,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

impl Peer {
    pub fn new(pubkey: [u8; 32]) -> Self {
        Self {
            identity: PeerIdentity::new(pubkey),
            reputation: 100,
            is_banned: false,
            ban_expires_at_unix_sec: 0,
            last_seen_unix_sec: 0,
            supported_transports: 0,
            is_relay_node: false,
            bytes_sent: 0,
            bytes_received: 0,
        }
    }

    /// Ban this peer for the specified duration (in seconds)
    pub fn ban(&mut self, duration_sec: u64) {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.is_banned = true;
        self.ban_expires_at_unix_sec = now + duration_sec;
    }

    /// Check if ban has expired and unban if so
    pub fn check_ban_expiration(&mut self) -> bool {
        if !self.is_banned {
            return false;
        }
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if now >= self.ban_expires_at_unix_sec {
            self.is_banned = false;
            self.ban_expires_at_unix_sec = 0;
            return true;
        }
        false
    }
}
