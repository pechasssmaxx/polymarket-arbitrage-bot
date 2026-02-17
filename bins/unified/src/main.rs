//! Moonbag Unified Binary
//!
//! Combined scanner + executor with tokio channels for lowest latency.
//! This is the recommended production deployment.

use clap::Parser;
use moonbag_core::Config;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[derive(Parser, Debug)]
#[command(name = "moonbag")]
#[command(about = "Polymarket moonbag trading bot (scanner + executor)")]
struct Args {
    /// Path to config file
    #[arg(short, long)]
    config: Option<String>,

    /// Enable dry-run mode (no actual trades)
    #[arg(long, default_value = "false")]
    dry_run: bool,

    /// Log level (debug, info, warn, error)
    #[arg(long, default_value = "info")]
    log_level: String,

    /// Number of analyzer workers
    #[arg(long, default_value = "4")]
    analyzer_workers: usize,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load environment
    dotenvy::dotenv().ok();

    let args = Args::parse();

    // Initialize tracing
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&args.log_level));

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .init();

    info!("Starting moonbag unified bot...");

    // Load config
    let config = Config::from_env()?;
    info!(
        safe_address = %config.polymarket_funder,
        min_profit = %config.min_profit,
        max_investment = %config.max_investment,
        dry_run = args.dry_run,
        analyzer_workers = args.analyzer_workers,
        "Config loaded"
    );

    // TODO: Create channels for pipeline
    // let (orderbook_tx, orderbook_rx) = tokio::sync::mpsc::channel(1000);
    // let (opportunity_tx, opportunity_rx) = tokio::sync::mpsc::channel(100);

    // TODO: Spawn components
    // 1. WebSocket task - receives orderbook updates
    // 2. Analyzer workers - detect opportunities
    // 3. Executor task - execute trades

    // Architecture:
    // WebSocket → DashMap (orderbook state)
    //          → Analyzer Workers (read from DashMap)
    //          → Opportunity Channel
    //          → Executor (sequential execution)

    info!("Unified bot started. Press Ctrl+C to stop.");

    // Wait for shutdown signal
    tokio::signal::ctrl_c().await?;
    info!("Shutting down...");

    Ok(())
}
