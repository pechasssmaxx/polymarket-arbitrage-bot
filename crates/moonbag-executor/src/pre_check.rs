//! Pre-execution verification
//!
//! Verifies opportunities are still valid before execution by:
//! 1. Checking opportunity age
//! 2. Re-fetching orderbook via REST
//! 3. Recalculating VWAP and profit
//! 4. Verifying minimum liquidity exists

use moonbag_core::models::{MoonbagOpportunity, PriceLevel};
use moonbag_core::{Config, MoonbagError, Result};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Pre-check configuration
#[derive(Debug, Clone)]
pub struct PreCheckConfig {
    /// Maximum opportunity age in seconds
    pub max_age_secs: f64,
    /// Minimum profit threshold (can be negative for farm mode)
    pub min_profit: Decimal,
    /// REST API timeout
    pub timeout: Duration,
    /// CLOB host URL
    pub clob_host: String,
    /// Liquidity margin percentage (require this much extra liquidity)
    /// Default 0.10 = require 110% of needed amount
    pub liquidity_margin_pct: Decimal,
}

impl Default for PreCheckConfig {
    fn default() -> Self {
        Self {
            max_age_secs: 30.0,
            min_profit: Decimal::new(5, 2), // $0.05
            timeout: Duration::from_secs(10),
            clob_host: "https://clob.polymarket.com".to_string(),
            liquidity_margin_pct: Decimal::ZERO, // No margin - FOK orders handle insufficient liquidity
        }
    }
}

impl From<&Config> for PreCheckConfig {
    fn from(config: &Config) -> Self {
        Self {
            max_age_secs: config.max_opportunity_age_secs,
            min_profit: config.pre_check_min_profit.unwrap_or(config.min_profit),
            timeout: Duration::from_secs(10),
            clob_host: config.clob_host().to_string(),
            liquidity_margin_pct: Decimal::ZERO, // No margin - FOK orders handle insufficient liquidity for safety
        }
    }
}

/// Result of pre-check verification
#[derive(Debug, Clone)]
pub struct PreCheckResult {
    pub valid: bool,
    pub reason: Option<String>,
    pub current_profit: Decimal,
    pub original_profit: Decimal,
    pub profit_delta: Decimal,
    pub age_secs: f64,
}

impl PreCheckResult {
    pub fn valid(current_profit: Decimal, original_profit: Decimal, age_secs: f64) -> Self {
        Self {
            valid: true,
            reason: None,
            current_profit,
            original_profit,
            profit_delta: current_profit - original_profit,
            age_secs,
        }
    }

    pub fn invalid(reason: impl Into<String>) -> Self {
        Self {
            valid: false,
            reason: Some(reason.into()),
            current_profit: Decimal::ZERO,
            original_profit: Decimal::ZERO,
            profit_delta: Decimal::ZERO,
            age_secs: 0.0,
        }
    }
}

#[derive(Debug, Serialize)]
struct BooksRequest {
    token_id: String,
}

/// Book response from CLOB API (/books batch)
#[derive(Debug, Deserialize)]
struct BookResponse {
    asset_id: Option<String>,
    #[serde(alias = "tokenId")]
    token_id: Option<String>,
    bids: Option<Vec<BookLevel>>,
    asks: Option<Vec<BookLevel>>,
}

#[derive(Debug, Deserialize)]
struct BookLevel {
    price: String,
    size: String,
}

impl BookLevel {
    fn to_price_level(&self) -> Option<PriceLevel> {
        let price: Decimal = self.price.parse().ok()?;
        let size: Decimal = self.size.parse().ok()?;
        if size <= Decimal::ZERO {
            return None;
        }
        Some((price, size))
    }
}

/// Pre-execution checker
pub struct PreChecker {
    config: PreCheckConfig,
    client: reqwest::Client,
}

impl PreChecker {
    /// Create a new pre-checker
    pub fn new(config: PreCheckConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .expect("Failed to create HTTP client");

        Self { config, client }
    }

    /// Create from global config
    pub fn from_config(config: &Config) -> Self {
        Self::new(PreCheckConfig::from(config))
    }

    /// Verify an opportunity is still valid
    pub async fn verify(&self, opp: &MoonbagOpportunity) -> Result<PreCheckResult> {
        // 1. Check age
        let age_secs = (chrono::Utc::now() - opp.timestamp).num_milliseconds() as f64 / 1000.0;

        if age_secs > self.config.max_age_secs {
            return Err(MoonbagError::OpportunityTooOld {
                age_secs,
                max_secs: self.config.max_age_secs,
            });
        }

        debug!(
            age_secs = %age_secs,
            max_secs = %self.config.max_age_secs,
            "Opportunity age check passed"
        );

        // 2. Fetch current orderbooks for selected NO + remaining YES (batch)
        let mut token_ids: Vec<String> =
            Vec::with_capacity(opp.selected_outcomes.len() + opp.remaining_outcomes.len());
        for selected in &opp.selected_outcomes {
            token_ids.push(selected.no_token_id.clone());
        }
        for remaining in &opp.remaining_outcomes {
            if remaining.convertible {
                token_ids.push(remaining.yes_token_id.clone());
            }
        }
        token_ids.sort();
        token_ids.dedup();

        let books = self.fetch_books(&token_ids).await?;

        // 3. Recalculate NO cost using fresh asks with per-outcome liquidity check
        let mut total_no_cost = Decimal::ZERO;
        let required_with_margin =
            opp.amount * (Decimal::ONE + self.config.liquidity_margin_pct);
        let mut outcomes_checked = 0;
        let mut outcomes_passed = 0;

        info!(
            k = opp.k,
            amount = %opp.amount,
            required_with_margin = %required_with_margin,
            margin_pct = %self.config.liquidity_margin_pct,
            "Pre-check: verifying liquidity for {} selected outcomes",
            opp.selected_outcomes.len()
        );

        for (idx, selected) in opp.selected_outcomes.iter().enumerate() {
            outcomes_checked += 1;
            let book = books.get(&selected.no_token_id);

            // Calculate VWAP for required amount
            let mut asks: Vec<PriceLevel> = book
                .and_then(|b| b.asks.as_ref())
                .map(|v| {
                    v.iter()
                        .filter_map(|l| l.to_price_level())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            asks.sort_by(|a, b| a.0.cmp(&b.0));

            let (vwap, available) = calculate_vwap(&asks, opp.amount);

            // Check liquidity with margin
            if available < required_with_margin {
                warn!(
                    outcome_idx = idx,
                    question = %selected.question,
                    no_token_id = %selected.no_token_id,
                    available = %available,
                    needed = %opp.amount,
                    required_with_margin = %required_with_margin,
                    "Pre-check FAILED: insufficient liquidity on outcome"
                );
                return Err(MoonbagError::LiquidityInsufficient {
                    available,
                    needed: required_with_margin,
                });
            }

            outcomes_passed += 1;
            debug!(
                outcome_idx = idx,
                question = %selected.question,
                available = %available,
                vwap = %vwap,
                "Pre-check: outcome liquidity OK"
            );

            total_no_cost += vwap * opp.amount;
        }

        info!(
            checked = outcomes_checked,
            passed = outcomes_passed,
            "Pre-check: all {} outcomes passed liquidity check",
            opp.k
        );

        // 4. Fetch current YES bids for sellable outcomes
        let mut total_yes_revenue = Decimal::ZERO;

        for remaining in &opp.remaining_outcomes {
            if !remaining.convertible {
                continue;
            }

            let Some(book) = books.get(&remaining.yes_token_id) else {
                // Missing/empty book means no sellable bids; treat revenue as 0.
                continue;
            };

            let mut bids: Vec<PriceLevel> = book
                .bids
                .as_ref()
                .map(|v| {
                    v.iter()
                        .filter_map(|l| l.to_price_level())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            bids.sort_by(|a, b| b.0.cmp(&a.0));

            if let Some((price, _)) = bids.first() {
                // Conservative: only count if bid is meaningful
                if *price >= opp.sell_yes_min_bid {
                    let (vwap, _) = calculate_vwap_from_bids(&bids, opp.amount);
                    total_yes_revenue += vwap * opp.amount;
                }
            }
        }

        // 5. Calculate current profit
        let usdc_return = Decimal::from(opp.k - 1) * opp.amount;
        let current_profit = (usdc_return + total_yes_revenue) - total_no_cost - opp.gas_cost;

        debug!(
            no_cost = %total_no_cost,
            usdc_return = %usdc_return,
            yes_revenue = %total_yes_revenue,
            gas_cost = %opp.gas_cost,
            current_profit = %current_profit,
            "Recalculated profit"
        );

        // 6. Check minimum profit threshold
        let min_profit = opp.pre_check_min_profit.unwrap_or(self.config.min_profit);

        if current_profit < min_profit {
            return Err(MoonbagError::ProfitDegraded {
                current: current_profit,
                min: min_profit,
            });
        }

        info!(
            current_profit = %current_profit,
            original_profit = %opp.guaranteed_profit,
            delta = %(current_profit - opp.guaranteed_profit),
            "Pre-check passed"
        );

        Ok(PreCheckResult::valid(
            current_profit,
            opp.guaranteed_profit,
            age_secs,
        ))
    }

    /// Fetch orderbooks for tokens (batch)
    async fn fetch_books(&self, token_ids: &[String]) -> Result<HashMap<String, BookResponse>> {
        if token_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let url = format!("{}/books", self.config.clob_host);
        let body: Vec<BooksRequest> = token_ids
            .iter()
            .map(|token_id| BooksRequest {
                token_id: token_id.clone(),
            })
            .collect();

        let response = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| MoonbagError::ClobApi {
                message: format!("Books fetch failed: {}", e),
                status: None,
            })?;

        let status = response.status();
        if !status.is_success() {
            return Err(MoonbagError::ClobApi {
                message: format!("Books fetch error: {}", status),
                status: Some(status.as_u16()),
            });
        }

        let books: Vec<BookResponse> =
            response.json().await.map_err(|e| MoonbagError::ClobApi {
                message: format!("Books parse failed: {}", e),
                status: None,
            })?;

        let mut map = HashMap::with_capacity(books.len());
        for book in books {
            let token_id = book
                .asset_id
                .clone()
                .or_else(|| book.token_id.clone())
                .unwrap_or_default();
            if token_id.is_empty() {
                continue;
            }
            map.insert(token_id, book);
        }

        Ok(map)
    }
}

/// Calculate VWAP from asks for buying
fn calculate_vwap(asks: &[PriceLevel], amount: Decimal) -> (Decimal, Decimal) {
    let mut total_cost = Decimal::ZERO;
    let mut filled = Decimal::ZERO;

    for (price, size) in asks {
        let fill = (*size).min(amount - filled);
        total_cost += price * fill;
        filled += fill;

        if filled >= amount {
            break;
        }
    }

    if filled == Decimal::ZERO {
        return (Decimal::ONE, Decimal::ZERO);
    }

    (total_cost / filled, filled)
}

/// Calculate VWAP from bids for selling
fn calculate_vwap_from_bids(bids: &[PriceLevel], amount: Decimal) -> (Decimal, Decimal) {
    let mut total_revenue = Decimal::ZERO;
    let mut filled = Decimal::ZERO;

    for (price, size) in bids {
        let fill = (*size).min(amount - filled);
        total_revenue += price * fill;
        filled += fill;

        if filled >= amount {
            break;
        }
    }

    if filled == Decimal::ZERO {
        return (Decimal::ZERO, Decimal::ZERO);
    }

    (total_revenue / filled, filled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_calculate_vwap() {
        let asks = vec![
            (dec!(0.45), dec!(50.0)),
            (dec!(0.46), dec!(50.0)),
            (dec!(0.47), dec!(100.0)),
        ];

        // Fill 50 at 0.45
        let (vwap, filled) = calculate_vwap(&asks, dec!(50.0));
        assert_eq!(filled, dec!(50.0));
        assert_eq!(vwap, dec!(0.45));

        // Fill 100: 50@0.45 + 50@0.46 = 45.5 / 100 = 0.455
        let (vwap, filled) = calculate_vwap(&asks, dec!(100.0));
        assert_eq!(filled, dec!(100.0));
        assert_eq!(vwap, dec!(0.455));
    }

    #[test]
    fn test_precheck_result() {
        let result = PreCheckResult::valid(dec!(1.50), dec!(1.25), 5.0);
        assert!(result.valid);
        assert_eq!(result.profit_delta, dec!(0.25));

        let result = PreCheckResult::invalid("Too old");
        assert!(!result.valid);
    }
}
