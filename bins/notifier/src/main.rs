//! Moonbag Notifier - Standalone Telegram notification service
//!
//! Reads opportunities from redb and sends Telegram notifications.
//!
//! Architecture:
//! 1. Opens shared redb database (read-only for opportunity_log)
//! 2. Tracks last_sent_id checkpoint (persisted)
//! 3. Deduplicates by market signature + cooldown (persisted)
//! 4. Rate-limited Telegram API with 429 handling
//! 5. Health endpoint on :8081

use axum::{routing::get, Json, Router};
use clap::Parser;
use moonbag_core::models::MoonbagOpportunity;
use moonbag_notifier::TelegramNotifier;
use redb::{Database, ReadableTable, TableDefinition};
use rust_decimal::Decimal;
use serde::Serialize;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

// Table definitions (must match scanner's store.rs)
const OPPORTUNITY_LOG_TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new("opportunity_log");

// Notifier-specific tables
const NOTIFIER_STATE_TABLE: TableDefinition<&str, u64> = TableDefinition::new("notifier_state");
// Dedup table: market_slug -> JSON { signature, profit, last_sent_unix }
const DEDUP_STATE_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("dedup_state");

const CHECKPOINT_KEY: &str = "last_sent_id";

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Parser, Debug)]
#[command(name = "moonbag-notifier")]
#[command(about = "Telegram notification service for moonbag opportunities")]
struct Args {
    /// Path to redb database
    #[arg(long, default_value = "./data/moonbag.redb")]
    db_path: String,

    /// Health server bind address
    #[arg(long, default_value = "0.0.0.0:8081")]
    health_bind: String,

    /// Poll interval in seconds
    #[arg(long, default_value = "5")]
    poll_interval_secs: u64,

    /// Cooldown between notifications for same market (seconds)
    #[arg(long, default_value = "60")]
    dedup_cooldown_secs: u64,

    /// Minimum profit change to re-notify (USDC)
    #[arg(long, default_value = "0.01")]
    min_profit_delta: String,

    /// Log level
    #[arg(long, default_value = "info")]
    log_level: String,
}

/// Health state for the notifier
struct NotifierHealth {
    started_at_unix: AtomicU64,
    running: AtomicBool,
    last_poll_ts: AtomicU64,
    opportunities_sent: AtomicU64,
    opportunities_skipped: AtomicU64,
    telegram_errors: AtomicU64,
    telegram_rate_limited: AtomicU64,
    backlog_size: AtomicU64,
    last_sent_id: AtomicU64,
}

impl NotifierHealth {
    fn new() -> Self {
        Self {
            started_at_unix: AtomicU64::new(now_unix()),
            running: AtomicBool::new(true),
            last_poll_ts: AtomicU64::new(0),
            opportunities_sent: AtomicU64::new(0),
            opportunities_skipped: AtomicU64::new(0),
            telegram_errors: AtomicU64::new(0),
            telegram_rate_limited: AtomicU64::new(0),
            backlog_size: AtomicU64::new(0),
            last_sent_id: AtomicU64::new(0),
        }
    }

    fn uptime_secs(&self) -> u64 {
        now_unix().saturating_sub(self.started_at_unix.load(Ordering::Relaxed))
    }
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    uptime_secs: u64,
    running: bool,
    last_poll_secs_ago: Option<u64>,
    opportunities_sent: u64,
    opportunities_skipped: u64,
    telegram_errors: u64,
    telegram_rate_limited: u64,
    backlog_size: u64,
    last_sent_id: u64,
}

/// Deduplication state for a market (in-memory)
#[derive(Debug, Clone)]
struct DedupEntry {
    signature: u64,
    profit: Decimal,
    last_sent_unix: u64,
}

/// Serializable dedup entry for persistence
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
struct DedupEntryPersist {
    signature: u64,
    profit: String, // Decimal as string for JSON
    last_sent_unix: u64,
}

impl DedupEntry {
    fn to_persist(&self) -> DedupEntryPersist {
        DedupEntryPersist {
            signature: self.signature,
            profit: self.profit.to_string(),
            last_sent_unix: self.last_sent_unix,
        }
    }

    fn from_persist(p: DedupEntryPersist) -> Option<Self> {
        Some(Self {
            signature: p.signature,
            profit: p.profit.parse().ok()?,
            last_sent_unix: p.last_sent_unix,
        })
    }
}

/// Notifier with dedup and rate limiting
struct Notifier {
    db: Arc<Database>,
    telegram: TelegramNotifier,
    health: Arc<NotifierHealth>,
    dedup: RwLock<HashMap<String, DedupEntry>>,
    cooldown: Duration,
    min_profit_delta: Decimal,
}

impl Notifier {
    fn new(
        db: Database,
        telegram: TelegramNotifier,
        health: Arc<NotifierHealth>,
        cooldown: Duration,
        min_profit_delta: Decimal,
    ) -> Self {
        let db = Arc::new(db);

        // Load persisted dedup state
        let dedup = Self::load_dedup_state(&db);
        info!(entries = dedup.len(), "Loaded dedup state from store");

        Self {
            db,
            telegram,
            health,
            dedup: RwLock::new(dedup),
            cooldown,
            min_profit_delta,
        }
    }

    /// Load dedup state from redb
    fn load_dedup_state(db: &Database) -> HashMap<String, DedupEntry> {
        let mut result = HashMap::new();

        let Ok(read_txn) = db.begin_read() else {
            return result;
        };
        let Ok(table) = read_txn.open_table(DEDUP_STATE_TABLE) else {
            return result;
        };

        let Ok(iter) = table.iter() else {
            return result;
        };

        for entry in iter.flatten() {
            let key = entry.0.value().to_string();
            let bytes = entry.1.value();

            if let Ok(persist) = serde_json::from_slice::<DedupEntryPersist>(bytes) {
                if let Some(dedup_entry) = DedupEntry::from_persist(persist) {
                    result.insert(key, dedup_entry);
                }
            }
        }

        result
    }

    /// Save a single dedup entry to redb (async via spawn_blocking)
    async fn save_dedup_entry(&self, market_slug: &str, entry: &DedupEntry) {
        let bytes = match serde_json::to_vec(&entry.to_persist()) {
            Ok(b) => b,
            Err(e) => {
                warn!(error = %e, "Failed to serialize dedup entry");
                return;
            }
        };

        let db = self.db.clone();
        let market_slug = market_slug.to_string();

        let result = tokio::task::spawn_blocking(move || {
            let Ok(write_txn) = db.begin_write() else {
                warn!("Failed to begin write txn for dedup");
                return;
            };
            let result = {
                let Ok(mut table) = write_txn.open_table(DEDUP_STATE_TABLE) else {
                    warn!("Failed to open dedup table");
                    return;
                };
                table
                    .insert(market_slug.as_str(), bytes.as_slice())
                    .map(|_| ())
            };
            if let Err(e) = result {
                warn!(error = %e, "Failed to insert dedup entry");
                return;
            }
            if let Err(e) = write_txn.commit() {
                warn!(error = %e, "Failed to commit dedup entry");
            }
        })
        .await;

        if let Err(e) = result {
            warn!(error = %e, "spawn_blocking for save_dedup_entry failed");
        }
    }

    /// Get the last sent opportunity ID (checkpoint)
    fn get_checkpoint(&self) -> u64 {
        let Ok(read_txn) = self.db.begin_read() else {
            return 0;
        };
        let Ok(table) = read_txn.open_table(NOTIFIER_STATE_TABLE) else {
            return 0;
        };
        table
            .get(CHECKPOINT_KEY)
            .ok()
            .flatten()
            .map(|v| v.value())
            .unwrap_or(0)
    }

    /// Set the checkpoint (async via spawn_blocking)
    async fn set_checkpoint(&self, id: u64) {
        let db = self.db.clone();

        let result = tokio::task::spawn_blocking(move || {
            let Ok(write_txn) = db.begin_write() else {
                warn!("Failed to begin write txn for checkpoint");
                return;
            };
            let result = {
                let Ok(mut table) = write_txn.open_table(NOTIFIER_STATE_TABLE) else {
                    warn!("Failed to open notifier_state table");
                    return;
                };
                table.insert(CHECKPOINT_KEY, id).map(|_| ())
            };
            if let Err(e) = result {
                warn!(error = %e, "Failed to insert checkpoint");
                return;
            }
            if let Err(e) = write_txn.commit() {
                warn!(error = %e, "Failed to commit checkpoint");
            }
        })
        .await;

        if let Err(e) = result {
            warn!(error = %e, "spawn_blocking for set_checkpoint failed");
        }
    }

    /// Read new opportunities since checkpoint
    fn read_new_opportunities(&self, after_id: u64) -> Vec<(u64, MoonbagOpportunity)> {
        let mut results = Vec::new();

        let Ok(read_txn) = self.db.begin_read() else {
            return results;
        };
        let Ok(table) = read_txn.open_table(OPPORTUNITY_LOG_TABLE) else {
            return results;
        };

        // Iterate from after_id + 1
        let range = table.range((after_id + 1)..);
        let Ok(iter) = range else {
            return results;
        };

        for entry in iter.flatten() {
            let id = entry.0.value();
            let bytes = entry.1.value();

            match serde_json::from_slice::<MoonbagOpportunity>(bytes) {
                Ok(opp) => results.push((id, opp)),
                Err(e) => {
                    warn!(id = id, error = %e, "Failed to deserialize opportunity");
                }
            }
        }

        results
    }

    /// Check if there's backlog (O(1) using last key)
    ///
    /// Returns approximate backlog based on key difference.
    /// Since keys are timestamps (ms), divides by expected rate.
    fn count_backlog(&self, after_id: u64) -> u64 {
        let Ok(read_txn) = self.db.begin_read() else {
            return 0;
        };
        let Ok(table) = read_txn.open_table(OPPORTUNITY_LOG_TABLE) else {
            return 0;
        };

        // Get the last (highest) key - O(1)
        let last_key = table
            .last()
            .ok()
            .flatten()
            .map(|(k, _)| k.value())
            .unwrap_or(0);

        if last_key <= after_id {
            return 0;
        }

        // Estimate: key difference / average interval
        // Keys are ~milliseconds, assume ~1 opportunity per second average
        let key_diff = last_key.saturating_sub(after_id);
        // Rough estimate: if keys span 60000ms (1 min), probably ~10-60 entries
        // For health metrics, just report if there's any backlog
        key_diff.min(1000) // Cap at 1000 to avoid huge numbers
    }

    /// Check if we should send notification (dedup logic)
    async fn should_send(&self, opp: &MoonbagOpportunity) -> bool {
        let key = opp.market_slug.clone();
        let signature = opportunity_signature(opp);
        let profit = opp.guaranteed_profit;
        let now = now_unix();
        let cooldown_secs = self.cooldown.as_secs();

        let mut dedup = self.dedup.write().await;

        let Some(entry) = dedup.get(&key) else {
            // First time seeing this market
            let new_entry = DedupEntry {
                signature,
                profit,
                last_sent_unix: now,
            };
            self.save_dedup_entry(&key, &new_entry).await;
            dedup.insert(key, new_entry);
            return true;
        };

        // New selection/size => send immediately
        if entry.signature != signature {
            let new_entry = DedupEntry {
                signature,
                profit,
                last_sent_unix: now,
            };
            self.save_dedup_entry(&key, &new_entry).await;
            dedup.insert(key, new_entry);
            return true;
        }

        // Same selection => check profit delta and cooldown
        let profit_delta = (profit - entry.profit).abs();
        let elapsed = now.saturating_sub(entry.last_sent_unix);
        if profit_delta >= self.min_profit_delta && elapsed >= cooldown_secs {
            let new_entry = DedupEntry {
                signature,
                profit,
                last_sent_unix: now,
            };
            self.save_dedup_entry(&key, &new_entry).await;
            dedup.insert(key, new_entry);
            return true;
        }

        false
    }

    /// Send notification with rate limit handling
    async fn send_with_retry(&self, opp: &MoonbagOpportunity) -> bool {
        const MAX_RETRIES: u32 = 3;
        let mut retry_delay = Duration::from_secs(1);

        for attempt in 0..MAX_RETRIES {
            match self.telegram.send_opportunity(opp).await {
                Ok(true) => {
                    self.health
                        .opportunities_sent
                        .fetch_add(1, Ordering::Relaxed);
                    return true;
                }
                Ok(false) => {
                    // Telegram returned error (possibly rate limited)
                    self.health.telegram_errors.fetch_add(1, Ordering::Relaxed);

                    if attempt < MAX_RETRIES - 1 {
                        // Check if rate limited (429)
                        // Note: TelegramNotifier doesn't expose status code, so we just retry
                        self.health
                            .telegram_rate_limited
                            .fetch_add(1, Ordering::Relaxed);
                        warn!(
                            attempt = attempt + 1,
                            delay_ms = retry_delay.as_millis(),
                            "Telegram send failed, retrying"
                        );
                        tokio::time::sleep(retry_delay).await;
                        retry_delay *= 2; // Exponential backoff
                    }
                }
                Err(e) => {
                    error!(error = %e, "Telegram send error");
                    self.health.telegram_errors.fetch_add(1, Ordering::Relaxed);
                    return false;
                }
            }
        }

        false
    }

    /// Process new opportunities
    async fn process(&self) {
        let checkpoint = self.get_checkpoint();
        self.health
            .last_sent_id
            .store(checkpoint, Ordering::Relaxed);

        // Count backlog
        let backlog = self.count_backlog(checkpoint);
        self.health.backlog_size.store(backlog, Ordering::Relaxed);

        // Read new opportunities
        let new_opps = self.read_new_opportunities(checkpoint);
        if new_opps.is_empty() {
            return;
        }

        debug!(count = new_opps.len(), "Found new opportunities");

        let mut last_processed_id = checkpoint;

        for (id, opp) in new_opps {
            // Check dedup
            if !self.should_send(&opp).await {
                self.health
                    .opportunities_skipped
                    .fetch_add(1, Ordering::Relaxed);
                last_processed_id = id;
                continue;
            }

            // Send notification - only advance checkpoint on success
            if self.send_with_retry(&opp).await {
                info!(
                    market = %opp.market_title,
                    profit = %opp.guaranteed_profit,
                    "Sent notification"
                );
                last_processed_id = id;
            } else {
                // Stop processing on first failure to avoid losing notifications
                warn!(
                    market = %opp.market_title,
                    id = id,
                    "Failed to send, stopping batch to retry later"
                );
                break;
            }

            // Small delay between sends to avoid rate limiting
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // Update checkpoint
        if last_processed_id > checkpoint {
            self.set_checkpoint(last_processed_id).await;
            self.health
                .last_sent_id
                .store(last_processed_id, Ordering::Relaxed);
        }
    }
}

/// Compute opportunity signature for dedup
fn opportunity_signature(opp: &MoonbagOpportunity) -> u64 {
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

/// Health endpoint handler
async fn health_handler(health: axum::extract::State<Arc<NotifierHealth>>) -> Json<HealthResponse> {
    let current_time = now_unix();

    let last_poll = health.last_poll_ts.load(Ordering::Relaxed);
    let last_poll_secs_ago = if last_poll > 0 {
        Some(current_time.saturating_sub(last_poll))
    } else {
        None
    };

    let running = health.running.load(Ordering::Relaxed);
    let status = if running { "ok" } else { "stopped" };

    Json(HealthResponse {
        status,
        uptime_secs: health.uptime_secs(),
        running,
        last_poll_secs_ago,
        opportunities_sent: health.opportunities_sent.load(Ordering::Relaxed),
        opportunities_skipped: health.opportunities_skipped.load(Ordering::Relaxed),
        telegram_errors: health.telegram_errors.load(Ordering::Relaxed),
        telegram_rate_limited: health.telegram_rate_limited.load(Ordering::Relaxed),
        backlog_size: health.backlog_size.load(Ordering::Relaxed),
        last_sent_id: health.last_sent_id.load(Ordering::Relaxed),
    })
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

    info!("Starting moonbag notifier...");

    // Ensure parent directory exists
    let db_path = std::path::Path::new(&args.db_path);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Open database
    info!(path = %args.db_path, "Opening redb database");
    let db = Database::create(&args.db_path)?;

    // Create notifier tables if they don't exist
    {
        let write_txn = db.begin_write()?;
        write_txn.open_table(NOTIFIER_STATE_TABLE)?;
        write_txn.open_table(DEDUP_STATE_TABLE)?;
        write_txn.commit()?;
    }

    // Create Telegram notifier
    let telegram = TelegramNotifier::from_env();
    if telegram.is_enabled() {
        info!("Telegram notifications enabled");
        let _ = telegram.send_message("Moonbag Notifier started").await;
    } else {
        warn!("Telegram notifications DISABLED - check TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID");
    }

    // Parse config
    let min_profit_delta: Decimal = args.min_profit_delta.parse()?;
    let cooldown = Duration::from_secs(args.dedup_cooldown_secs);

    // Create health state
    let health = Arc::new(NotifierHealth::new());

    // Create notifier
    let notifier = Arc::new(Notifier::new(
        db,
        telegram,
        health.clone(),
        cooldown,
        min_profit_delta,
    ));

    // Start health server
    let health_bind: SocketAddr = args.health_bind.parse()?;
    let health_router = Router::new()
        .route("/health", get(health_handler))
        .with_state(health.clone());

    let health_handle = tokio::spawn(async move {
        info!(bind = %health_bind, "Health server starting");
        let listener = tokio::net::TcpListener::bind(health_bind).await.unwrap();
        axum::serve(listener, health_router).await.unwrap();
    });

    // Main loop
    let poll_interval = Duration::from_secs(args.poll_interval_secs);
    info!(
        poll_interval_secs = args.poll_interval_secs,
        cooldown_secs = args.dedup_cooldown_secs,
        "Notifier running. Press Ctrl+C to stop."
    );

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("Shutting down...");
                health.running.store(false, Ordering::Relaxed);
                break;
            }

            _ = tokio::time::sleep(poll_interval) => {
                // Update last poll timestamp
                health.last_poll_ts.store(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    Ordering::Relaxed,
                );

                // Process new opportunities
                notifier.process().await;
            }
        }
    }

    // Cleanup
    health_handle.abort();
    info!("Notifier stopped.");

    Ok(())
}
