//! WiFi-Direct peer discovery module for StellarConduit.
//!
//! Provides `MdnsAdvertiser` (broadcasts this device's identity via mDNS) and `MdnsScanner`
//! (passively listens for mDNS service advertisements from nearby StellarConduit devices
//! on the WiFi-Direct P2P subnet and maintains the `PeerList`).
//!
//! When a WiFi-Direct P2P group is formed, devices land on a private 192.168.x.x subnet.
//! mDNS allows each device to broadcast its presence with its identity payload and TCP port,
//! so other devices can connect without knowing the IP address in advance.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::discovery::errors::DiscoveryError;
use crate::discovery::peer_list::PeerList;
use crate::peer::identity::PeerIdentity;

/// mDNS service type for StellarConduit peer discovery.
pub const MDNS_SERVICE_TYPE: &str = "_stellarconduit._tcp.local.";

// ─── MdnsAdvertiser ────────────────────────────────────────────────────────────

/// Advertises this node's identity via mDNS so nearby `MdnsScanner` instances can discover it.
///
/// Encodes the local `PeerIdentity` pubkey and capability flags into TXT records
/// and registers a service of type `MDNS_SERVICE_TYPE` on the local subnet.
pub struct MdnsAdvertiser {
    service_daemon: mdns_sd::ServiceDaemon,
    service_name: String,
}

impl MdnsAdvertiser {
    /// Start advertising this node's TCP port and Ed25519 pubkey on the local subnet.
    ///
    /// The service is registered with TXT records:
    /// - `pubkey=<hex>`: The Ed25519 public key as a hex string
    /// - `is_relay=<0|1>`: Whether this node is a relay (0 or 1)
    ///
    /// # Arguments
    /// * `port` - TCP port on which this node is listening
    /// * `identity` - This node's peer identity (contains the Ed25519 pubkey)
    /// * `is_relay` - Whether this node acts as a relay
    ///
    /// # Returns
    /// Returns `Ok(MdnsAdvertiser)` on success, or `DiscoveryError` if service registration fails.
    pub fn start(
        port: u16,
        identity: PeerIdentity,
        is_relay: bool,
    ) -> Result<Self, DiscoveryError> {
        // Create the mDNS service daemon
        let daemon = mdns_sd::ServiceDaemon::new().map_err(|e| {
            DiscoveryError::MdnsError(format!("Failed to create service daemon: {}", e))
        })?;

        // Build TXT records: pubkey=<hex>, is_relay=<0|1>
        let pubkey_hex = identity.display_id.clone();
        let is_relay_str = if is_relay { "1" } else { "0" };

        // Create a unique service name (using the first 16 chars of the pubkey hex)
        let service_name = format!("StellarConduit-{}", &identity.display_id[..16]);

        // Build the service info - ServiceInfo::new takes 6 arguments:
        // service_type, instance_name, host_name, ip_addr, port, properties
        let host_name = format!("{}.local.", service_name);

        // Create TXT properties - mdns-sd accepts HashMap<String, String> which implements IntoTxtProperties
        let mut txt_properties = HashMap::new();
        txt_properties.insert("pubkey".to_string(), pubkey_hex);
        txt_properties.insert("is_relay".to_string(), is_relay_str.to_string());

        // Use localhost IP - mdns-sd will use this or auto-detect
        // &[IpAddr] implements AsIpAddrs
        let local_ip = std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1));
        let local_ips: &[std::net::IpAddr] = &[local_ip];

        let service_info = mdns_sd::ServiceInfo::new(
            MDNS_SERVICE_TYPE,
            &service_name,
            &host_name,
            local_ips,
            port,
            txt_properties,
        )
        .map_err(|e| {
            DiscoveryError::ServiceRegistrationError(format!(
                "Failed to create service info: {}",
                e
            ))
        })?;

        // Register the service
        daemon.register(service_info).map_err(|e| {
            DiscoveryError::ServiceRegistrationError(format!("Failed to register service: {}", e))
        })?;

        log::info!(
            "mDNS service registered: {} on port {} (pubkey: {}, is_relay: {})",
            service_name,
            port,
            &identity.display_id[..16],
            is_relay
        );

        Ok(Self {
            service_daemon: daemon,
            service_name,
        })
    }

    /// Stop advertising this node's service.
    ///
    /// Unregisters the mDNS service and shuts down the service daemon.
    pub fn stop(&self) {
        // Unregister the service
        if let Err(e) = self.service_daemon.unregister(&self.service_name) {
            log::warn!("Failed to unregister mDNS service: {}", e);
        }
        // Shutdown the daemon
        let _ = self.service_daemon.shutdown();
        log::debug!("mDNS advertiser stopped");
    }
}

// ─── MdnsScanner ────────────────────────────────────────────────────────────────

/// Continuously scans for mDNS service advertisements from other StellarConduit devices.
///
/// When a service of type `MDNS_SERVICE_TYPE` is discovered, the scanner:
/// 1. Resolves the service to get TXT records.
/// 2. Parses the `pubkey` TXT record to extract the Ed25519 public key.
/// 3. Calls `PeerList::insert_or_update` with the peer's pubkey.
/// 4. The peer list will generate `DiscoveryEvent::PeerDiscovered` or `PeerUpdated` events.
pub struct MdnsScanner {
    #[allow(dead_code)] // Stored for potential future use (e.g., accessing peer list state)
    peer_list: Arc<Mutex<PeerList>>,
    service_daemon: mdns_sd::ServiceDaemon,
    receiver: Option<mdns_sd::Receiver<mdns_sd::ServiceEvent>>,
    _task_handle: Option<tokio::task::JoinHandle<()>>,
}

impl MdnsScanner {
    /// Start the mDNS scanner.
    ///
    /// Begins browsing for services of type `MDNS_SERVICE_TYPE` and spawns a background
    /// task to process discovered services.
    ///
    /// # Arguments
    /// * `peer_list` - Shared peer list that will be updated when peers are discovered
    ///
    /// # Returns
    /// Returns `Ok(MdnsScanner)` on success, or `DiscoveryError` if browsing fails.
    pub fn start(peer_list: Arc<Mutex<PeerList>>) -> Result<Self, DiscoveryError> {
        // Create the mDNS service daemon
        let daemon = mdns_sd::ServiceDaemon::new().map_err(|e| {
            DiscoveryError::MdnsError(format!("Failed to create service daemon: {}", e))
        })?;

        // Start browsing for services
        let receiver = daemon.browse(MDNS_SERVICE_TYPE).map_err(|e| {
            DiscoveryError::ServiceBrowseError(format!("Failed to browse services: {}", e))
        })?;

        log::info!("mDNS scanner started, browsing for {}", MDNS_SERVICE_TYPE);

        // Clone the receiver for the background task
        let receiver_clone = receiver.clone();
        let peer_list_clone = peer_list.clone();

        // Spawn a background task to process service events
        let task_handle = tokio::spawn(async move {
            Self::process_service_events(receiver_clone, peer_list_clone).await;
        });

        Ok(Self {
            peer_list,
            service_daemon: daemon,
            receiver: Some(receiver),
            _task_handle: Some(task_handle),
        })
    }

    /// Background task that processes mDNS service events.
    async fn process_service_events(
        receiver: mdns_sd::Receiver<mdns_sd::ServiceEvent>,
        peer_list: Arc<Mutex<PeerList>>,
    ) {
        loop {
            // Use async recv if available, otherwise fall back to blocking recv
            let event = match receiver.recv_async().await {
                Ok(event) => event,
                Err(_) => {
                    log::debug!("mDNS receiver closed, stopping scanner task");
                    break;
                }
            };

            match event {
                mdns_sd::ServiceEvent::ServiceResolved(info) => {
                    if let Err(e) = Self::handle_service_resolved(info, &peer_list).await {
                        log::warn!("Failed to handle resolved service: {}", e);
                    }
                }
                mdns_sd::ServiceEvent::ServiceFound(_service_type, _fullname) => {
                    // Service found but not yet resolved - we'll wait for ServiceResolved
                    log::debug!("mDNS service found, waiting for resolution");
                }
                mdns_sd::ServiceEvent::ServiceRemoved(_service_type, _fullname) => {
                    // Service removed - could trigger PeerLost event in the future
                    log::debug!("mDNS service removed");
                }
                mdns_sd::ServiceEvent::SearchStarted(_service_type) => {
                    log::debug!("mDNS search started");
                }
                mdns_sd::ServiceEvent::SearchStopped(_service_type) => {
                    log::debug!("mDNS search stopped");
                    break;
                }
            }
        }
    }

    /// Handle a resolved service event by parsing TXT records and updating the peer list.
    async fn handle_service_resolved(
        info: mdns_sd::ServiceInfo,
        peer_list: &Arc<Mutex<PeerList>>,
    ) -> Result<(), DiscoveryError> {
        // Extract TXT records - get_properties() returns TxtProperties
        let txt_props = info.get_properties();
        // Convert TxtProperty to &str for parsing - TxtProperty can be converted to string
        let txt_vec: Vec<String> = txt_props.iter().map(|tp| tp.to_string()).collect();
        let txt_strs: Vec<&str> = txt_vec.iter().map(|s| s.as_str()).collect();

        // Parse pubkey from TXT records
        let pubkey = Self::parse_pubkey_from_txt(&txt_strs)?;

        // Create PeerIdentity from the pubkey
        let identity = PeerIdentity::new(pubkey);

        // Update peer list (signal strength is not available from mDNS, use 0 as default)
        let mut list = peer_list.lock().await;
        if let Some(_event) = list.insert_or_update(pubkey, 0) {
            log::info!(
                "mDNS peer discovered/updated: {} from service {}",
                &identity.display_id[..16],
                info.get_fullname()
            );
            // The event is already generated by insert_or_update, but we could broadcast it here
            // if we had an event channel like BleScanner does
        }

        Ok(())
    }

    /// Parse the Ed25519 public key from TXT record properties.
    ///
    /// Expects a TXT record with format: `pubkey=<hex_string>` where hex_string is 64 characters.
    /// TxtProperties can be iterated to get &str values in "key=value" format.
    fn parse_pubkey_from_txt(txt_props: &[&str]) -> Result<[u8; 32], DiscoveryError> {
        // Find the pubkey TXT record - TxtProperties returns "key=value" format
        let pubkey_str = txt_props
            .iter()
            .find(|prop| prop.starts_with("pubkey="))
            .ok_or_else(|| {
                DiscoveryError::InvalidTxtRecord("Missing pubkey TXT record".to_string())
            })?
            .strip_prefix("pubkey=")
            .ok_or_else(|| {
                DiscoveryError::InvalidTxtRecord("Malformed pubkey TXT record".to_string())
            })?;

        // Parse hex string to bytes
        if pubkey_str.len() != 64 {
            return Err(DiscoveryError::InvalidTxtRecord(format!(
                "Pubkey hex string must be 64 characters, got {}",
                pubkey_str.len()
            )));
        }

        let mut pubkey = [0u8; 32];
        for (i, chunk) in pubkey_str.as_bytes().chunks(2).enumerate() {
            if i >= 32 {
                break;
            }
            let hex_byte = std::str::from_utf8(chunk).map_err(|e| {
                DiscoveryError::InvalidTxtRecord(format!("Invalid hex string: {}", e))
            })?;
            pubkey[i] = u8::from_str_radix(hex_byte, 16).map_err(|e| {
                DiscoveryError::InvalidTxtRecord(format!("Invalid hex byte: {}", e))
            })?;
        }

        Ok(pubkey)
    }

    /// Stop the mDNS scanner.
    ///
    /// Stops browsing for services and shuts down the service daemon.
    pub fn stop(&mut self) {
        // Stop browsing
        if let Err(e) = self.service_daemon.stop_browse(MDNS_SERVICE_TYPE) {
            log::warn!("Failed to stop browsing: {}", e);
        }

        // Shutdown the daemon
        let _ = self.service_daemon.shutdown();

        // Drop the receiver to signal the background task to stop
        self.receiver.take();

        log::debug!("mDNS scanner stopped");
    }
}

// ─── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(b: u8) -> [u8; 32] {
        [b; 32]
    }

    // ── TXT record parsing ───────────────────────────────────────────────────────

    #[test]
    fn parse_pubkey_from_valid_txt() {
        let identity = PeerIdentity::new(pk(0xAB));
        let txt_pubkey = format!("pubkey={}", identity.display_id);
        let txt_props = vec![txt_pubkey.as_str(), "is_relay=1"];

        let parsed = MdnsScanner::parse_pubkey_from_txt(&txt_props).unwrap();
        assert_eq!(parsed, pk(0xAB));
    }

    #[test]
    fn parse_pubkey_rejects_missing_pubkey() {
        let txt_props = vec!["is_relay=1"];
        let result = MdnsScanner::parse_pubkey_from_txt(&txt_props);
        assert!(matches!(result, Err(DiscoveryError::InvalidTxtRecord(_))));
    }

    #[test]
    fn parse_pubkey_rejects_malformed_hex() {
        let txt_props = vec!["pubkey=invalid_hex_string"];
        let result = MdnsScanner::parse_pubkey_from_txt(&txt_props);
        assert!(matches!(result, Err(DiscoveryError::InvalidTxtRecord(_))));
    }

    #[test]
    fn parse_pubkey_rejects_wrong_length() {
        let txt_props = vec!["pubkey=abcd"]; // Too short
        let result = MdnsScanner::parse_pubkey_from_txt(&txt_props);
        assert!(matches!(result, Err(DiscoveryError::InvalidTxtRecord(_))));
    }

    // ── MdnsAdvertiser ──────────────────────────────────────────────────────────

    #[test]
    fn advertiser_registers_service() {
        let identity = PeerIdentity::new(pk(0x42));
        let advertiser = MdnsAdvertiser::start(8080, identity, false);

        // This test may fail if mDNS daemon cannot be created (e.g., in CI without network)
        // So we check if it's an error due to daemon creation vs other errors
        match advertiser {
            Ok(adv) => {
                adv.stop();
            }
            Err(DiscoveryError::MdnsError(_)) => {
                // Expected in environments without mDNS support
                log::warn!("mDNS daemon creation failed (expected in some test environments)");
            }
            Err(e) => {
                panic!("Unexpected error: {:?}", e);
            }
        }
    }

    // ── MdnsScanner ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn scanner_starts_browsing() {
        let peer_list = Arc::new(Mutex::new(PeerList::new(300)));
        let scanner_result = MdnsScanner::start(peer_list);

        // This test may fail if mDNS daemon cannot be created (e.g., in CI without network)
        match scanner_result {
            Ok(mut scanner) => {
                scanner.stop();
            }
            Err(DiscoveryError::MdnsError(_)) => {
                // Expected in environments without mDNS support
                log::warn!("mDNS daemon creation failed (expected in some test environments)");
            }
            Err(e) => {
                panic!("Unexpected error: {:?}", e);
            }
        }
    }

    // ── Integration test with mock service ──────────────────────────────────────

    #[tokio::test]
    async fn scanner_discovers_advertised_service() {
        // This is a more complex integration test that would require:
        // 1. Starting an advertiser
        // 2. Starting a scanner
        // 3. Waiting for the service to be discovered
        // 4. Verifying the peer was added to the peer list
        //
        // However, this requires actual mDNS networking which may not be available
        // in all test environments. The mdns-sd crate provides test utilities,
        // but they may require additional setup.
        //
        // For now, we test the parsing logic separately above.
        // A full integration test would be:
        //
        // let identity = PeerIdentity::new(pk(0x99));
        // let advertiser = MdnsAdvertiser::start(8080, identity.clone(), false)?;
        // let peer_list = Arc::new(Mutex::new(PeerList::new(300)));
        // let mut scanner = MdnsScanner::start(peer_list.clone())?;
        //
        // // Wait for discovery
        // tokio::time::sleep(Duration::from_secs(2)).await;
        //
        // let list = peer_list.lock().await;
        // assert!(list.get_active_peers().iter().any(|p| p.identity.pubkey == identity.pubkey));
        //
        // scanner.stop();
        // advertiser.stop();
    }
}
