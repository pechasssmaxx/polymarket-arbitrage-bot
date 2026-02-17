//! Configuration management for the moonbag trading system
//!
//! Configuration can be loaded from:
//! - Environment variables (primary)
//! - TOML config file (optional override)

use crate::error::{MoonbagError, Result};
use crate::models::ExecutionMode;
use rust_decimal::Decimal;
use serde::Deserialize;
use std::path::PathBuf;
use std::str::FromStr;

/// Main configuration struct
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    // ───────────────────────────────────────────────────────────
    // Credentials (from environment)
    // ───────────────────────────────────────────────────────────
    /// EOA private key (32 bytes hex)
    #[serde(default)]
    pub polygon_private_key: String,

    /// Gnosis Safe address (checksummed)
    #[serde(default)]
    pub polymarket_funder: String,

    /// Polygon RPC URL (Alchemy/Infura)
    #[serde(default)]
    pub polygon_rpc_url: String,

    /// CLOB API key (optional for authenticated endpoints)
    #[serde(default)]
    pub polymarket_api_key: Option<String>,

    /// CLOB API secret
    #[serde(default)]
    pub polymarket_api_secret: Option<String>,

    /// CLOB API passphrase
    #[serde(default)]
    pub polymarket_passphrase: Option<String>,

    // ───────────────────────────────────────────────────────────
    // Scanner settings
    // ───────────────────────────────────────────────────────────
    /// Execution mode (profit or farm)
    #[serde(default)]
    pub mode: ExecutionMode,

    /// Minimum profit threshold (USD)
    #[serde(default = "default_min_profit")]
    pub min_profit: Decimal,

    /// Target size per outcome (tokens)
    #[serde(default = "default_target_size")]
    pub target_size: Decimal,

    /// Maximum investment per opportunity (USD)
    #[serde(default = "default_max_investment")]
    pub max_investment: Decimal,

    /// Max remaining outcomes (None = unlimited)
    #[serde(default)]
    pub max_remaining_outcomes: Option<u8>,

    /// Minimum moonbag count required
    #[serde(default)]
    pub min_moonbags: u8,

    /// Minimum K (selected outcomes)
    #[serde(default = "default_min_k")]
    pub min_k: u8,

    /// Maximum K (None = unlimited)
    #[serde(default)]
    pub max_k: Option<u8>,

    /// Moonbag threshold (YES price below this = moonbag)
    #[serde(default = "default_moonbag_threshold")]
    pub moonbag_threshold: Decimal,

    // ───────────────────────────────────────────────────────────
    // WebSocket settings
    // ───────────────────────────────────────────────────────────
    /// WebSocket URL
    #[serde(default = "default_ws_url")]
    pub ws_url: String,

    /// Reconnect delay (ms)
    #[serde(default = "default_ws_reconnect_delay")]
    pub ws_reconnect_delay_ms: u64,

    /// Ping interval (ms)
    #[serde(default = "default_ws_ping_interval")]
    pub ws_ping_interval_ms: u64,

    /// Maximum pending tasks before backpressure
    #[serde(default = "default_max_pending_tasks")]
    pub max_pending_tasks: usize,

    /// Cooldown for book events (ms) - fast reaction to depth changes
    #[serde(default = "default_book_cooldown")]
    pub book_cooldown_ms: u64,

    /// Enable trailing-edge debounce (catch final state after cooldown)
    #[serde(default = "default_true")]
    pub enable_debounce: bool,

    // ───────────────────────────────────────────────────────────
    // Executor settings
    // ───────────────────────────────────────────────────────────
    /// Slippage tolerance for FOK orders
    #[serde(default = "default_slippage")]
    pub slippage_tolerance: Decimal,

    /// Maximum settlement wait time (seconds)
    #[serde(default = "default_settlement_wait")]
    pub settlement_max_wait_secs: u64,

    /// Settlement poll interval (ms)
    #[serde(default = "default_settlement_poll")]
    pub settlement_poll_interval_ms: u64,

    /// Pre-check minimum profit (override)
    #[serde(default)]
    pub pre_check_min_profit: Option<Decimal>,

    /// Maximum opportunity age for pre-check (seconds)
    #[serde(default = "default_max_opp_age")]
    pub max_opportunity_age_secs: f64,

    /// Maximum queue age (skip stale jobs)
    #[serde(default = "default_max_queue_age")]
    pub max_queue_age_secs: f64,

    // ───────────────────────────────────────────────────────────
    // Safe execution settings
    // ───────────────────────────────────────────────────────────
    /// First receipt timeout (seconds)
    #[serde(default = "default_receipt_first_timeout")]
    pub safe_receipt_first_timeout_secs: u64,

    /// Maximum receipt timeout (seconds)
    #[serde(default = "default_receipt_max_timeout")]
    pub safe_receipt_max_timeout_secs: u64,

    /// Receipt poll interval (seconds)
    #[serde(default = "default_receipt_poll_interval")]
    pub safe_receipt_poll_interval_secs: f64,

    // ───────────────────────────────────────────────────────────
    // Queue settings
    // ───────────────────────────────────────────────────────────
    /// Queue directory path
    #[serde(default = "default_queue_path")]
    pub queue_path: PathBuf,

    /// Cleanup stale items after (seconds)
    #[serde(default = "default_cleanup_stale")]
    pub queue_cleanup_stale_secs: u64,

    // ───────────────────────────────────────────────────────────
    // Market database
    // ───────────────────────────────────────────────────────────
    /// Path to markets JSON database
    #[serde(default = "default_markets_path")]
    pub markets_db_path: PathBuf,

    // ───────────────────────────────────────────────────────────
    // Notification settings
    // ───────────────────────────────────────────────────────────
    /// Telegram bot token
    #[serde(default)]
    pub telegram_bot_token: Option<String>,

    /// Telegram chat ID
    #[serde(default)]
    pub telegram_chat_id: Option<String>,

    // ───────────────────────────────────────────────────────────
    // Gas settings
    // ───────────────────────────────────────────────────────────
    /// Gas price in gwei
    #[serde(default = "default_gas_price")]
    pub gas_price_gwei: Decimal,

    /// POL/MATIC price in USD
    #[serde(default = "default_pol_price")]
    pub pol_usd_price: Decimal,

    /// Gas multiplier for CONVERT
    #[serde(default = "default_gas_multiplier")]
    pub convert_gas_multiplier: Decimal,

    // ───────────────────────────────────────────────────────────
    // Runtime flags
    // ───────────────────────────────────────────────────────────
    /// Dry run mode (no real trades)
    #[serde(default = "default_true")]
    pub dry_run: bool,

    /// Skip pre-check verification
    #[serde(default)]
    pub skip_precheck: bool,

    /// Verify opportunities before queueing
    #[serde(default)]
    pub verify_before_queue: bool,

    /// Enable Telegram notifications
    #[serde(default)]
    pub telegram_enabled: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// Default value functions
// ─────────────────────────────────────────────────────────────────────────────

fn default_min_profit() -> Decimal {
    Decimal::new(5, 2) // $0.05
}
fn default_target_size() -> Decimal {
    Decimal::new(100, 1) // 10.0 tokens
}
fn default_max_investment() -> Decimal {
    Decimal::new(300, 0) // $300
}
fn default_min_k() -> u8 {
    2
}
fn default_moonbag_threshold() -> Decimal {
    Decimal::new(5, 3) // 0.005
}
fn default_ws_url() -> String {
    "wss://ws-subscriptions-clob.polymarket.com/ws/market".to_string()
}
fn default_ws_reconnect_delay() -> u64 {
    1000
}
fn default_ws_ping_interval() -> u64 {
    30000
}
fn default_max_pending_tasks() -> usize {
    3000
}
fn default_book_cooldown() -> u64 {
    30 // 30ms - balance between latency and CPU usage
}
fn default_slippage() -> Decimal {
    Decimal::new(2, 2) // 0.02 = 2%
}
fn default_settlement_wait() -> u64 {
    180
}
fn default_settlement_poll() -> u64 {
    2000
}
fn default_max_opp_age() -> f64 {
    30.0
}
fn default_max_queue_age() -> f64 {
    15.0
}
fn default_receipt_first_timeout() -> u64 {
    180
}
fn default_receipt_max_timeout() -> u64 {
    600
}
fn default_receipt_poll_interval() -> f64 {
    3.0
}
fn default_queue_path() -> PathBuf {
    PathBuf::from("./queue")
}
fn default_cleanup_stale() -> u64 {
    300
}
fn default_markets_path() -> PathBuf {
    PathBuf::from("./markets.json")
}
fn default_gas_price() -> Decimal {
    Decimal::new(35, 0) // 35 gwei
}
fn default_pol_price() -> Decimal {
    Decimal::new(50, 2) // $0.50
}
fn default_gas_multiplier() -> Decimal {
    Decimal::ONE
}
fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            polygon_private_key: String::new(),
            polymarket_funder: String::new(),
            polygon_rpc_url: String::new(),
            polymarket_api_key: None,
            polymarket_api_secret: None,
            polymarket_passphrase: None,
            mode: ExecutionMode::default(),
            min_profit: default_min_profit(),
            target_size: default_target_size(),
            max_investment: default_max_investment(),
            max_remaining_outcomes: None,
            min_moonbags: 0,
            min_k: default_min_k(),
            max_k: None,
            moonbag_threshold: default_moonbag_threshold(),
            ws_url: default_ws_url(),
            ws_reconnect_delay_ms: default_ws_reconnect_delay(),
            ws_ping_interval_ms: default_ws_ping_interval(),
            max_pending_tasks: default_max_pending_tasks(),
            book_cooldown_ms: default_book_cooldown(),
            enable_debounce: true,
            slippage_tolerance: default_slippage(),
            settlement_max_wait_secs: default_settlement_wait(),
            settlement_poll_interval_ms: default_settlement_poll(),
            pre_check_min_profit: None,
            max_opportunity_age_secs: default_max_opp_age(),
            max_queue_age_secs: default_max_queue_age(),
            safe_receipt_first_timeout_secs: default_receipt_first_timeout(),
            safe_receipt_max_timeout_secs: default_receipt_max_timeout(),
            safe_receipt_poll_interval_secs: default_receipt_poll_interval(),
            queue_path: default_queue_path(),
            queue_cleanup_stale_secs: default_cleanup_stale(),
            markets_db_path: default_markets_path(),
            telegram_bot_token: None,
            telegram_chat_id: None,
            gas_price_gwei: default_gas_price(),
            pol_usd_price: default_pol_price(),
            convert_gas_multiplier: default_gas_multiplier(),
            dry_run: true,
            skip_precheck: false,
            verify_before_queue: false,
            telegram_enabled: false,
        }
    }
}

impl Config {
    /// Load configuration from environment variables
    pub fn from_env() -> Result<Self> {
        // Load .env file if present
        let _ = dotenvy::dotenv();

        let mut config = Self::default();

        // Credentials (required for live mode)
        config.polygon_private_key = std::env::var("POLYGON_PRIVATE_KEY").unwrap_or_default();
        config.polymarket_funder = std::env::var("POLYMARKET_FUNDER").unwrap_or_default();
        config.polygon_rpc_url = std::env::var("POLYGON_RPC_URL").unwrap_or_default();
        config.polymarket_api_key = std::env::var("POLYMARKET_API_KEY").ok();
        config.polymarket_api_secret = std::env::var("POLYMARKET_API_SECRET").ok();
        config.polymarket_passphrase = std::env::var("POLYMARKET_PASSPHRASE").ok();

        // Scanner settings
        if let Ok(mode) = std::env::var("MOONBAG_MODE") {
            config.mode = match mode.to_lowercase().as_str() {
                "farm" => ExecutionMode::Farm,
                _ => ExecutionMode::Profit,
            };
        }

        if let Ok(v) = std::env::var("MOONBAG_MIN_PROFIT") {
            config.min_profit = Decimal::from_str(&v).unwrap_or(default_min_profit());
        }

        if let Ok(v) = std::env::var("MOONBAG_TARGET_SIZE") {
            config.target_size = Decimal::from_str(&v).unwrap_or(default_target_size());
        }

        if let Ok(v) = std::env::var("MOONBAG_MAX_INVESTMENT") {
            config.max_investment = Decimal::from_str(&v).unwrap_or(default_max_investment());
        }

        if let Ok(v) = std::env::var("MOONBAG_MAX_REMAINING_OUTCOMES") {
            config.max_remaining_outcomes = v.parse().ok();
        }

        if let Ok(v) = std::env::var("MOONBAG_MIN_K") {
            config.min_k = v.parse().unwrap_or(default_min_k());
        }

        if let Ok(v) = std::env::var("MOONBAG_MAX_K") {
            config.max_k = v.parse().ok();
        }

        // Cooldown/debounce settings
        if let Ok(v) = std::env::var("MOONBAG_BOOK_COOLDOWN_MS") {
            config.book_cooldown_ms = v.parse().unwrap_or(default_book_cooldown());
        }

        if let Ok(v) = std::env::var("MOONBAG_ENABLE_DEBOUNCE") {
            config.enable_debounce = !matches!(v.to_lowercase().as_str(), "0" | "false" | "no");
        }

        // Pre-check settings
        if let Ok(v) = std::env::var("MOONBAG_PRE_CHECK_MIN_PROFIT") {
            config.pre_check_min_profit = Decimal::from_str(&v).ok();
        }

        // Gas settings
        if let Ok(v) = std::env::var("MOONBAG_GAS_PRICE_GWEI") {
            config.gas_price_gwei = Decimal::from_str(&v).unwrap_or(default_gas_price());
        }

        if let Ok(v) =
            std::env::var("MOONBAG_POL_USD").or_else(|_| std::env::var("MOONBAG_MATIC_USD"))
        {
            config.pol_usd_price = Decimal::from_str(&v).unwrap_or(default_pol_price());
        }

        // Telegram
        config.telegram_bot_token = std::env::var("TELEGRAM_BOT_TOKEN").ok();
        config.telegram_chat_id = std::env::var("TELEGRAM_CHAT_ID").ok();

        // Paths
        if let Ok(v) = std::env::var("MOONBAG_QUEUE_PATH") {
            config.queue_path = PathBuf::from(v);
        }

        if let Ok(v) = std::env::var("MOONBAG_MARKETS_DB") {
            config.markets_db_path = PathBuf::from(v);
        }

        // Flags
        if let Ok(v) = std::env::var("MOONBAG_DRY_RUN") {
            config.dry_run = !matches!(v.to_lowercase().as_str(), "0" | "false" | "no");
        }

        if let Ok(v) = std::env::var("MOONBAG_SKIP_PRECHECK") {
            config.skip_precheck = matches!(v.to_lowercase().as_str(), "1" | "true" | "yes");
        }

        Ok(config)
    }

    /// Validate configuration for live trading
    pub fn validate_for_live(&self) -> Result<()> {
        if self.polygon_private_key.is_empty() {
            return Err(MoonbagError::MissingConfig(
                "POLYGON_PRIVATE_KEY".to_string(),
            ));
        }
        if self.polymarket_funder.is_empty() {
            return Err(MoonbagError::MissingConfig("POLYMARKET_FUNDER".to_string()));
        }
        if self.polygon_rpc_url.is_empty() {
            return Err(MoonbagError::MissingConfig("POLYGON_RPC_URL".to_string()));
        }
        Ok(())
    }

    /// Check if Telegram is properly configured
    pub fn telegram_configured(&self) -> bool {
        self.telegram_bot_token.is_some() && self.telegram_chat_id.is_some()
    }

    /// Get CLOB host URL
    pub fn clob_host(&self) -> &str {
        "https://clob.polymarket.com"
    }

    /// Get Polygon chain ID
    pub const fn chain_id(&self) -> u64 {
        137
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert!(config.dry_run);
        assert_eq!(config.min_k, 2);
        assert_eq!(config.mode, ExecutionMode::Profit);
    }

    #[test]
    fn test_validate_for_live() {
        let mut config = Config::default();

        // Should fail - missing credentials
        assert!(config.validate_for_live().is_err());

        // Add credentials
        config.polygon_private_key = "abc123".to_string();
        config.polymarket_funder = "0x123".to_string();
        config.polygon_rpc_url = "https://polygon.example.com".to_string();

        // Should pass
        assert!(config.validate_for_live().is_ok());
    }
}
