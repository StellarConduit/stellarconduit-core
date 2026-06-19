use clap::Parser;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use stellarconduit_core::gossip::protocol::{run_gossip_loop, GossipState};
use stellarconduit_core::gossip::round::GossipScheduler;
use stellarconduit_core::gossip::strike_tracker::StrikeTracker;
use stellarconduit_core::persistence::db::MeshDatabase;
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
        if args.seed.is_some() { "provided" } else { "random" }
    );

    // Shared shutdown token — child tokens propagate cancellation from the root.
    let shutdown = CancellationToken::new();

    // TODO: 1. Generate or load Identity (Issue #2) from seed

    // 2. Initialize Database
    let db = MeshDatabase::init(&args.db_path).await?;

    // 3. TODO: Start StatePruner (Issue #19)

    // 4. Set up TransportManager
    let transport_manager = Arc::new(Mutex::new(TransportManager::new(TransportPreference::Auto)));

    // 5. Spin up Gossip Event Loop — keep the JoinHandle so we can await it on shutdown.
    let gossip_state = Arc::new(Mutex::new(GossipState::new()));
    // TODO: wire up real PeerList and TopologyEventBus (Issue #2 / #19)
    let peer_list = Arc::new(Mutex::new(
        stellarconduit_core::discovery::peer_list::PeerList::new(300),
    ));
    let gossip_shutdown = shutdown.child_token();
    let gossip_handle = tokio::spawn(run_gossip_loop(
        GossipScheduler::new(),
        StrikeTracker::new(),
        gossip_state.clone(),
        peer_list,
        Arc::clone(&transport_manager),
        None,
        gossip_shutdown,
    ));

    log::info!("Daemon initialized. Press CTRL+C to shutdown.");

    // Block until CTRL+C or SIGTERM.
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;

    log::info!("Shutting down daemon...");

    // Signal all child tasks.
    shutdown.cancel();

    // Flush pending envelopes to connected peers — best effort, 2 s deadline.
    let _ = tokio::time::timeout(Duration::from_secs(2), flush_pending_messages(&gossip_state, &transport_manager)).await;

    // Wait for the gossip loop to exit cleanly (max 3 s).
    let _ = tokio::time::timeout(Duration::from_secs(3), gossip_handle).await;

    // Disconnect all transport connections.
    transport_manager.lock().await.disconnect_all().await;

    // Checkpoint WAL and close the database.
    db.close().await.ok();

    log::info!("Daemon exited cleanly.");
    Ok(())
}

/// Drain `GossipState::active_queue` and forward every envelope to all connected peers.
async fn flush_pending_messages(
    gossip_state: &Arc<Mutex<GossipState>>,
    transport_manager: &Arc<Mutex<TransportManager>>,
) {
    use stellarconduit_core::message::types::ProtocolMessage;
    use stellarconduit_core::peer::identity::PeerIdentity;

    let envelopes: Vec<_> = {
        let mut state = gossip_state.lock().await;
        let mut out = Vec::new();
        while let Some(msg) = state.active_queue.pop() {
            out.push(msg);
        }
        out
    };

    if envelopes.is_empty() {
        return;
    }

    let peers: Vec<PeerIdentity> = transport_manager
        .lock()
        .await
        .connected_peers()
        .into_iter()
        .map(PeerIdentity::new)
        .collect();

    if peers.is_empty() {
        return;
    }

    let mut transport = transport_manager.lock().await;
    for msg in envelopes {
        if let ProtocolMessage::Transaction(_) = &msg {
            for peer in &peers {
                let _ = tokio::time::timeout(
                    Duration::from_millis(500),
                    transport.send_to(peer, msg.clone()),
                )
                .await;
            }
        }
    }
}
