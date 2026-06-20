use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::Mutex;

use clap::Parser;

use stellarconduit_core::discovery::mdns::{DiscoveredPeer, MdnsDiscovery};
use stellarconduit_core::discovery::peer_list::PeerList;
use stellarconduit_core::transport::unified::{TransportManager, TransportPreference};

#[derive(Parser, Debug)]
#[command(version, about = "StellarConduit Core Daemon - Run a local mesh node", long_about = None)]
struct Args {
    /// Port to listen on for Virtual WiFi-Direct (TCP) connections
    #[arg(short, long, default_value_t = 8080)]
    port: u16,

    /// Is this node a gateway to the internet?
    #[arg(short, long, default_value_t = false)]
    is_relay: bool,

    /// Path to the SQLite database
    #[arg(short, long, default_value = "./stellarconduit.db")]
    db_path: String,

    /// Optional: Secret seed to generate consistent Ed25519 identity
    /// (random if omitted)
    #[arg(short, long)]
    seed: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    let args = Args::parse();

    log::info!("Starting StellarConduit Core Daemon...");
    log::info!("Configuration:");
    log::info!("  Port: {}", args.port);
    log::info!("  Relay Node: {}", args.is_relay);
    log::info!("  Database: {}", args.db_path);
    log::info!(
        "  Identity Seed: {}",
        if args.seed.is_some() {
            "provided"
        } else {
            "random"
        }
    );

    // TODO: 1. Generate or load Identity (Issue #2) from seed
    // For now, generate a random ephemeral identity — real identity loading
    // will be wired in a follow-up issue.
    let local_pubkey: [u8; 32] = rand::random();

    // TODO: 2. Initialize Database (Issue #18)
    // TODO: 3. Start StatePruner (Issue #19)

    // 4. Start TransportManager with WiFi-Direct listener (TCP) on `args.port`
    let transport_manager = Arc::new(Mutex::new(TransportManager::new(TransportPreference::Auto)));

    let (incoming_tx, mut incoming_rx) = mpsc::channel(64);

    let wifi_addr = {
        let mgr = transport_manager.lock().await;
        mgr.start_wifi_listener(args.port, incoming_tx).await?
    };
    log::info!("WiFi-Direct listener bound on {wifi_addr}");

    let tm_inbound = transport_manager.clone();
    tokio::spawn(async move {
        while let Some((peer, conn)) = incoming_rx.recv().await {
            tm_inbound.lock().await.register_inbound(peer, conn);
        }
    });

    let peer_list = Arc::new(Mutex::new(PeerList::new(300)));

    // 5. Start mDNS discovery
    let mdns = MdnsDiscovery::new()?;
    // 255 = unknown hop count; will be updated via topology events
    mdns.advertise(&local_pubkey, args.port, 255)?;

    let (discovered_tx, mut discovered_rx) = mpsc::channel::<DiscoveredPeer>(64);
    mdns.browse(discovered_tx).await?;

    // Spawn a background task that connects newly discovered peers
    let peer_list_clone = peer_list.clone();
    tokio::spawn(async move {
        while let Some(peer) = discovered_rx.recv().await {
            let mut list = peer_list_clone.lock().await;
            if list.get_peer(&peer.pubkey).is_none() {
                let addr = SocketAddr::new(peer.address, peer.port);
                log::info!(
                    "Discovered new peer via mDNS: {} at {addr} (hops: {})",
                    hex::encode(&peer.pubkey[..8]),
                    peer.hops_to_relay,
                );
                list.insert_or_update(peer.pubkey, 0);
                if let Some(p) = list.get_peer_mut(&peer.pubkey) {
                    p.is_relay_node = peer.hops_to_relay < 255;
                }
                // TODO: connect via TransportManager once identity is fully wired
            }
        }
    });

    // TODO: 6. Start Gossip Event Loop
    // TODO: 7. Subscribe to TopologyEvent::RelayReachabilityGained / Lost and
    //    re-advertise with updated TXT records when hops-to-relay changes.

    log::info!("Daemon initialized. Press CTRL+C to shutdown.");

    // Block forever waiting for CTRL+C
    tokio::signal::ctrl_c().await?;
    mdns.stop();
    log::info!("Shutting down daemon...");
    Ok(())
}
