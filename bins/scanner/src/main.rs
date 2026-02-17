//! Moonbag Scanner Binary - 24/7 Operation
//!
//! Monitors WebSocket orderbook feeds and detects moonbag opportunities.
//!
//! Architecture:
//! 1. Initialize redb store for persistence (LKG + convert_index cache)
//! 2. Load markets from Gamma API (with LKG fallback)
//! 3. Bootstrap orderbooks via REST (POST /books)
//! 4. Connect to WebSocket with dynamic resubscribe
//! 5. Gamma API poller (every 5 min) for market discovery
//! 6. Market reconciler handles additions/removals
//! 7. Health server on :8080 (/health, /ready, /metrics)

use clap::Parser;
use moonbag_analyzer::quick_check_with_best_profit;
use moonbag_core::models::{ExecutionResult, MarketState, MoonbagOpportunity, StoredMarket};
use moonbag_core::Config;
use moonbag_executor::Executor;
use moonbag_notifier::TelegramNotifier;
use moonbag_scanner::{
    AsyncStateStore, BootstrapConfig, Bootstrapper, GammaApiClient, GammaApiConfig, HealthServer,
    HealthState, MarketReconciler, OrderbookManager, RedbStore, Scanner, ScannerConfig, WsClient,
    WsControl, WsUpdate, WsUpdateKind,
};
use rust_decimal::Decimal;
use std::collections::{hash_map::DefaultHasher, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[derive(Parser, Debug)]
#[command(name = "moonbag-scanner")]
#[command(about = "Polymarket moonbag opportunity scanner - 24/7 operation")]
struct Args {
    /// Path to markets JSON file (optional, uses Gamma API if not provided)
    #[arg(short, long)]
    markets: Option<String>,

    /// Dry-run mode (prevents live trading). Defaults to `MOONBAG_DRY_RUN` (true if unset).
    ///
    /// Examples:
    /// - `--dry-run false` (force live)
    /// - `--dry-run true`  (force dry-run)
    #[arg(long)]
    dry_run: Option<bool>,

    /// Log level (debug, info, warn, error)
    #[arg(long, default_value = "info")]
    log_level: String,

    /// Scan interval in milliseconds (periodic full scan)
    #[arg(long, default_value = "5000")]
    scan_interval_ms: u64,

    /// Skip WebSocket, just do periodic REST scans
    #[arg(long, default_value = "false")]
    rest_only: bool,

    /// Skip initial bootstrap
    #[arg(long, default_value = "false")]
    skip_bootstrap: bool,

    /// Health server bind address
    #[arg(long, default_value = "0.0.0.0:8080")]
    health_bind: String,

    /// Data directory for redb store
    #[arg(long, default_value = "./data")]
    data_dir: String,

    /// Gamma API refresh interval in seconds
    #[arg(long, default_value = "300")]
    gamma_refresh_secs: u64,

    /// Disable Telegram notifications (use separate notifier process instead)
    #[arg(long, default_value = "false")]
    no_telegram: bool,

    /// Enable execution (scanner + executor mode)
    /// When enabled, detected opportunities are sent to executor for trading
    #[arg(long, default_value = "false")]
    execute: bool,

    /// Max concurrent executions (queue size)
    #[arg(long, default_value = "1")]
    max_concurrent_executions: usize,

    /// Execute only ONE opportunity then exit (for testing)
    #[arg(long, default_value = "false")]
    once: bool,
}

#[derive(Debug, Clone)]
struct LastTelegramNotification {
    signature: u64,
    profit: Decimal,
    sent_at: Instant,
}

/// Per-market Telegram de-duplication to avoid spamming identical opportunities.
///
/// - Sends immediately for a new signature (different selection set / size).
/// - For the same signature, only sends if profit changed "meaningfully" AND a cooldown has passed.
struct TelegramDeduper {
    per_market: HashMap<String, LastTelegramNotification>,
    cooldown: Duration,
    min_profit_delta: Decimal,
}

impl TelegramDeduper {
    fn from_env() -> Self {
        let cooldown_secs = std::env::var("MOONBAG_TELEGRAM_DEDUP_COOLDOWN_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(60);

        let min_profit_delta = std::env::var("MOONBAG_TELEGRAM_DEDUP_MIN_PROFIT_DELTA")
            .ok()
            .and_then(|v| v.parse::<Decimal>().ok())
            .unwrap_or_else(|| Decimal::new(1, 2)); // 0.01

        Self {
            per_market: HashMap::new(),
            cooldown: Duration::from_secs(cooldown_secs),
            min_profit_delta,
        }
    }

    fn should_send(&mut self, opp: &moonbag_core::models::MoonbagOpportunity) -> bool {
        let key = opp.market_slug.clone();
        let now = Instant::now();
        let signature = opportunity_signature(opp);
        let profit = opp.guaranteed_profit;

        let Some(last) = self.per_market.get_mut(&key) else {
            self.per_market.insert(
                key,
                LastTelegramNotification {
                    signature,
                    profit,
                    sent_at: now,
                },
            );
            return true;
        };

        // New selection/size => send immediately.
        if last.signature != signature {
            *last = LastTelegramNotification {
                signature,
                profit,
                sent_at: now,
            };
            return true;
        }

        // Same selection => only send if profit moved enough and we're past cooldown.
        let profit_delta = (profit - last.profit).abs();
        if profit_delta >= self.min_profit_delta
            && now.duration_since(last.sent_at) >= self.cooldown
        {
            *last = LastTelegramNotification {
                signature,
                profit,
                sent_at: now,
            };
            return true;
        }

        false
    }
}

/// Per-market scan state for debounce logic
#[derive(Debug, Clone)]
struct MarketScanState {
    /// Whether a scan is pending (deferred due to cooldown)
    pending: bool,
    /// When the deferred scan should fire
    due_at: Option<Instant>,
}

impl MarketScanState {
    fn new() -> Self {
        Self {
            pending: false,
            due_at: None,
        }
    }
}

/// Debounce statistics for monitoring
#[derive(Debug, Default)]
struct DebounceStats {
    /// Scans executed immediately (no cooldown)
    scans_immediate: u64,
    /// Scans deferred due to cooldown
    scans_deferred: u64,
    /// Deferred scans that were actually executed
    scans_executed_deferred: u64,
    /// Deferred scans that found opportunities
    scans_fired_deferred: u64,
    /// PriceChange events ignored (don't affect analyzer)
    price_changes_ignored: u64,
}

#[derive(Debug)]
enum GammaUpdate {
    Markets(Vec<StoredMarket>),
    Error(String),
}

fn opportunity_signature(opp: &moonbag_core::models::MoonbagOpportunity) -> u64 {
    let mut hasher = DefaultHasher::new();

    opp.market_slug.hash(&mut hasher);
    opp.k.hash(&mut hasher);
    opp.m.hash(&mut hasher);
    opp.amount_raw.hash(&mut hasher);

    for sel in &opp.selected_outcomes {
        sel.no_token_id.hash(&mut hasher);
    }

    hasher.finish()
}

/// Executor task - receives opportunities from channel and executes them sequentially
async fn run_executor_task(
    mut rx: mpsc::Receiver<MoonbagOpportunity>,
    result_tx: mpsc::Sender<ExecutionResult>,
    executor: Executor,
    notifier: TelegramNotifier,
    dry_run: bool,
) {
    info!("Executor task started (dry_run={})", dry_run);

    while let Some(opp) = rx.recv().await {
        info!(
            market = %opp.market_slug,
            k = opp.k,
            profit = %opp.guaranteed_profit,
            "Executing opportunity"
        );

        // Execute the opportunity with retry (up to 3 attempts for transient failures)
        let result = executor.execute_with_retry(&opp, 3).await;

        if result.success {
            info!(
                market = %opp.market_slug,
                profit = %result.profit,
                tx_hash = ?result.tx_hash,
                "Execution SUCCESS"
            );

            // Send success notification
            if notifier.is_enabled() {
                let msg = format!(
                    "✅ *EXECUTED*\n\
                    Market: {}\n\
                    K={}/{}\n\
                    Profit: ${:.2}\n\
                    Moonbags: {}\n\
                    {}",
                    opp.market_slug,
                    opp.k,
                    opp.m,
                    result.profit,
                    opp.moonbag_count,
                    if dry_run { "🧪 DRY RUN" } else { "🔥 LIVE" }
                );
                let _ = notifier.send_message(&msg).await;
            }
        } else {
            warn!(
                market = %opp.market_slug,
                stage = %result.stage,
                error = ?result.error,
                needs_manual = result.needs_manual_intervention,
                "Execution FAILED"
            );

            // Send failure notification
            if notifier.is_enabled() {
                let msg = format!(
                    "❌ *EXECUTION FAILED*\n\
                    Market: {}\n\
                    Stage: {}\n\
                    Error: {}\n\
                    {}",
                    opp.market_slug,
                    result.stage,
                    result.error.as_deref().unwrap_or("Unknown"),
                    if result.needs_manual_intervention {
                        "⚠️ MANUAL INTERVENTION NEEDED"
                    } else {
                        ""
                    }
                );
                let _ = notifier.send_message(&msg).await;
            }
        }

        // Send result back to scanner (for --once retry logic)
        let _ = result_tx.send(result).await;
    }

    info!("Executor task stopped");
}

/// Send opportunity to executor if enabled
/// Returns true if successfully sent to executor
async fn maybe_execute(
    opp_tx: &Option<mpsc::Sender<MoonbagOpportunity>>,
    opp: &MoonbagOpportunity,
) -> bool {
    if let Some(tx) = opp_tx {
        match tx.try_send(opp.clone()) {
            Ok(()) => {
                debug!(market = %opp.market_slug, "Opportunity sent to executor");
                return true;
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                warn!(market = %opp.market_slug, "Executor queue full, skipping opportunity");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                error!("Executor channel closed!");
            }
        }
    }
    false
}

async fn maybe_send_telegram(
    notifier: &TelegramNotifier,
    deduper: &mut TelegramDeduper,
    opp: &moonbag_core::models::MoonbagOpportunity,
    enabled: bool,
) {
    if !enabled || !notifier.is_enabled() {
        return;
    }
    if !deduper.should_send(opp) {
        return;
    }
    let _ = notifier.send_opportunity(opp).await;
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

    info!("Starting moonbag scanner (24/7 mode)...");

    // Load config
    let mut config = Config::from_env()?;
    let effective_dry_run = args.dry_run.unwrap_or(config.dry_run);

    // Override config.dry_run with CLI flag
    config.dry_run = effective_dry_run;

    // ─────────────────────────────────────────────────────────────────
    // 0. Initialize executor (if --execute is enabled)
    // ─────────────────────────────────────────────────────────────────
    let (opp_tx, result_rx, mut executor_handle) = if args.execute {
        info!("Execution mode ENABLED - will trade detected opportunities");

        // Validate config for live trading (unless dry_run)
        if !effective_dry_run {
            config.validate_for_live()?;
        }

        // Create opportunity channel
        let (tx, rx) = mpsc::channel::<MoonbagOpportunity>(args.max_concurrent_executions);
        // Create result channel (for --once retry logic)
        let (result_tx, result_rx) = mpsc::channel::<ExecutionResult>(args.max_concurrent_executions);

        // Create executor
        let executor = Executor::from_config(&config)?;
        let notifier_for_exec = TelegramNotifier::from_env();
        let dry_run = effective_dry_run;

        // Spawn executor task
        let handle = tokio::spawn(async move {
            run_executor_task(rx, result_tx, executor, notifier_for_exec, dry_run).await;
        });

        (Some(tx), Some(result_rx), Some(handle))
    } else {
        info!("Execution mode DISABLED - detection only (use --execute to enable trading)");
        (None, None, None)
    };

    // ─────────────────────────────────────────────────────────────────
    // 1. Initialize persistent store (redb)
    // ─────────────────────────────────────────────────────────────────
    let store_path = format!("{}/moonbag.redb", args.data_dir);
    info!(path = %store_path, "Opening redb store");
    let redb_store = Arc::new(RedbStore::open(&store_path)?);
    let store = Arc::new(AsyncStateStore::new(redb_store.clone()));
    info!(
        convert_indices = redb_store.convert_index_count(),
        "Store initialized"
    );

    // ─────────────────────────────────────────────────────────────────
    // 2. Initialize Health state and server
    // ─────────────────────────────────────────────────────────────────
    let health_state = Arc::new(HealthState::default());
    let health_server = HealthServer::new(&args.health_bind, health_state.clone())?;
    tokio::spawn(async move {
        if let Err(e) = health_server.run().await {
            error!(error = %e, "Health server error");
        }
    });
    info!(bind = %args.health_bind, "Health server started");

    // ─────────────────────────────────────────────────────────────────
    // 3. Initialize Telegram notifier (unless --no-telegram is set)
    // ─────────────────────────────────────────────────────────────────
    let telegram_enabled = !args.no_telegram;
    let notifier = Arc::new(TelegramNotifier::from_env());
    if telegram_enabled && notifier.is_enabled() {
        info!("Telegram notifications enabled (inline mode)");
        let _ = notifier
            .send_message("🚀 Moonbag Scanner started (24/7 mode)")
            .await;
    } else if args.no_telegram {
        info!("Telegram notifications disabled (use separate notifier process)");
    } else {
        info!("Telegram notifications disabled (missing env vars)");
    }
    let mut telegram_deduper = TelegramDeduper::from_env();

    // ─────────────────────────────────────────────────────────────────
    // 4. Initialize Gamma API client (with store for persistence)
    // ─────────────────────────────────────────────────────────────────
    let gamma_client = Arc::new(GammaApiClient::with_store(
        GammaApiConfig::default(),
        store.clone(),
    ));

    // ─────────────────────────────────────────────────────────────────
    // 5. Load markets (from JSON or Gamma API with retry)
    // ─────────────────────────────────────────────────────────────────
    let stored_markets: Vec<StoredMarket> = if let Some(ref markets_path) = args.markets {
        // Legacy mode: load from JSON file
        info!(path = %markets_path, "Loading markets from JSON file");
        load_markets_from_json(markets_path).await?
    } else {
        // Dynamic mode: load from Gamma API with retry loop
        info!("Loading markets from Gamma API...");
        let mut retry_delay = Duration::from_secs(5);
        let max_retry_delay = Duration::from_secs(60);

        loop {
            match gamma_client.fetch_neg_risk_markets().await {
                Ok(markets) if !markets.is_empty() => {
                    info!(count = markets.len(), "Loaded markets from Gamma API");
                    break markets;
                }
                Ok(_) => {
                    // Empty result - check LKG
                    let lkg = gamma_client.get_lkg_snapshot();
                    if !lkg.is_empty() {
                        warn!(
                            count = lkg.len(),
                            "Using LKG snapshot (Gamma API returned empty)"
                        );
                        break lkg;
                    }
                    warn!(
                        retry_secs = retry_delay.as_secs(),
                        "Gamma API returned no markets and no LKG available, retrying..."
                    );
                }
                Err(e) => {
                    // Error - check LKG
                    let lkg = gamma_client.get_lkg_snapshot();
                    if !lkg.is_empty() {
                        warn!(count = lkg.len(), error = %e, "Using LKG snapshot (Gamma API error)");
                        break lkg;
                    }
                    warn!(
                        retry_secs = retry_delay.as_secs(),
                        error = %e,
                        "Gamma API error and no LKG available, retrying..."
                    );
                }
            }

            // Wait before retry
            tokio::time::sleep(retry_delay).await;
            retry_delay = (retry_delay * 2).min(max_retry_delay);
        }
    };

    if stored_markets.is_empty() {
        error!("No markets found!");
        return Ok(());
    }

    // ─────────────────────────────────────────────────────────────────
    // 6. Create scanner and register markets
    // ─────────────────────────────────────────────────────────────────
    let scanner_config = ScannerConfig {
        min_profit: config.min_profit,
        target_size: config.target_size,
        max_investment: config.max_investment,
        max_remaining_outcomes: config.max_remaining_outcomes,
        min_moonbags: config.min_moonbags,
        min_k: config.min_k,
        max_k: config.max_k,
        // Use configurable book cooldown (default 30ms instead of 250ms)
        market_scan_cooldown: Duration::from_millis(config.book_cooldown_ms),
        bootstrap_enabled: !args.skip_bootstrap,
        dry_run: effective_dry_run,
    };

    info!(
        min_profit = %scanner_config.min_profit,
        max_investment = %scanner_config.max_investment,
        min_k = scanner_config.min_k,
        dry_run = scanner_config.dry_run,
        "Config loaded"
    );

    let mut scanner = Scanner::new(scanner_config.clone());
    let orderbook = scanner.orderbook();

    // Convert to MarketState and register
    let market_states: Vec<MarketState> =
        stored_markets.iter().map(|m| m.to_market_state()).collect();
    scanner.register_markets(market_states);

    // Update health state
    health_state
        .markets_active
        .store(stored_markets.len(), std::sync::atomic::Ordering::Relaxed);

    // Get all token IDs for WebSocket subscription
    let token_ids: Vec<String> = stored_markets
        .iter()
        .flat_map(|m| m.get_all_token_ids())
        .collect();
    health_state
        .tokens_subscribed
        .store(token_ids.len(), std::sync::atomic::Ordering::Relaxed);

    info!(
        markets = stored_markets.len(),
        tokens = token_ids.len(),
        "Markets registered"
    );

    // ─────────────────────────────────────────────────────────────────
    // 7. Bootstrap orderbooks via REST
    // ─────────────────────────────────────────────────────────────────
    if !args.skip_bootstrap {
        info!("Bootstrapping orderbooks via REST API...");
        // Load both YES and NO books for accurate profit calculation
        let mut bootstrap_config = BootstrapConfig::default();
        bootstrap_config.only_no_tokens = false;
        let bootstrapper = Bootstrapper::new(bootstrap_config);
        let bootstrap_result = bootstrapper
            .bootstrap_batch(&token_ids, orderbook.clone())
            .await;
        info!(
            success = bootstrap_result.success_count,
            failed = bootstrap_result.failed_count,
            "Orderbook bootstrap complete"
        );

        // Initial scan after bootstrap
        // NOTE: Skip initial scan in execute mode - bootstrap data may be stale
        // WS-triggered scans use fresh data after orderbook updates arrive
        if !args.execute {
            info!("Running initial scan...");
            let opportunities = scanner.scan_all();
            if !opportunities.is_empty() {
                info!(count = opportunities.len(), "Initial opportunities found!");
                for opp in &opportunities {
                    print_opportunity(opp);
                    maybe_send_telegram(&notifier, &mut telegram_deduper, opp, telegram_enabled)
                        .await;
                    store.log_opportunity(opp.clone());
                }
            } else {
                info!("No opportunities found in initial scan");
            }
        } else {
            info!("Skipping initial scan in execute mode (bootstrap data may be stale)");
            info!("Will scan on fresh WS updates...");
        }
        let once_done_in_initial = false;

        // If --once mode and we already sent one, skip main loop
        if once_done_in_initial {
            info!("--once mode: Waiting for executor to complete...");
            // Drop sender so executor knows no more opportunities
            drop(opp_tx);
            // Wait for executor to finish
            if let Some(handle) = executor_handle.take() {
                let _ = handle.await;
            }
            return Ok(());
        }
    }

    // ─────────────────────────────────────────────────────────────────
    // 8. Run main loop (WS or REST mode)
    // ─────────────────────────────────────────────────────────────────
    if args.rest_only {
        info!("Running in REST-only mode (no WebSocket)");
        let mut bootstrap_config = BootstrapConfig::default();
        bootstrap_config.only_no_tokens = false;
        let bootstrapper = Bootstrapper::new(bootstrap_config);
        run_rest_scan_loop(
            scanner,
            bootstrapper,
            token_ids,
            args.scan_interval_ms,
            notifier,
            telegram_deduper,
            telegram_enabled,
            opp_tx,
        )
        .await?;
    } else {
        // WebSocket mode with dynamic market updates
        run_ws_scan_loop_dynamic(
            scanner,
            orderbook,
            gamma_client,
            health_state,
            store,
            args.gamma_refresh_secs,
            args.scan_interval_ms,
            notifier,
            telegram_deduper,
            telegram_enabled,
            opp_tx,
            result_rx,
            config.book_cooldown_ms,
            config.enable_debounce,
            args.once,
        )
        .await?;
    }

    // Wait for executor task to finish if running
    if let Some(handle) = executor_handle {
        info!("Waiting for executor task to finish...");
        let _ = handle.await;
    }

    Ok(())
}

/// Run REST-only scan loop (no WebSocket)
async fn run_rest_scan_loop(
    mut scanner: Scanner,
    bootstrapper: Bootstrapper,
    token_ids: Vec<String>,
    scan_interval_ms: u64,
    notifier: Arc<TelegramNotifier>,
    mut telegram_deduper: TelegramDeduper,
    telegram_enabled: bool,
    opp_tx: Option<mpsc::Sender<MoonbagOpportunity>>,
) -> anyhow::Result<()> {
    let orderbook = scanner.orderbook();
    let stats = scanner.stats();

    // Minimum 2 seconds for REST mode to avoid hammering API
    let interval = scan_interval_ms.max(2000);
    let mut scan_ticker = tokio::time::interval(Duration::from_millis(interval));
    let mut stats_ticker = tokio::time::interval(Duration::from_secs(30));

    info!("Scanner running (REST-only mode). Press Ctrl+C to stop.");

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("Shutting down...");
                break;
            }

            _ = scan_ticker.tick() => {
                // Refresh orderbooks via REST
                let result = bootstrapper.bootstrap_batch(&token_ids, orderbook.clone()).await;
                tracing::debug!(success = result.success_count, "Refreshed orderbooks");

                // Scan for opportunities
                let opps = scanner.scan_all();
                for opp in opps {
                    print_opportunity(&opp);
                    maybe_send_telegram(&notifier, &mut telegram_deduper, &opp, telegram_enabled).await;
                    // Send to executor if enabled
                    maybe_execute(&opp_tx, &opp).await;
                }
            }

            _ = stats_ticker.tick() => {
                let s = stats.snapshot();
                info!(
                    opportunities = s.opportunities_found,
                    quick_pass = s.quick_check_pass,
                    quick_fail = s.quick_check_fail,
                    "Stats"
                );
            }
        }
    }

    Ok(())
}

/// Print an opportunity
fn print_opportunity(opp: &moonbag_core::models::MoonbagOpportunity) {
    info!(
        "OPPORTUNITY: {} | K={}/{} | profit=${:.2} | cost=${:.2} | moonbags={}",
        opp.market_slug, opp.k, opp.m, opp.guaranteed_profit, opp.no_cost, opp.moonbag_count
    );

    for selected in &opp.selected_outcomes {
        tracing::debug!(
            "  BUY NO: {} @ {:.4} (impact: {:.4})",
            truncate(&selected.question, 40),
            selected.no_vwap,
            selected.no_impact_price
        );
    }

    for remaining in &opp.remaining_outcomes {
        if remaining.convertible {
            tracing::debug!(
                "  MOONBAG: {} (YES bid: {:.4})",
                truncate(&remaining.question, 40),
                remaining.yes_bid
            );
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max.saturating_sub(3)])
    }
}

/// Load markets from a JSON file
async fn load_markets_from_json(path: &str) -> anyhow::Result<Vec<StoredMarket>> {
    let content = tokio::fs::read_to_string(path).await?;
    let markets: Vec<StoredMarket> = serde_json::from_str(&content)?;
    Ok(markets)
}

/// Run WebSocket-based scan loop with dynamic market updates
///
/// Features:
/// - Gamma API polling every `gamma_refresh_secs` for new/resolved markets
/// - MarketReconciler with hysteresis for stable market management
/// - WsClient with control channel for dynamic resubscribe
/// - Health state updates
/// - Optional executor integration for live trading
async fn run_ws_scan_loop_dynamic(
    mut scanner: Scanner,
    orderbook: Arc<OrderbookManager>,
    gamma_client: Arc<GammaApiClient>,
    health_state: Arc<HealthState>,
    store: Arc<AsyncStateStore>,
    gamma_refresh_secs: u64,
    scan_interval_ms: u64,
    notifier: Arc<TelegramNotifier>,
    mut telegram_deduper: TelegramDeduper,
    telegram_enabled: bool,
    mut opp_tx: Option<mpsc::Sender<MoonbagOpportunity>>,
    mut result_rx: Option<mpsc::Receiver<ExecutionResult>>,
    book_cooldown_ms: u64,
    enable_debounce: bool,
    once: bool,
) -> anyhow::Result<()> {
    // Create channels
    let (update_tx, mut update_rx) = mpsc::channel::<WsUpdate>(10000);
    let (control_tx, control_rx) = mpsc::channel::<WsControl>(16);
    let (shutdown_tx, shutdown_rx) = broadcast::channel::<()>(1);

    // Create WS client
    let ws_client = Arc::new(WsClient::with_orderbook(orderbook.clone()));

    // Spawn WS client task with control channel
    let ws_client_task = ws_client.clone();
    let ws_handle = tokio::spawn(async move {
        if let Err(e) = ws_client_task
            .run_with_control(update_tx, control_rx, shutdown_rx)
            .await
        {
            error!(error = %e, "WebSocket client error");
        }
    });

    // Create market reconciler
    let mut reconciler = MarketReconciler::with_defaults();

    // Debounce state for per-market scan cooldown
    let book_cooldown = Duration::from_millis(book_cooldown_ms);
    let mut scan_states: HashMap<String, MarketScanState> = HashMap::new();
    let mut debounce_stats = DebounceStats::default();

    info!(
        book_cooldown_ms = book_cooldown_ms,
        enable_debounce = enable_debounce,
        "Debounce settings"
    );

    // Stats tracking
    let stats = scanner.stats();
    let mut stats_ticker = tokio::time::interval(Duration::from_secs(30));
    let mut scan_ticker = tokio::time::interval(Duration::from_millis(scan_interval_ms));
    let (gamma_update_tx, mut gamma_update_rx) = mpsc::channel::<GammaUpdate>(1);

    // Run Gamma refresh in the background so WS processing never stalls on long pagination.
    let gamma_client_bg = gamma_client.clone();
    let mut gamma_interval = tokio::time::interval(Duration::from_secs(gamma_refresh_secs));
    gamma_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Skip the first tick (we just loaded markets)
    gamma_interval.tick().await;
    let mut gamma_shutdown_rx = shutdown_tx.subscribe();
    let gamma_handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = gamma_shutdown_rx.recv() => {
                    break;
                }
                _ = gamma_interval.tick() => {
                    debug!("Polling Gamma API for market updates...");
                    let msg = match gamma_client_bg.fetch_neg_risk_markets().await {
                        Ok(markets) => GammaUpdate::Markets(markets),
                        Err(e) => GammaUpdate::Error(e.to_string()),
                    };
                    if gamma_update_tx.send(msg).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    info!(
        gamma_refresh_secs = gamma_refresh_secs,
        once = once,
        "Scanner running with dynamic market updates. Press Ctrl+C to stop."
    );

    // Flag for --once mode: set to true after first successful execution
    let mut once_triggered = false;
    // Flag for --once mode: waiting for execution result
    let mut once_pending = false;

    loop {
        // Check if --once mode triggered, break out of loop
        if once_triggered {
            break;
        }

        // Fire deferred scans SYNCHRONOUSLY at start of each iteration
        // This fixes the bug where WS updates arrive faster than debounce timeout,
        // preventing the async debounce timer from ever completing
        if enable_debounce {
            let now = Instant::now();
            let mut markets_to_scan = Vec::new();

            for (market_id, state) in scan_states.iter_mut() {
                if state.pending && state.due_at.map(|d| d <= now).unwrap_or(false) {
                    markets_to_scan.push(market_id.clone());
                    state.pending = false;
                    state.due_at = None;
                }
            }

            if !markets_to_scan.is_empty() {
                tracing::debug!(count = markets_to_scan.len(), "Firing deferred scans");
            }
            for market_id in markets_to_scan {
                debounce_stats.scans_executed_deferred += 1;
                if let Some(opp) = scanner.check_market(&market_id) {
                    debounce_stats.scans_fired_deferred += 1;

                    info!(
                        market = %opp.market_slug,
                        k = opp.k,
                        m = opp.m,
                        profit = format!("{:.2}", opp.guaranteed_profit),
                        cost = format!("{:.2}", opp.no_cost),
                        moonbags = opp.m - opp.k,
                        "OPPORTUNITY (deferred)"
                    );
                    maybe_send_telegram(&notifier, &mut telegram_deduper, &opp, telegram_enabled).await;
                    store.log_opportunity(opp.clone());
                    health_state.opportunities_found.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if maybe_execute(&opp_tx, &opp).await && once && !once_pending {
                        info!("--once mode: Sent opportunity to executor, waiting for result...");
                        once_pending = true;
                    }
                }
            }
        }

        tokio::select! {
            // Handle execution results (for --once retry logic)
            Some(result) = async {
                if let Some(ref mut rx) = result_rx {
                    rx.recv().await
                } else {
                    std::future::pending().await
                }
            }, if once_pending => {
                if result.success || result.partial_k || result.converted_amount > rust_decimal::Decimal::ZERO {
                    // Success or partial success - shutdown
                    info!("--once mode: Execution completed (success={}, partial_k={}), shutting down...",
                          result.success, result.partial_k);
                    let _ = shutdown_tx.send(());
                    once_triggered = true;
                } else {
                    // Full failure (no fills) - retry with another opportunity
                    info!("--once mode: Execution FAILED completely, continuing to scan for another opportunity...");
                    once_pending = false;
                }
            }
            // Handle Ctrl+C
            _ = tokio::signal::ctrl_c() => {
                info!("Shutting down...");
                let _ = shutdown_tx.send(());
                break;
            }

            // Process WebSocket updates
            Some(update) = update_rx.recv() => {
                match update {
                    WsUpdate::Token { token_id, kind, received_at, server_ts_ms } => {
                        // Update health: last WS message time
                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as u64;
                        health_state.last_ws_msg.store(
                            now_ms / 1000,
                            std::sync::atomic::Ordering::Relaxed,
                        );

                        // Ignore PriceChange events - they don't affect analyzer inputs
                        // (after phantom liquidity fix, analyzer only uses orderbook depth)
                        if kind == WsUpdateKind::PriceChange {
                            debounce_stats.price_changes_ignored += 1;
                            continue;
                        }

                        // Book event - proceed with scan

                        // Sample E2E latency every 1000 messages with server timestamp
                        static MSG_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                        let count = MSG_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if count % 1000 == 0 {
                            if let Some(server_ts) = server_ts_ms {
                                let e2e_ms = now_ms.saturating_sub(server_ts);
                                let process_us = received_at.elapsed().as_micros();
                                debug!(
                                    e2e_ms = e2e_ms,
                                    process_us = process_us,
                                    msg_count = count,
                                    "Latency sample"
                                );
                            }
                        }

                        // Get market_id for debounce tracking
                        let Some(market_id) = orderbook.get_market_id(&token_id) else {
                            continue;
                        };

                        // Check this market for opportunities
                        if let Some(opp) = scanner.process_update(&token_id) {
                            debounce_stats.scans_immediate += 1;

                            // Clear pending flag since we just scanned
                            if let Some(state) = scan_states.get_mut(&market_id) {
                                state.pending = false;
                                state.due_at = None;
                            }

                            let process_latency = received_at.elapsed();

                            // Calculate E2E latency if server timestamp available
                            let e2e_latency_ms = server_ts_ms.map(|server_ts| {
                                now_ms.saturating_sub(server_ts)
                            });

                            info!(
                                process_us = process_latency.as_micros(),
                                e2e_ms = ?e2e_latency_ms,
                                market = %opp.market_slug,
                                profit = format!("{:.2}", opp.guaranteed_profit),
                                "OPPORTUNITY detected"
                            );
                            print_opportunity(&opp);
                            maybe_send_telegram(&notifier, &mut telegram_deduper, &opp, telegram_enabled).await;
                            // Log to store
                            store.log_opportunity(opp.clone());
                            health_state.opportunities_found.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            // Send to executor if enabled
                            if maybe_execute(&opp_tx, &opp).await && once && !once_pending {
                                info!("--once mode: Sent opportunity to executor, waiting for result...");
                                once_pending = true;
                            }
                        } else if enable_debounce {
                            // Scanner returned None (likely cooldown) - schedule deferred scan
                            let state = scan_states.entry(market_id.clone()).or_insert_with(MarketScanState::new);
                            if !state.pending {
                                state.pending = true;
                                state.due_at = Some(Instant::now() + book_cooldown);
                                debounce_stats.scans_deferred += 1;
                            }
                        }
                    }
                    WsUpdate::Connected => {
                        info!("WebSocket connected");
                        health_state.ws_connected.store(true, std::sync::atomic::Ordering::Relaxed);
                        health_state.ws_fail_streak.store(0, std::sync::atomic::Ordering::Relaxed);
                    }
                    WsUpdate::Disconnected => {
                        warn!("WebSocket disconnected, will reconnect...");
                        health_state.ws_connected.store(false, std::sync::atomic::Ordering::Relaxed);
                        health_state.ws_fail_streak.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        health_state.ws_reconnect_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            }

            // NOTE: Debounce timer logic moved to synchronous check at start of loop
            // to fix bug where WS updates arriving faster than 30ms prevented timer from firing

            // Gamma API polling for market discovery (background task sends updates here)
            Some(gamma_update) = gamma_update_rx.recv() => {
                match gamma_update {
                    GammaUpdate::Markets(fresh_markets) => {
                        // Update health: last Gamma OK
                        health_state.last_gamma_ok.store(
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs(),
                            std::sync::atomic::Ordering::Relaxed,
                        );
                        health_state.gamma_degraded.store(false, std::sync::atomic::Ordering::Relaxed);

                        // Get current market IDs
                        let current_ids = orderbook.active_market_ids();

                        // Reconcile
                        let result = reconciler.reconcile(&current_ids, &fresh_markets);

                        // Apply changes
                        if result.has_changes() {
                            // Remove markets
                            for market_id in &result.removed {
                                orderbook.unregister_market(market_id);
                                info!(market_id = %market_id, "Removed market");
                            }

                            // Add new markets
                            for market in &result.added {
                                let state = market.to_market_state();
                                orderbook.register_market(state);
                                info!(
                                    market_id = %market.neg_risk_market_id,
                                    title = %market.title,
                                    outcomes = market.outcomes.len(),
                                    "Added market"
                                );
                            }

                            // Update health state
                            health_state.markets_active.store(
                                orderbook.active_market_ids().len(),
                                std::sync::atomic::Ordering::Relaxed,
                            );
                            health_state.tokens_subscribed.store(
                                orderbook.active_token_ids().len(),
                                std::sync::atomic::Ordering::Relaxed,
                            );

                            // Trigger WS resubscribe if needed
                            if result.needs_resubscribe {
                                info!(
                                    added = result.added.len(),
                                    removed = result.removed.len(),
                                    "Triggering WS resubscribe"
                                );
                                let _ = control_tx.send(WsControl::Resubscribe).await;
                            }
                        }
                    }
                    GammaUpdate::Error(error) => {
                        warn!(error = %error, "Gamma API fetch failed, using LKG");
                        health_state.gamma_degraded.store(true, std::sync::atomic::Ordering::Relaxed);
                        // Don't clear markets - use LKG snapshot
                    }
                }
            }

            // Periodic full scan (backup, catches any missed updates)
            _ = scan_ticker.tick() => {
                let opps = scanner.scan_all();
                for opp in opps {
                    print_opportunity(&opp);
                    maybe_send_telegram(&notifier, &mut telegram_deduper, &opp, telegram_enabled).await;
                    store.log_opportunity(opp.clone());
                    health_state.opportunities_found.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    // Send to executor if enabled
                    if maybe_execute(&opp_tx, &opp).await && once && !once_pending {
                        info!("--once mode: Sent opportunity to executor, waiting for result...");
                        once_pending = true;
                        break; // Break from for loop, wait for result
                    }
                }
            }

            // Print stats periodically
            _ = stats_ticker.tick() => {
                let s = stats.snapshot();
                let book_updates = orderbook.get_book_updates();
                let market_ids = orderbook.active_market_ids();
                let tokens = orderbook.active_token_ids().len();
                let ws_messages = ws_client.message_count();
                let ws_dropped = ws_client.dropped_count();
                let ws_price_changes = ws_client.price_change_updates();
                let ws_book_updates = ws_client.book_updates();

                // Expose dropped WS updates via health/metrics (best-effort; updated on stats tick)
                health_state
                    .dropped_updates
                    .store(ws_dropped, std::sync::atomic::Ordering::Relaxed);

                // Find best profit across all markets (Option to avoid formatting sentinels)
                let mut best_profit: Option<Decimal> = None;
                let mut best_market = String::new();
                for market_id in &market_ids {
                    if let Some(market) = orderbook.get_market_clone(market_id) {
                        if let Some(profit) = quick_check_with_best_profit(
                            &market,
                            Decimal::new(100, 0),  // target size = 100
                            2,          // min_k
                            None,       // max_k
                            Decimal::new(1, 2), // moonbag_threshold = 0.01
                        ) {
                            if best_profit.map(|b| profit > b).unwrap_or(true) {
                                best_profit = Some(profit);
                                best_market = market.slug.clone();
                            }
                        }
                    }
                }

                info!(
                    markets = market_ids.len(),
                    tokens = tokens,
                    book_updates = book_updates,
                    opportunities = s.opportunities_found,
                    quick_pass = s.quick_check_pass,
                    quick_fail = s.quick_check_fail,
                    ws_messages = ws_messages,
                    ws_dropped = ws_dropped,
                    ws_price_changes = ws_price_changes,
                    ws_book_updates = ws_book_updates,
                    best_profit = best_profit
                        .map(|p| format!("{:.4}", p))
                        .unwrap_or_else(|| "n/a".to_string()),
                    best_market = %best_market,
                    ws_connected = health_state.ws_connected.load(std::sync::atomic::Ordering::Relaxed),
                    scans_immediate = debounce_stats.scans_immediate,
                    scans_deferred = debounce_stats.scans_deferred,
                    scans_executed = debounce_stats.scans_executed_deferred,
                    scans_with_opp = debounce_stats.scans_fired_deferred,
                    price_changes_ignored = debounce_stats.price_changes_ignored,
                    "Stats"
                );
            }
        }
    }

    // Wait for WS client to finish
    let _ = ws_handle.await;
    gamma_handle.abort();
    info!("Scanner stopped.");

    Ok(())
}
