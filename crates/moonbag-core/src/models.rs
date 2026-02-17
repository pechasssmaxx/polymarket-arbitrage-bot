//! Data models for the moonbag trading system
//!
//! Ported from Python: `moonbag/models.py`
//!
//! Key concepts:
//! - `StoredOutcome` / `StoredMarket`: Static data from markets.json
//! - `OutcomeState`: Single outcome with live orderbook
//! - `MarketState`: Full neg_risk market (multiple outcomes)
//! - `MoonbagOpportunity`: Trade opportunity ready for execution

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ─────────────────────────────────────────────────────────────────────────────
// Order Size Constants & Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Minimum shares for any order (Polymarket requirement)
pub const MIN_SHARES: Decimal = Decimal::from_parts(5, 0, 0, false, 0);

/// Minimum dollar value for any order (Polymarket requirement)
pub const MIN_DOLLAR_VALUE: Decimal = Decimal::from_parts(1, 0, 0, false, 0);

/// Calculate minimum order size based on price.
///
/// Polymarket marketable orders (FOK/IOC) require $1 minimum value.
/// No minimum share count - just need shares × price >= $1.
///
/// So minimum = ceil($1/price)
///
/// # Arguments
/// * `price` - The price per share (0 < price < 1)
///
/// # Returns
/// Minimum number of shares required for a valid order
#[inline]
pub fn calculate_min_order_size(price: Decimal) -> Decimal {
    if price <= Decimal::ZERO {
        return MIN_SHARES; // fallback for invalid price
    }
    (MIN_DOLLAR_VALUE / price).ceil()
}

// ─────────────────────────────────────────────────────────────────────────────
// StoredOutcome / StoredMarket (from markets.json)
// ─────────────────────────────────────────────────────────────────────────────

/// Static data for a single outcome from markets.json
///
/// CRITICAL: `alphabetical_index` is the onchain CONVERT index (bit position for index_set),
/// not a human alphabetical sort.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredOutcome {
    /// Question text for display
    pub question: String,
    /// Used for CLOB trading
    pub condition_id: String,
    /// YES token ID (ERC1155 - decimal string)
    pub yes_token_id: String,
    /// NO token ID (ERC1155 - decimal string)
    pub no_token_id: String,
    /// Onchain CONVERT index for index_set bit position
    pub alphabetical_index: u8,
}

/// Static data for a neg_risk market from markets.json
///
/// Contains all information needed for CONVERT execution:
/// - neg_risk_market_id for convertPositions()
/// - outcomes with CONVERT indices for index_set
/// - token IDs for orderbook subscription
/// - convertible_indices: which outcomes can be used for CONVERT
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredMarket {
    /// bytes32 hex for convertPositions()
    pub neg_risk_market_id: String,
    /// Event title
    pub title: String,
    /// URL slug
    pub slug: String,
    /// Outcomes with CONVERT indices
    pub outcomes: Vec<StoredOutcome>,
    /// Total outcome count
    #[serde(default)]
    pub outcome_count: usize,
    /// When data was collected
    #[serde(default)]
    pub collected_at: String,
    /// Which outcomes are convertible (verified onchain)
    #[serde(default)]
    pub convertible_indices: Vec<u8>,
    // Optional Gamma metadata
    #[serde(default)]
    pub event_id: Option<String>,
    #[serde(default)]
    pub active: Option<bool>,
    #[serde(default)]
    pub closed: Option<bool>,
    #[serde(default)]
    pub liquidity: Option<f64>,
    #[serde(default)]
    pub volume: Option<f64>,
    #[serde(default)]
    pub volume24hr: Option<f64>,
    #[serde(default)]
    pub open_interest: Option<f64>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

impl StoredMarket {
    /// Convert to runtime MarketState with empty orderbooks
    pub fn to_market_state(&self) -> MarketState {
        let mut state = MarketState::new(
            self.title.clone(),
            self.slug.clone(),
            self.neg_risk_market_id.clone(),
        );
        state.convertible_indices = self.convertible_indices.clone();

        for outcome in &self.outcomes {
            let os = OutcomeState::new(
                outcome.question.clone(),
                outcome.condition_id.clone(),
                outcome.yes_token_id.clone(),
                outcome.no_token_id.clone(),
                outcome.alphabetical_index,
            );
            state.upsert_outcome(os);
        }
        state
    }

    /// Get all token IDs (YES and NO) for WebSocket subscription
    pub fn get_all_token_ids(&self) -> Vec<String> {
        self.outcomes
            .iter()
            .flat_map(|o| vec![o.yes_token_id.clone(), o.no_token_id.clone()])
            .collect()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Orderbook Level
// ─────────────────────────────────────────────────────────────────────────────

/// Single price level in an orderbook: (price, size)
pub type PriceLevel = (Decimal, Decimal);

// ─────────────────────────────────────────────────────────────────────────────
// OutcomeState
// ─────────────────────────────────────────────────────────────────────────────

/// State for a single outcome in a neg_risk market.
///
/// Each outcome has:
/// - YES and NO tokens (ERC1155)
/// - Live orderbook from WebSocket
/// - Calculated VWAP prices
///
/// NOTE: `convert_index` is the onchain index used by NegRiskAdapter for index_set bits.
#[derive(Debug, Clone)]
pub struct OutcomeState {
    // Identifiers
    /// Full question text (display/debug)
    pub question: String,
    /// Used for trading on CLOB
    pub condition_id: String,
    /// YES token ID (ERC1155)
    pub yes_token_id: String,
    /// NO token ID (ERC1155)
    pub no_token_id: String,
    /// Index (0..M-1) for CONVERT index_set bit
    pub convert_index: u8,

    // Orderbook data (updated via WebSocket)
    // Format: [(price, size), ...] sorted by price
    /// Sell NO offers (ascending by price)
    pub no_asks: Vec<PriceLevel>,
    /// Buy NO offers (descending by price)
    pub no_bids: Vec<PriceLevel>,
    /// Sell YES offers (ascending by price)
    pub yes_asks: Vec<PriceLevel>,
    /// Buy YES offers (descending by price)
    pub yes_bids: Vec<PriceLevel>,

    // Calculated VWAPs (set by analyzer)
    /// Cost to BUY NO (from asks)
    pub no_vwap: Option<Decimal>,
    /// Revenue from SELL YES (from bids)
    pub yes_vwap: Option<Decimal>,

    // Market prices from /price endpoint
    //
    // NOTE: On Polymarket CLOB, `/price?side=sell` corresponds to the best ask (price to buy now),
    // and `/price?side=buy` corresponds to the best bid (price to sell now).
    /// Best ask price to BUY NO (from /price?side=sell)
    pub no_market_price: Option<Decimal>,
    /// Best ask price to BUY YES (from /price?side=sell)
    pub yes_market_price: Option<Decimal>,

    // Quote-only best prices from WebSocket `price_changes` (no size).
    // These MUST NOT be used for VWAP/liquidity calculations.
    pub ws_best_no_bid: Option<Decimal>,
    pub ws_best_no_ask: Option<Decimal>,
    pub ws_best_yes_bid: Option<Decimal>,
    pub ws_best_yes_ask: Option<Decimal>,

    // Metadata
    pub last_update: DateTime<Utc>,
}

impl OutcomeState {
    /// Create a new outcome state with empty orderbooks
    pub fn new(
        question: String,
        condition_id: String,
        yes_token_id: String,
        no_token_id: String,
        convert_index: u8,
    ) -> Self {
        Self {
            question,
            condition_id,
            yes_token_id,
            no_token_id,
            convert_index,
            no_asks: Vec::new(),
            no_bids: Vec::new(),
            yes_asks: Vec::new(),
            yes_bids: Vec::new(),
            no_vwap: None,
            yes_vwap: None,
            no_market_price: None,
            yes_market_price: None,
            ws_best_no_bid: None,
            ws_best_no_ask: None,
            ws_best_yes_bid: None,
            ws_best_yes_ask: None,
            last_update: Utc::now(),
        }
    }

    /// Get the effective NO price for analysis (BUYING NO)
    /// Uses ONLY real orderbook data - no_asks sorted ASC, first = best (lowest) price
    /// Ignores no_market_price as it may contain phantom prices from stale /price API
    #[inline]
    pub fn effective_no_price(&self) -> Option<Decimal> {
        self.no_asks.first().map(|(p, _)| *p)
    }

    /// Get the effective YES price for analysis (SELLING YES)
    /// Uses ONLY real orderbook data - yes_bids sorted DESC, first = best (highest) price
    #[inline]
    pub fn effective_yes_price(&self) -> Option<Decimal> {
        self.yes_bids.first().map(|(p, _)| *p)
    }

    /// Check if there is liquidity to buy NO (NO asks exist)
    #[inline]
    pub fn has_no_liquidity(&self) -> bool {
        !self.no_asks.is_empty()
    }

    /// Size available at best NO ask level
    #[inline]
    pub fn no_best_ask_size(&self) -> Decimal {
        self.no_asks
            .first()
            .map(|(_, size)| *size)
            .unwrap_or(Decimal::ZERO)
    }

    /// Update market prices from /price endpoint
    pub fn update_market_prices(&mut self, no_price: Option<Decimal>, yes_price: Option<Decimal>) {
        self.no_market_price = no_price;
        self.yes_market_price = yes_price;
        self.last_update = Utc::now();
    }

    /// Best (lowest) price to buy NO
    #[inline]
    pub fn best_no_ask(&self) -> Decimal {
        self.no_asks
            .first()
            .map(|(price, _)| *price)
            .unwrap_or(Decimal::ONE)
    }

    /// Best (highest) price to sell YES
    #[inline]
    pub fn best_yes_bid(&self) -> Decimal {
        self.yes_bids
            .first()
            .map(|(price, _)| *price)
            .unwrap_or(Decimal::ZERO)
    }

    /// Total NO ask liquidity in USD
    pub fn no_liquidity(&self) -> Decimal {
        self.no_asks.iter().map(|(price, size)| price * size).sum()
    }

    /// Total YES bid liquidity in USD
    pub fn yes_liquidity(&self) -> Decimal {
        self.yes_bids.iter().map(|(price, size)| price * size).sum()
    }

    /// Update the NO orderbook
    pub fn update_no_book(&mut self, asks: Vec<PriceLevel>, bids: Vec<PriceLevel>) {
        self.no_asks = asks;
        self.no_bids = bids;
        self.last_update = Utc::now();
        // Invalidate cached VWAP
        self.no_vwap = None;
    }

    /// Update the YES orderbook
    pub fn update_yes_book(&mut self, asks: Vec<PriceLevel>, bids: Vec<PriceLevel>) {
        self.yes_asks = asks;
        self.yes_bids = bids;
        self.last_update = Utc::now();
        // Invalidate cached VWAP
        self.yes_vwap = None;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// MarketState
// ─────────────────────────────────────────────────────────────────────────────

/// State for a complete neg_risk market (multi-outcome event).
///
/// Example: "Who will win the election?"
/// - M outcomes (candidates)
/// - One neg_risk_market_id for CONVERT
/// - Multiple condition_ids for trading
#[derive(Debug, Clone)]
pub struct MarketState {
    // Identifiers
    /// Event title
    pub title: String,
    /// URL slug
    pub slug: String,
    /// bytes32 hex - THE KEY for convertPositions()!
    pub neg_risk_market_id: String,

    /// Outcomes (keyed by yes_token_id for fast lookup from WebSocket)
    pub outcomes: HashMap<String, OutcomeState>,

    /// Convertibility - which CONVERT indices can be used
    /// If empty, all outcomes are assumed convertible (legacy behavior)
    pub convertible_indices: Vec<u8>,

    // Metadata
    pub last_update: DateTime<Utc>,
}

impl MarketState {
    /// Create a new market state
    pub fn new(title: String, slug: String, neg_risk_market_id: String) -> Self {
        Self {
            title,
            slug,
            neg_risk_market_id,
            outcomes: HashMap::new(),
            convertible_indices: Vec::new(),
            last_update: Utc::now(),
        }
    }

    /// Total number of outcomes (M)
    #[inline]
    pub fn outcome_count(&self) -> usize {
        self.outcomes.len()
    }

    /// Get outcomes sorted alphabetically by question (for CONVERT index)
    pub fn get_sorted_outcomes(&self) -> Vec<&OutcomeState> {
        let mut outcomes: Vec<_> = self.outcomes.values().collect();
        outcomes.sort_by(|a, b| a.question.cmp(&b.question));
        outcomes
    }

    /// Find outcome by NO token ID
    pub fn get_outcome_by_no_token(&self, no_token_id: &str) -> Option<&OutcomeState> {
        self.outcomes
            .values()
            .find(|o| o.no_token_id == no_token_id)
    }

    /// Find outcome by NO token ID (mutable)
    pub fn get_outcome_by_no_token_mut(&mut self, no_token_id: &str) -> Option<&mut OutcomeState> {
        self.outcomes
            .values_mut()
            .find(|o| o.no_token_id == no_token_id)
    }

    /// Add or update an outcome
    pub fn upsert_outcome(&mut self, outcome: OutcomeState) {
        let key = outcome.yes_token_id.clone();
        self.outcomes.insert(key, outcome);
        self.last_update = Utc::now();
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Selected/Remaining Outcomes (for serialization)
// ─────────────────────────────────────────────────────────────────────────────

/// Selected outcome data (buying NO on these)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectedOutcome {
    pub condition_id: String,
    pub no_token_id: String,
    pub question: String,
    pub no_price: Decimal,
    pub no_vwap: Decimal,
    pub no_impact_price: Decimal,
    pub alphabetical_index: u8,
}

/// Remaining outcome data (getting YES moonbags on these)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemainingOutcome {
    pub condition_id: String,
    pub yes_token_id: String,
    pub no_token_id: String,
    pub question: String,
    pub yes_bid: Decimal,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub yes_vwap: Option<Decimal>,
    pub alphabetical_index: u8,
    pub convertible: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// Execution Mode & Status
// ─────────────────────────────────────────────────────────────────────────────

/// Execution mode for opportunities
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionMode {
    /// Maximize immediate USDC profit
    #[default]
    Profit,
    /// Accumulate moonbags at near-breakeven prices
    Farm,
}

impl std::fmt::Display for ExecutionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Profit => write!(f, "profit"),
            Self::Farm => write!(f, "farm"),
        }
    }
}

/// Status of an opportunity
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum OpportunityStatus {
    #[default]
    Pending,
    Executing,
    Done,
    Failed,
}

// ─────────────────────────────────────────────────────────────────────────────
// MoonbagOpportunity
// ─────────────────────────────────────────────────────────────────────────────

/// A validated moonbag farming opportunity ready for execution.
///
/// Contains ALL data needed for the executor:
/// 1. Market identifiers for CONVERT
/// 2. Trade parameters
/// 3. Selected outcomes (buying NO)
/// 4. Remaining outcomes (getting YES moonbags)
/// 5. P&L calculations
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoonbagOpportunity {
    // ───────────────────────────────────────────────────────────
    // Market identifiers
    // ───────────────────────────────────────────────────────────
    /// bytes32 hex for convertPositions()
    pub neg_risk_market_id: String,
    /// Human-readable title
    pub market_title: String,
    /// URL slug
    pub market_slug: String,

    // ───────────────────────────────────────────────────────────
    // Trade parameters
    // ───────────────────────────────────────────────────────────
    /// Number of NO positions to buy
    pub k: u8,
    /// Total outcomes in market
    pub m: u8,
    /// Bitmask for CONVERT (onchain index order, supports up to 128 outcomes)
    pub index_set: u128,
    /// Size per outcome (in tokens)
    pub amount: Decimal,
    /// Size in USDC decimals (x 10^6)
    pub amount_raw: u64,

    // ───────────────────────────────────────────────────────────
    // Outcomes
    // ───────────────────────────────────────────────────────────
    /// Selected outcomes (buying NO on these)
    pub selected_outcomes: Vec<SelectedOutcome>,
    /// Remaining outcomes (getting YES moonbags on these)
    pub remaining_outcomes: Vec<RemainingOutcome>,

    // ───────────────────────────────────────────────────────────
    // Financials (all calculated with VWAP)
    // ───────────────────────────────────────────────────────────
    /// Total cost to buy NO tokens
    pub no_cost: Decimal,
    /// USDC from CONVERT = (K-1) * amount
    pub usdc_return: Decimal,
    /// Revenue from selling high-priced YES moonbags
    pub yes_revenue: Decimal,

    // ───────────────────────────────────────────────────────────
    // Gas estimation (dynamic based on M)
    // ───────────────────────────────────────────────────────────
    /// Gas units for CONVERT (500k + M*100k)
    pub estimated_gas: u64,
    /// Estimated gas cost in USD
    pub gas_cost: Decimal,

    // ───────────────────────────────────────────────────────────
    // P&L
    // ───────────────────────────────────────────────────────────
    /// Conservative profit estimate (USDC + sellable YES) - no_cost - gas
    pub guaranteed_profit: Decimal,
    /// Number of TRUE moonbags (kept, not sold)
    pub moonbag_count: u8,
    /// Questions of moonbag outcomes
    pub moonbag_names: Vec<String>,

    // ───────────────────────────────────────────────────────────
    // Metadata
    // ───────────────────────────────────────────────────────────
    pub timestamp: DateTime<Utc>,
    #[serde(default)]
    pub status: OpportunityStatus,

    // ───────────────────────────────────────────────────────────
    // Execution controls (optional; used by "farm" mode)
    // ───────────────────────────────────────────────────────────
    #[serde(default)]
    pub mode: ExecutionMode,
    /// Run up to N cycles back-to-back (best-effort)
    #[serde(default = "default_max_cycles")]
    pub max_cycles: u8,
    /// Stop further cycles if cumulative profit <= -max_total_loss (0 = ignore)
    #[serde(default)]
    pub max_total_loss: Decimal,
    /// Override executor pre-check threshold for this job
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_check_min_profit: Option<Decimal>,

    // ───────────────────────────────────────────────────────────
    // Risk/behavior toggles for executor
    // ───────────────────────────────────────────────────────────
    /// If False, abort on partial fills (safer for farming fixed index_set)
    #[serde(default = "default_true")]
    pub allow_partial_k: bool,
    /// If >0, cap convert amount per cycle (prevents converting old inventory)
    #[serde(default)]
    pub convert_cap: Decimal,
    /// Auto-sell sellable YES after convert
    #[serde(default = "default_true")]
    pub sell_yes: bool,
    /// Only auto-sell YES with bid >= this
    #[serde(default = "default_sell_yes_min_bid")]
    pub sell_yes_min_bid: Decimal,
    /// If True, sell at most `converted_amount` per outcome
    #[serde(default)]
    pub sell_yes_cap_to_converted: bool,
}

fn default_max_cycles() -> u8 {
    1
}
fn default_true() -> bool {
    true
}
fn default_sell_yes_min_bid() -> Decimal {
    Decimal::new(5, 3) // 0.005
}

impl MoonbagOpportunity {
    /// Average NO price across selected outcomes
    pub fn avg_no_price(&self) -> Decimal {
        if self.selected_outcomes.is_empty() {
            return Decimal::ZERO;
        }
        let sum: Decimal = self.selected_outcomes.iter().map(|o| o.no_vwap).sum();
        sum / Decimal::from(self.selected_outcomes.len())
    }

    /// Theoretical breakeven price = (K-1)/K
    pub fn breakeven_price(&self) -> Decimal {
        if self.k == 0 {
            return Decimal::ZERO;
        }
        Decimal::from(self.k - 1) / Decimal::from(self.k)
    }

    /// How much better than breakeven (positive = profit)
    pub fn edge(&self) -> Decimal {
        self.breakeven_price() - self.avg_no_price()
    }

    /// Profit before gas (USDC return + sellable YES - NO cost)
    pub fn gross_profit(&self) -> Decimal {
        (self.usdc_return + self.yes_revenue) - self.no_cost
    }

    /// USDC-only edge (ignores YES sales + gas)
    pub fn usdc_only_profit(&self) -> Decimal {
        self.usdc_return - self.no_cost
    }
}

impl std::fmt::Display for MoonbagOpportunity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "MoonbagOpportunity(\n\
             \x20 market={:.40}...\n\
             \x20 K={}, M={}, index_set={} (bin: {:b})\n\
             \x20 amount={:.2} tokens\n\
             \x20 avg_no={:.4}, breakeven={:.4}, edge={:+.4}\n\
             \x20 no_cost=${:.2}, usdc_return=${:.2}, yes_revenue=${:.2}\n\
             \x20 profit=${:.2}, moonbags={}: {:?}\n\
             )",
            self.market_title,
            self.k,
            self.m,
            self.index_set,
            self.index_set,
            self.amount,
            self.avg_no_price(),
            self.breakeven_price(),
            self.edge(),
            self.no_cost,
            self.usdc_return,
            self.yes_revenue,
            self.guaranteed_profit,
            self.moonbag_count,
            self.moonbag_names
        )
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// ExecutionResult
// ─────────────────────────────────────────────────────────────────────────────

/// Result of executing an opportunity
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionResult {
    pub success: bool,
    #[serde(default)]
    pub profit: Decimal,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_hash: Option<String>,
    #[serde(default)]
    pub notes: Vec<String>,
    /// Was full K not filled?
    #[serde(default)]
    pub partial_k: bool,
    /// Stage where execution stopped
    #[serde(default)]
    pub stage: String,
    /// Does this failure require manual wallet intervention?
    #[serde(default)]
    pub needs_manual_intervention: bool,
    /// Amount actually converted
    #[serde(default)]
    pub converted_amount: Decimal,
}

impl Default for ExecutionResult {
    fn default() -> Self {
        Self {
            success: false,
            profit: Decimal::ZERO,
            error: None,
            tx_hash: None,
            notes: Vec::new(),
            partial_k: false,
            stage: String::new(),
            needs_manual_intervention: false,
            converted_amount: Decimal::ZERO,
        }
    }
}

impl ExecutionResult {
    /// Create a successful result
    pub fn success(profit: Decimal, tx_hash: Option<String>) -> Self {
        Self {
            success: true,
            profit,
            tx_hash,
            stage: "done".to_string(),
            ..Default::default()
        }
    }

    /// Create a failed result
    pub fn failure(error: impl Into<String>, stage: impl Into<String>) -> Self {
        Self {
            success: false,
            error: Some(error.into()),
            stage: stage.into(),
            ..Default::default()
        }
    }

    /// Create a failed result that needs manual intervention
    pub fn failure_manual(error: impl Into<String>, stage: impl Into<String>) -> Self {
        Self {
            success: false,
            error: Some(error.into()),
            stage: stage.into(),
            needs_manual_intervention: true,
            ..Default::default()
        }
    }

    /// Add a note
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_outcome_state_best_prices() {
        let mut outcome = OutcomeState::new(
            "Test Question".to_string(),
            "cond123".to_string(),
            "yes123".to_string(),
            "no123".to_string(),
            0,
        );

        // Empty books should return defaults
        assert_eq!(outcome.best_no_ask(), Decimal::ONE);
        assert_eq!(outcome.best_yes_bid(), Decimal::ZERO);

        // Add some levels
        outcome.no_asks = vec![(dec!(0.45), dec!(100.0)), (dec!(0.50), dec!(200.0))];
        outcome.yes_bids = vec![(dec!(0.55), dec!(150.0)), (dec!(0.50), dec!(100.0))];

        assert_eq!(outcome.best_no_ask(), dec!(0.45));
        assert_eq!(outcome.best_yes_bid(), dec!(0.55));
    }

    #[test]
    fn test_opportunity_calculations() {
        let opp = MoonbagOpportunity {
            neg_risk_market_id: "0x123".to_string(),
            market_title: "Test Market".to_string(),
            market_slug: "test-market".to_string(),
            k: 3,
            m: 5,
            index_set: 7,
            amount: dec!(100.0),
            amount_raw: 100_000_000,
            selected_outcomes: vec![
                SelectedOutcome {
                    condition_id: "c1".to_string(),
                    no_token_id: "n1".to_string(),
                    question: "Q1".to_string(),
                    no_price: dec!(0.60),
                    no_vwap: dec!(0.60),
                    no_impact_price: dec!(0.61),
                    alphabetical_index: 0,
                },
                SelectedOutcome {
                    condition_id: "c2".to_string(),
                    no_token_id: "n2".to_string(),
                    question: "Q2".to_string(),
                    no_price: dec!(0.65),
                    no_vwap: dec!(0.65),
                    no_impact_price: dec!(0.66),
                    alphabetical_index: 1,
                },
                SelectedOutcome {
                    condition_id: "c3".to_string(),
                    no_token_id: "n3".to_string(),
                    question: "Q3".to_string(),
                    no_price: dec!(0.70),
                    no_vwap: dec!(0.70),
                    no_impact_price: dec!(0.71),
                    alphabetical_index: 2,
                },
            ],
            remaining_outcomes: vec![],
            no_cost: dec!(195.0),
            usdc_return: dec!(200.0),
            yes_revenue: dec!(10.0),
            estimated_gas: 1_500_000,
            gas_cost: dec!(0.50),
            guaranteed_profit: dec!(14.50),
            moonbag_count: 2,
            moonbag_names: vec!["Q4".to_string(), "Q5".to_string()],
            timestamp: Utc::now(),
            status: OpportunityStatus::Pending,
            mode: ExecutionMode::Profit,
            max_cycles: 1,
            max_total_loss: Decimal::ZERO,
            pre_check_min_profit: None,
            allow_partial_k: true,
            convert_cap: Decimal::ZERO,
            sell_yes: true,
            sell_yes_min_bid: dec!(0.005),
            sell_yes_cap_to_converted: false,
        };

        // avg_no_price = (0.60 + 0.65 + 0.70) / 3 = 0.65
        assert_eq!(opp.avg_no_price(), dec!(0.65));

        // breakeven = (3-1)/3 = 0.666...
        let breakeven = opp.breakeven_price();
        assert!(breakeven > dec!(0.66) && breakeven < dec!(0.67));

        // gross_profit = (200 + 10) - 195 = 15
        assert_eq!(opp.gross_profit(), dec!(15.0));

        // usdc_only = 200 - 195 = 5
        assert_eq!(opp.usdc_only_profit(), dec!(5.0));
    }

    #[test]
    fn test_opportunity_serialization() {
        let opp = MoonbagOpportunity {
            neg_risk_market_id: "0x123".to_string(),
            market_title: "Test".to_string(),
            market_slug: "test".to_string(),
            k: 2,
            m: 3,
            index_set: 3,
            amount: dec!(50.0),
            amount_raw: 50_000_000,
            selected_outcomes: vec![],
            remaining_outcomes: vec![],
            no_cost: dec!(90.0),
            usdc_return: dec!(50.0),
            yes_revenue: dec!(45.0),
            estimated_gas: 1_000_000,
            gas_cost: dec!(0.35),
            guaranteed_profit: dec!(4.65),
            moonbag_count: 1,
            moonbag_names: vec!["Moon".to_string()],
            timestamp: Utc::now(),
            status: OpportunityStatus::Pending,
            mode: ExecutionMode::Farm,
            max_cycles: 5,
            max_total_loss: dec!(10.0),
            pre_check_min_profit: Some(dec!(0.5)),
            allow_partial_k: false,
            convert_cap: dec!(100.0),
            sell_yes: true,
            sell_yes_min_bid: dec!(0.01),
            sell_yes_cap_to_converted: true,
        };

        let json = serde_json::to_string(&opp).unwrap();
        let decoded: MoonbagOpportunity = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded.k, 2);
        assert_eq!(decoded.mode, ExecutionMode::Farm);
        assert_eq!(decoded.max_cycles, 5);
    }
}
