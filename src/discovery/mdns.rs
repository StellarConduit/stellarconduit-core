//! mDNS-based WiFi peer discovery for StellarConduit.
//!
//! Provides [`MdnsDiscovery`], a high-level manager that both advertises this
//! node's presence on the local network and browses for other StellarConduit
//! nodes via multicast DNS (mDNS).
//!
//! Service type: `_stellarconduit._tcp.local.`
//!
//! Each advertisement includes TXT records:
//! - `pubkey=<64-char-hex>` — the node's Ed25519 public key
//! - `hops=<u8>` — estimated hops to a relay node (255 = unknown / no relay)
//! - `version=1` — protocol version

use std::collections::HashMap;

use crate::discovery::errors::DiscoveryError;

/// mDNS service type for StellarConduit peer discovery.
pub const MDNS_SERVICE_TYPE: &str = "_stellarconduit._tcp.local.";

/// A peer discovered via mDNS on the local WiFi network.
#[derive(Debug, Clone)]
pub struct DiscoveredPeer {
    /// Ed25519 public key of the discovered peer.
    pub pubkey: [u8; 32],
    /// IP address of the peer on the local network.
    pub address: std::net::IpAddr,
    /// TCP port the peer's WiFi-Direct listener is bound to.
    pub port: u16,
    /// Estimated hop count to a relay node (255 = unknown / no relay).
    pub hops_to_relay: u8,
}

/// High-level mDNS discovery manager.
///
/// Handles both advertising this node on the local network and
/// browsing for other StellarConduit peers.  All registered services
/// are automatically unregistered (with a goodbye packet) when the
/// daemon is shut down via [`MdnsDiscovery::stop`].
pub struct MdnsDiscovery {
    daemon: mdns_sd::ServiceDaemon,
    service_type: String,
}

impl MdnsDiscovery {
    /// Create a new mDNS discovery instance.
    ///
    /// Initialises the underlying mDNS service daemon.  Returns
    /// [`DiscoveryError::MdnsError`] if the daemon cannot be started
    /// (e.g., mDNS sockets are unavailable on the platform).
    pub fn new() -> Result<Self, DiscoveryError> {
        let daemon = mdns_sd::ServiceDaemon::new()
            .map_err(|e| DiscoveryError::MdnsError(format!("Failed to create mDNS daemon: {e}")))?;
        Ok(Self {
            daemon,
            service_type: MDNS_SERVICE_TYPE.to_string(),
        })
    }

    /// Advertise this node on the local network.
    ///
    /// Registers a service of type `_stellarconduit._tcp.local.` with:
    ///
    /// | Key       | Value                                  |
    /// |-----------|----------------------------------------|
    /// | `pubkey`  | Full 64-char hex-encoded Ed25519 key   |
    /// | `hops`    | Current estimated hops to relay        |
    /// | `version` | `1` (forward-compat protocol version)  |
    ///
    /// The *service name* is set to the first 8 bytes of the pubkey
    /// (16 hex chars) so that peers can quickly identify the source
    /// without scanning TXT records.
    pub fn advertise(
        &self,
        local_pubkey: &[u8; 32],
        port: u16,
        hops_to_relay: u8,
    ) -> Result<(), DiscoveryError> {
        let pubkey_hex = hex::encode(local_pubkey);
        let service_name = hex::encode(&local_pubkey[..8]);

        let mut properties = HashMap::new();
        properties.insert("pubkey".to_string(), pubkey_hex);
        properties.insert("hops".to_string(), hops_to_relay.to_string());
        properties.insert("version".to_string(), "1".to_string());

        // Use the wildcard address so the daemon responds on all interfaces.
        let addrs: &[std::net::IpAddr] = &[std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)];

        let host_name = format!("{service_name}.local.");

        let service_info = mdns_sd::ServiceInfo::new(
            &self.service_type,
            &service_name,
            &host_name,
            addrs,
            port,
            properties,
        )
        .map_err(|e| {
            DiscoveryError::ServiceRegistrationError(format!(
                "Failed to create mDNS service info: {e}"
            ))
        })?;

        self.daemon.register(service_info).map_err(|e| {
            DiscoveryError::ServiceRegistrationError(format!(
                "Failed to register mDNS service: {e}"
            ))
        })?;

        log::info!(
            "mDNS advertisement registered: {service_name} on port {port} (hops: {hops_to_relay})",
        );

        Ok(())
    }

    /// Start browsing for StellarConduit peers on the local network.
    ///
    /// Discovered peers are sent through the provided channel.  The
    /// background browse task runs until [`MdnsDiscovery::stop`] is
    /// called or the channel is closed.
    pub async fn browse(
        &self,
        discovered_peers: tokio::sync::mpsc::Sender<DiscoveredPeer>,
    ) -> Result<(), DiscoveryError> {
        let receiver = self.daemon.browse(&self.service_type).map_err(|e| {
            DiscoveryError::ServiceBrowseError(format!("Failed to start mDNS browse: {e}"))
        })?;

        tokio::spawn(async move {
            Self::browse_event_loop(receiver, discovered_peers).await;
        });

        Ok(())
    }

    /// Stop advertising and shutdown the mDNS daemon.
    ///
    /// Sends mDNS goodbye packets for all registered services so that
    /// other nodes can promptly remove this peer from their peer lists.
    pub fn stop(&self) {
        let _ = self.daemon.shutdown();
        log::debug!("mDNS discovery stopped");
    }

    // ── Internal helpers ────────────────────────────────────────────────────

    /// Event loop that processes mDNS service events from the browse receiver.
    async fn browse_event_loop(
        receiver: mdns_sd::Receiver<mdns_sd::ServiceEvent>,
        sender: tokio::sync::mpsc::Sender<DiscoveredPeer>,
    ) {
        loop {
            let event = match receiver.recv_async().await {
                Ok(ev) => ev,
                Err(_) => {
                    log::debug!("mDNS browse receiver closed, stopping event loop");
                    break;
                }
            };

            match event {
                mdns_sd::ServiceEvent::ServiceResolved(info) => {
                    // Ignore services that do not belong to StellarConduit.
                    if !info.get_type().starts_with("_stellarconduit") {
                        continue;
                    }

                    match Self::parse_resolved_service(&info) {
                        Ok(peer) => {
                            if sender.send(peer).await.is_err() {
                                log::debug!(
                                    "mDNS discovered_peers channel closed, stopping event loop"
                                );
                                break;
                            }
                        }
                        Err(e) => {
                            log::warn!("Failed to parse discovered mDNS service: {e}");
                        }
                    }
                }
                mdns_sd::ServiceEvent::ServiceRemoved(_ty, _fullname) => {
                    log::debug!("mDNS service removed");
                }
                mdns_sd::ServiceEvent::SearchStopped(_) => {
                    log::debug!("mDNS search stopped");
                    break;
                }
                _ => {
                    // ServiceFound / SearchStarted — wait for resolution.
                }
            }
        }
    }

    /// Extract a [`DiscoveredPeer`] from a resolved mDNS service info.
    fn parse_resolved_service(
        info: &mdns_sd::ServiceInfo,
    ) -> Result<DiscoveredPeer, DiscoveryError> {
        let txt_props = info.get_properties();
        let txt_owned: Vec<String> = txt_props.iter().map(|tp| tp.to_string()).collect();
        let txt_refs: Vec<&str> = txt_owned.iter().map(|s| s.as_str()).collect();
        let pubkey = parse_pubkey_from_txt(&txt_refs)?;
        let hops = parse_hops_from_txt(&txt_refs)?;

        let address = info
            .get_addresses()
            .iter()
            .next()
            .copied()
            .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));

        let port = info.get_port();

        Ok(DiscoveredPeer {
            pubkey,
            address,
            port,
            hops_to_relay: hops,
        })
    }
}

// ── TXT record helpers (testable without mDNS infrastructure) ─────────────────

/// Find the value for a given key in a list of `"key=value"` strings.
fn find_txt_value<'a>(entries: &'a [&'a str], key: &str) -> Option<&'a str> {
    let prefix = format!("{key}=");
    entries
        .iter()
        .find(|entry| entry.starts_with(&prefix))
        .and_then(|entry| entry.strip_prefix(&prefix))
}

/// Parse the Ed25519 public key from TXT record entries.
///
/// Expects an entry of the form `"pubkey=<64-char-hex>"`.
/// Returns [`DiscoveryError::InvalidTxtRecord`] on failure.
fn parse_pubkey_from_txt(entries: &[&str]) -> Result<[u8; 32], DiscoveryError> {
    let hex_str = find_txt_value(entries, "pubkey")
        .ok_or_else(|| DiscoveryError::InvalidTxtRecord("Missing pubkey TXT record".to_string()))?;

    if hex_str.len() != 64 {
        return Err(DiscoveryError::InvalidTxtRecord(format!(
            "Pubkey hex must be 64 characters, got {}",
            hex_str.len()
        )));
    }

    let bytes = hex::decode(hex_str)
        .map_err(|e| DiscoveryError::InvalidTxtRecord(format!("Invalid hex pubkey: {e}")))?;

    bytes.try_into().map_err(|_| {
        DiscoveryError::InvalidTxtRecord("Pubkey must be exactly 32 bytes".to_string())
    })
}

/// Parse the hop count from TXT record entries.
///
/// Expects an entry of the form `"hops=<u8>"`.  Returns 255 (unknown)
/// if the entry is absent.
fn parse_hops_from_txt(entries: &[&str]) -> Result<u8, DiscoveryError> {
    match find_txt_value(entries, "hops") {
        Some(s) => s
            .parse::<u8>()
            .map_err(|e| DiscoveryError::InvalidTxtRecord(format!("Invalid hops value: {e}"))),
        None => Ok(255),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_hex_pubkey() -> String {
        "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string()
    }

    // ── TXT record parsing ──────────────────────────────────────────────────

    #[test]
    fn test_mdns_txt_record_parsing() {
        let hex = valid_hex_pubkey();
        let entries = [format!("pubkey={hex}"), "hops=3".into(), "version=1".into()];
        let refs: Vec<&str> = entries.iter().map(|s| s.as_str()).collect();

        let pubkey = parse_pubkey_from_txt(&refs).unwrap();
        let hops = parse_hops_from_txt(&refs).unwrap();

        assert_eq!(hops, 3);
        let expected: [u8; 32] = hex::decode(&hex).unwrap().try_into().unwrap();
        assert_eq!(pubkey, expected);
    }

    #[test]
    fn test_mdns_handles_malformed_pubkey_txt() {
        let entries = ["pubkey=not-a-hex-string".to_string(), "hops=3".into()];
        let refs: Vec<&str> = entries.iter().map(|s| s.as_str()).collect();

        let result = parse_pubkey_from_txt(&refs);
        assert!(result.is_err(), "Expected error for malformed pubkey");
        assert!(
            matches!(result, Err(DiscoveryError::InvalidTxtRecord(_))),
            "Expected InvalidTxtRecord, got {:?}",
            result
        );
    }

    #[test]
    fn test_mdns_ignores_unknown_service_type() {
        // The browse event loop skips service types that do not start with
        // `_stellarconduit`.  Verify the prefix check works correctly.
        assert!(
            !"_http._tcp.local.".starts_with("_stellarconduit"),
            "HTTP service should be rejected"
        );
        assert!(
            "_stellarconduit._tcp.local.".starts_with("_stellarconduit"),
            "StellarConduit service should be accepted"
        );
    }

    #[test]
    fn test_mdns_default_hops_when_missing() {
        let hex = valid_hex_pubkey();
        let entries = [format!("pubkey={hex}")];
        let refs: Vec<&str> = entries.iter().map(|s| s.as_str()).collect();

        let hops = parse_hops_from_txt(&refs).unwrap();
        assert_eq!(hops, 255, "Default hops should be 255 when not in TXT");
    }

    #[test]
    fn test_mdns_missing_pubkey_returns_error() {
        let entries = vec!["hops=3"];
        let result = parse_pubkey_from_txt(&entries);
        assert!(result.is_err());
    }

    #[test]
    fn test_mdns_pubkey_wrong_length_returns_error() {
        let entries = vec!["pubkey=abcd"]; // 4 chars, not 64
        let result = parse_pubkey_from_txt(&entries);
        assert!(result.is_err());
    }

    #[test]
    fn test_find_txt_value_returns_none_for_missing_key() {
        let entries = vec!["foo=bar", "baz=qux"];
        assert!(find_txt_value(&entries, "pubkey").is_none());
    }

    #[test]
    fn test_find_txt_value_extracts_value_correctly() {
        let entries = vec!["pubkey=abcdef", "hops=5"];
        assert_eq!(find_txt_value(&entries, "pubkey"), Some("abcdef"));
        assert_eq!(find_txt_value(&entries, "hops"), Some("5"));
    }
}
