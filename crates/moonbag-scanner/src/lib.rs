//! Moonbag Scanner - WebSocket orderbook monitoring
//!
//! High-performance scanner for detecting moonbag opportunities in real-time.
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────┐     ┌──────────────┐     ┌─────────────┐
//! │  Bootstrap  │────▶│  Orderbook   │◀────│  WebSocket  │
//! │  (REST)     │     │  (DashMap)   │     │  Client     │
//! └─────────────┘     └──────────────┘     └─────────────┘
//!                            │
//!                            ▼
//!                     ┌──────────────┐
//!                     │   Analyzer   │
//!                     │  (moonbag-   │
//!                     │   analyzer)  │
//!                     └──────────────┘
//!                            │
//!                            ▼
//!                     ┌──────────────┐
//!                     │ Opportunity  │
//!                     │   Channel    │
//!                     └──────────────┘
//! ```
//!
//! ## Performance
//!
//! - DashMap for lock-free concurrent orderbook access
//! - Zero-copy message parsing where possible
//! - Parallel message processing
//! - Bootstrap preloads depth before WS connects

pub mod bootstrap;
pub mod gamma_api;
pub mod health;
pub mod orderbook;
pub mod reconciler;
pub mod store;
pub mod ws_client;

pub use bootstrap::{
    BootstrapConfig, BootstrapResult, Bootstrapper, PriceBootstrapResult, TokenPrice,
};
pub use gamma_api::{ConvertIndexCache, GammaApiClient, GammaApiConfig, GAMMA_API_URL};
pub use health::{HealthMode, HealthResponse, HealthServer, HealthState, ReadyResponse};
pub use orderbook::{OrderbookManager, TokenMapping, TokenType};
pub use reconciler::{MarketReconciler, ReconcileResult, ReconcilerConfig};
pub use store::{AsyncStateStore, RedbStore, StateStore, StoreConfig};
pub use ws_client::{WsClient, WsClientConfig, WsControl, WsUpdate, WsUpdateKind, WS_URL};

use moonbag_analyzer::{quick_check, MoonbagAnalyzer};
use moonbag_core::models::{MarketState, MoonbagOpportunity};
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info};

/// Scanner configuration
#[derive(Debug, Clone)]
pub struct ScannerConfig {
    /// Minimum profit threshold (USD)
    pub min_profit: Decimal,
    /// Target size per outcome (0 = auto)
    pub target_size: Decimal,
    /// Maximum investment per opportunity (USD)
    pub max_investment: Decimal,
    /// Max remaining outcomes (None = unlimited)
    pub max_remaining_outcomes: Option<u8>,
    /// Minimum moonbag count
    pub min_moonbags: u8,
    /// Minimum K (selected outcomes)
    pub min_k: u8,
    /// Maximum K (None = unlimited)
    pub max_k: Option<u8>,
    /// Cooldown between market scans (seconds)
    pub market_scan_cooldown: Duration,
    /// Enable bootstrap from REST
    pub bootstrap_enabled: bool,
    /// Dry run mode (no queue writes)
    pub dry_run: bool,
}

impl Default for ScannerConfig {
    fn default() -> Self {
        Self {
            min_profit: Decimal::new(5, 2), // $0.05
            target_size: Decimal::ZERO,     // auto
            max_investment: Decimal::new(300, 0),
            max_remaining_outcomes: Some(1),
            min_moonbags: 0,
            min_k: 2,
            max_k: None,
            market_scan_cooldown: Duration::from_millis(250),
            bootstrap_enabled: true,
            dry_run: true,
        }
    }
}

/// Statistics for the scanner
#[derive(Debug, Default)]
pub struct ScannerStats {
    pub messages_received: AtomicU64,
    pub book_updates: AtomicU64,
    pub dropped_updates: AtomicU64,
    pub opportunities_found: AtomicU64,
    pub quick_check_pass: AtomicU64,
    pub quick_check_fail: AtomicU64,
}

impl ScannerStats {
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            messages_received: self.messages_received.load(Ordering::Relaxed),
            book_updates: self.book_updates.load(Ordering::Relaxed),
            dropped_updates: self.dropped_updates.load(Ordering::Relaxed),
            opportunities_found: self.opportunities_found.load(Ordering::Relaxed),
            quick_check_pass: self.quick_check_pass.load(Ordering::Relaxed),
            quick_check_fail: self.quick_check_fail.load(Ordering::Relaxed),
        }
    }
}

/// Point-in-time snapshot of stats
#[derive(Debug, Clone)]
pub struct StatsSnapshot {
    pub messages_received: u64,
    pub book_updates: u64,
    pub dropped_updates: u64,
    pub opportunities_found: u64,
    pub quick_check_pass: u64,
    pub quick_check_fail: u64,
}

/// Main scanner orchestrator
pub struct Scanner {
    config: ScannerConfig,
    orderbook: Arc<OrderbookManager>,
    analyzer: MoonbagAnalyzer,
    stats: Arc<ScannerStats>,
    last_scan_at: HashMap<String, Instant>,
}

impl Scanner {
    /// Create a new scanner
    pub fn new(config: ScannerConfig) -> Self {
        let analyzer = MoonbagAnalyzer {
            min_profit: config.min_profit,
            min_moonbags: config.min_moonbags,
            min_k: config.min_k,
            max_k: config.max_k,
            ..Default::default()
        };

        Self {
            config,
            orderbook: OrderbookManager::new_shared(),
            analyzer,
            stats: Arc::new(ScannerStats::default()),
            last_scan_at: HashMap::new(),
        }
    }

    /// Get the orderbook manager (for sharing with WS client)
    pub fn orderbook(&self) -> Arc<OrderbookManager> {
        self.orderbook.clone()
    }

    /// Get stats
    pub fn stats(&self) -> Arc<ScannerStats> {
        self.stats.clone()
    }

    /// Register markets from loaded data
    pub fn register_markets(&self, markets: Vec<MarketState>) {
        for market in markets {
            self.orderbook.register_market(market);
        }
        info!(
            "Registered {} markets, {} tokens",
            self.orderbook.market_count(),
            self.orderbook.token_count()
        );
    }

    /// Check a single market for opportunities (with cooldown for WS-triggered scans)
    pub fn check_market(&mut self, market_id: &str) -> Option<MoonbagOpportunity> {
        // Check cooldown - only for WS-triggered scans
        let now = Instant::now();
        if let Some(last) = self.last_scan_at.get(market_id) {
            if now.duration_since(*last) < self.config.market_scan_cooldown {
                return None;
            }
        }
        self.last_scan_at.insert(market_id.to_string(), now);

        self.scan_market_internal(market_id)
    }

    /// Check a single market without cooldown (for periodic full scans)
    pub fn check_market_no_cooldown(&mut self, market_id: &str) -> Option<MoonbagOpportunity> {
        self.scan_market_internal(market_id)
    }

    /// Internal market scanning logic (no cooldown tracking)
    fn scan_market_internal(&mut self, market_id: &str) -> Option<MoonbagOpportunity> {
        // Get market
        let market = self.orderbook.get_market(market_id)?;

        // Quick check
        let quick_size = if self.config.target_size > Decimal::ZERO {
            self.config.target_size
        } else {
            Decimal::new(20, 0)
        };

        // Quick check with zero min_profit threshold (pass anything potentially profitable)
        let min_profit_per_share = Decimal::ZERO;
        let moonbag_threshold = Decimal::new(5, 3); // 0.005

        if !quick_check(
            &market,
            quick_size,
            min_profit_per_share,
            self.config.min_k,
            self.config.max_k,
            moonbag_threshold,
        ) {
            self.stats.quick_check_fail.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        self.stats.quick_check_pass.fetch_add(1, Ordering::Relaxed);

        // Analyzer uses binary search to find optimal size within liquidity constraints
        // Pass large target_size as upper bound - analyzer will find best profitable size
        let target_size = Decimal::new(10000, 0); // 10,000 shares as upper bound

        let opp = self.analyzer.find_opportunity(&market, target_size)?;

        // Budget constraint: total NO cost must fit within max_investment
        if self.config.max_investment > Decimal::ZERO
            && opp.no_cost > self.config.max_investment
        {
            debug!(
                market = %market.slug,
                no_cost = %opp.no_cost,
                max_investment = %self.config.max_investment,
                amount = %opp.amount,
                k = opp.k,
                "Rejected: exceeds budget"
            );
            return None;
        }

        // Check remaining outcomes
        if let Some(max_rem) = self.config.max_remaining_outcomes {
            if opp.remaining_outcomes.len() > max_rem as usize {
                return None;
            }
        }

        debug!(
            market = %opp.market_slug,
            k = opp.k,
            m = opp.m,
            amount = %opp.amount,
            no_cost = %opp.no_cost,
            profit = %opp.guaranteed_profit,
            moonbags = opp.moonbag_count,
            "Accepted opportunity (binary search optimal)"
        );

        self.stats
            .opportunities_found
            .fetch_add(1, Ordering::Relaxed);

        Some(opp)
    }

    /// Process an update and check for opportunities
    pub fn process_update(&mut self, token_id: &str) -> Option<MoonbagOpportunity> {
        let market_id = self.orderbook.get_market_id(token_id)?;
        self.check_market(&market_id)
    }

    /// Run a full scan across all markets (no cooldown - doesn't block WS scans)
    pub fn scan_all(&mut self) -> Vec<MoonbagOpportunity> {
        let market_ids = self.orderbook.market_ids();
        let mut opportunities = Vec::new();

        for market_id in market_ids {
            // Use no-cooldown version to avoid blocking WS-triggered scans
            if let Some(opp) = self.check_market_no_cooldown(&market_id) {
                opportunities.push(opp);
            }
        }

        // Sort by profit
        opportunities.sort_by(|a, b| b.guaranteed_profit.cmp(&a.guaranteed_profit));
        opportunities
    }
}

impl Default for Scanner {
    fn default() -> Self {
        Self::new(ScannerConfig::default())
    }
}
