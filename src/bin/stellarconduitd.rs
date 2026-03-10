use clap::Parser;

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
    // TODO: 2. Initialize Database (Issue #18)
    // TODO: 3. Start StatePruner (Issue #19)
    // TODO: 4. Start TransportManager with WiFi-Direct listener (TCP) on `args.port`
    // TODO: 5. Start Gossip Event Loop

    log::info!("Daemon initialized. Press CTRL+C to shutdown.");

    // Block forever waiting for CTRL+C
    tokio::signal::ctrl_c().await?;
    log::info!("Shutting down daemon...");
    Ok(())
}
