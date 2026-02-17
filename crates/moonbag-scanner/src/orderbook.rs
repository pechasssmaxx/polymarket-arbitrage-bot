//! Orderbook state management with DashMap
//!
//! Provides concurrent, lock-free access to orderbook state across multiple
//! WebSocket message handlers and analyzer workers.

use dashmap::mapref::one::Ref;
use dashmap::DashMap;
use moonbag_core::models::{MarketState, PriceLevel};
use rust_decimal::Decimal;
use std::sync::Arc;
use tracing::{debug, trace};

/// Token type (YES or NO)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenType {
    Yes,
    No,
}

/// Orderbook side
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BookSide {
    Bid,
    Ask,
}

/// Mapping from token_id to market info
#[derive(Debug, Clone)]
pub struct TokenMapping {
    pub market_id: String,
    pub outcome_key: String, // yes_token_id (unique outcome identifier)
    pub token_type: TokenType,
}

/// Concurrent orderbook manager using DashMap
///
/// Provides O(1) lookups for:
/// - token_id -> market mapping
/// - market_id -> MarketState
pub struct OrderbookManager {
    /// Market states keyed by neg_risk_market_id
    markets: DashMap<String, MarketState>,

    /// Token ID to market mapping
    token_map: DashMap<String, TokenMapping>,

    /// Resolved markets
    resolved: DashMap<String, ()>,

    /// Statistics
    pub book_updates: std::sync::atomic::AtomicU64,
}

impl OrderbookManager {
    /// Create a new orderbook manager
    pub fn new() -> Self {
        Self {
            markets: DashMap::new(),
            token_map: DashMap::new(),
            resolved: DashMap::new(),
            book_updates: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Create a new orderbook manager wrapped in Arc for sharing
    pub fn new_shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Register a market and its outcomes
    pub fn register_market(&self, market: MarketState) {
        let market_id = market.neg_risk_market_id.clone();

        // Clear any resolved flag (market re-activated or re-added)
        self.resolved.remove(&market_id);

        // Register token mappings for each outcome
        for outcome in market.outcomes.values() {
            // YES token mapping
            self.token_map.insert(
                outcome.yes_token_id.clone(),
                TokenMapping {
                    market_id: market_id.clone(),
                    outcome_key: outcome.yes_token_id.clone(),
                    token_type: TokenType::Yes,
                },
            );

            // NO token mapping
            self.token_map.insert(
                outcome.no_token_id.clone(),
                TokenMapping {
                    market_id: market_id.clone(),
                    outcome_key: outcome.yes_token_id.clone(),
                    token_type: TokenType::No,
                },
            );
        }

        // Store market
        self.markets.insert(market_id, market);
    }

    /// Unregister a market and all its token mappings
    pub fn unregister_market(&self, market_id: &str) -> bool {
        self.resolved.remove(market_id);
        let Some((_, market)) = self.markets.remove(market_id) else {
            return false;
        };
        for outcome in market.outcomes.values() {
            self.token_map.remove(&outcome.yes_token_id);
            self.token_map.remove(&outcome.no_token_id);
        }
        debug!(
            market_id = market_id,
            tokens_removed = market.outcomes.len() * 2,
            "Unregistered market"
        );
        true
    }

    /// Mark a market as resolved
    pub fn mark_resolved(&self, market_id: &str) {
        if self.markets.contains_key(market_id) {
            self.resolved.insert(market_id.to_string(), ());
        }
    }

    /// Check if a market is resolved
    pub fn is_resolved(&self, market_id: &str) -> bool {
        self.resolved.contains_key(market_id)
    }
    /// Get a market by ID (borrowed via DashMap read guard - no clone!)
    pub fn get_market(&self, market_id: &str) -> Option<Ref<'_, String, MarketState>> {
        self.markets.get(market_id)
    }

    /// Get a market by ID (owned snapshot - expensive, use sparingly)
    pub fn get_market_clone(&self, market_id: &str) -> Option<MarketState> {
        self.markets.get(market_id).map(|m| m.clone())
    }

    /// Get market ID for a token
    pub fn get_market_id(&self, token_id: &str) -> Option<String> {
        self.token_map.get(token_id).map(|m| m.market_id.clone())
    }

    /// Get all market IDs
    pub fn market_ids(&self) -> Vec<String> {
        self.markets.iter().map(|r| r.key().clone()).collect()
    }

    /// Get only active (non-resolved) market IDs
    pub fn active_market_ids(&self) -> Vec<String> {
        self.markets
            .iter()
            .filter(|r| !self.resolved.contains_key(r.key()))
            .map(|r| r.key().clone())
            .collect()
    }
    /// Get all token IDs (for WebSocket subscription)
    pub fn all_token_ids(&self) -> Vec<String> {
        self.token_map.iter().map(|r| r.key().clone()).collect()
    }

    /// Get token IDs only for active (non-resolved) markets
    pub fn active_token_ids(&self) -> Vec<String> {
        self.token_map
            .iter()
            .filter(|r| !self.resolved.contains_key(&r.market_id))
            .map(|r| r.key().clone())
            .collect()
    }
    /// Get only NO token IDs (for bootstrap - sufficient for USDC-only edges)
    pub fn no_token_ids(&self) -> Vec<String> {
        self.token_map
            .iter()
            .filter(|r| r.token_type == TokenType::No)
            .map(|r| r.key().clone())
            .collect()
    }

    /// Number of markets
    pub fn market_count(&self) -> usize {
        self.markets.len()
    }

    /// Number of tokens
    pub fn token_count(&self) -> usize {
        self.token_map.len()
    }

    /// Number of resolved markets
    pub fn resolved_count(&self) -> usize {
        self.resolved.len()
    }
    /// Update orderbook for a token with full depth
    ///
    /// This is called when we receive a full book snapshot from WS or REST.
    pub fn update_orderbook(&self, token_id: &str, bids: Vec<PriceLevel>, asks: Vec<PriceLevel>) {
        let Some(mapping) = self.token_map.get(token_id) else {
            trace!(token_id, "Unknown token");
            return;
        };

        let Some(mut market) = self.markets.get_mut(&mapping.market_id) else {
            return;
        };

        let Some(outcome) = market.outcomes.get_mut(&mapping.outcome_key) else {
            return;
        };

        // Sort bids descending, asks ascending
        let mut sorted_bids = bids;
        let mut sorted_asks = asks;
        sorted_bids.sort_by(|a, b| b.0.cmp(&a.0));
        sorted_asks.sort_by(|a, b| a.0.cmp(&b.0));

        match mapping.token_type {
            TokenType::Yes => {
                outcome.yes_bids = sorted_bids;
                outcome.yes_asks = sorted_asks;
            }
            TokenType::No => {
                let best_ask = sorted_asks.first().map(|(p, _)| *p);
                outcome.no_bids = sorted_bids;
                outcome.no_asks = sorted_asks;
                // Keep the cached market price in sync with the current best ask.
                // `effective_no_price()` prefers this value, so stale data can cause repeated false positives.
                outcome.no_market_price = best_ask;
            }
        }

        outcome.last_update = chrono::Utc::now();
        market.last_update = chrono::Utc::now();

        self.book_updates
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Apply a single price level delta update.
    ///
    /// This is used for WS `price_change` events which carry `{price, size, side}` deltas.
    ///
    /// - `BookSide::Bid`: update bids (sorted DESC by price)
    /// - `BookSide::Ask`: update asks (sorted ASC by price)
    pub fn update_price_level(
        &self,
        token_id: &str,
        side: BookSide,
        price: Decimal,
        size: Decimal,
    ) {
        #[inline]
        fn apply_delta(levels: &mut Vec<PriceLevel>, descending: bool, price: Decimal, size: Decimal) {
            // Keep only a bounded amount of depth (hot path safety).
            const MAX_LEVELS: usize = 256;

            let idx = if descending {
                levels.binary_search_by(|(p, _)| p.cmp(&price).reverse())
            } else {
                levels.binary_search_by(|(p, _)| p.cmp(&price))
            };

            match idx {
                Ok(i) => {
                    if size <= Decimal::ZERO {
                        levels.remove(i);
                    } else {
                        levels[i].1 = size;
                    }
                }
                Err(i) => {
                    if size <= Decimal::ZERO {
                        return;
                    }
                    levels.insert(i, (price, size));
                }
            }

            if levels.len() > MAX_LEVELS {
                levels.truncate(MAX_LEVELS);
            }
        }

        let Some(mapping) = self.token_map.get(token_id) else {
            return;
        };

        let Some(mut market) = self.markets.get_mut(&mapping.market_id) else {
            return;
        };

        let Some(outcome) = market.outcomes.get_mut(&mapping.outcome_key) else {
            return;
        };

        match (mapping.token_type, side) {
            (TokenType::Yes, BookSide::Bid) => {
                apply_delta(&mut outcome.yes_bids, true, price, size);
            }
            (TokenType::Yes, BookSide::Ask) => {
                apply_delta(&mut outcome.yes_asks, false, price, size);
            }
            (TokenType::No, BookSide::Bid) => {
                apply_delta(&mut outcome.no_bids, true, price, size);
            }
            (TokenType::No, BookSide::Ask) => {
                apply_delta(&mut outcome.no_asks, false, price, size);
                // Keep cached best ask in sync for NO.
                outcome.no_market_price = outcome.no_asks.first().map(|(p, _)| *p);
            }
        }

        outcome.last_update = chrono::Utc::now();
        market.last_update = chrono::Utc::now();

        self.book_updates
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Update best prices from WS price_changes (no full depth)
    ///
    /// NOTE: Some WS messages are quote-only (`best_bid`/`best_ask` without `{price, size, side}`).
    /// Those MUST NOT mutate depth (no_asks/no_bids/yes_asks/yes_bids). We store them separately.
    pub fn update_best_prices(
        &self,
        token_id: &str,
        best_bid: Option<Decimal>,
        best_ask: Option<Decimal>,
    ) {
        let Some(mapping) = self.token_map.get(token_id) else {
            return;
        };

        let Some(mut market) = self.markets.get_mut(&mapping.market_id) else {
            return;
        };

        let Some(outcome) = market.outcomes.get_mut(&mapping.outcome_key) else {
            return;
        };

        match mapping.token_type {
            TokenType::Yes => {
                outcome.ws_best_yes_bid = best_bid;
                outcome.ws_best_yes_ask = best_ask;
            }
            TokenType::No => {
                outcome.ws_best_no_bid = best_bid;
                outcome.ws_best_no_ask = best_ask;
            }
        }

        outcome.last_update = chrono::Utc::now();
        market.last_update = chrono::Utc::now();

        self.book_updates
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Get book update count
    pub fn get_book_updates(&self) -> u64 {
        self.book_updates.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Alias for update_orderbook (used by WS client)
    pub fn update_book(&self, token_id: &str, bids: Vec<PriceLevel>, asks: Vec<PriceLevel>) {
        self.update_orderbook(token_id, bids, asks);
    }

    /// Update market prices from /price endpoint bootstrap
    ///
    /// This is CRITICAL for accurate analysis! The orderbook has wide spreads
    /// (bids at $0.01, asks at $0.99) but /price returns actual market prices.
    pub fn update_market_prices(&self, prices: &std::collections::HashMap<String, Decimal>) {
        for (token_id, price) in prices {
            let Some(mapping) = self.token_map.get(token_id) else {
                continue;
            };

            let Some(mut market) = self.markets.get_mut(&mapping.market_id) else {
                continue;
            };

            let Some(outcome) = market.outcomes.get_mut(&mapping.outcome_key) else {
                continue;
            };

            match mapping.token_type {
                TokenType::Yes => {
                    outcome.yes_market_price = Some(*price);
                }
                TokenType::No => {
                    outcome.no_market_price = Some(*price);
                }
            }

            outcome.last_update = chrono::Utc::now();
        }

        debug!("Updated market prices for {} tokens", prices.len());
    }
}

impl Default for OrderbookManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moonbag_core::models::OutcomeState;
    use rust_decimal_macros::dec;

    #[test]
    fn test_register_and_lookup() {
        let manager = OrderbookManager::new();

        let mut market = MarketState::new(
            "Test Market".to_string(),
            "test-market".to_string(),
            "0x123".to_string(),
        );

        let outcome = OutcomeState::new(
            "Test Outcome".to_string(),
            "cond_1".to_string(),
            "yes_1".to_string(),
            "no_1".to_string(),
            0,
        );
        market.upsert_outcome(outcome);

        manager.register_market(market);

        assert_eq!(manager.market_count(), 1);
        assert_eq!(manager.token_count(), 2); // YES + NO

        assert_eq!(manager.get_market_id("yes_1"), Some("0x123".to_string()));
        assert_eq!(manager.get_market_id("no_1"), Some("0x123".to_string()));
    }

    #[test]
    fn test_update_orderbook() {
        let manager = OrderbookManager::new();

        let mut market =
            MarketState::new("Test".to_string(), "test".to_string(), "0x123".to_string());
        let outcome = OutcomeState::new(
            "Test".to_string(),
            "cond".to_string(),
            "yes".to_string(),
            "no".to_string(),
            0,
        );
        market.upsert_outcome(outcome);
        manager.register_market(market);

        // Update NO book
        manager.update_orderbook(
            "no",
            vec![(dec!(0.40), dec!(100.0))],
            vec![(dec!(0.45), dec!(50.0))],
        );

        let market = manager.get_market("0x123").unwrap();
        let outcome = market.outcomes.get("yes").unwrap();
        assert_eq!(outcome.no_bids.len(), 1);
        assert_eq!(outcome.no_asks.len(), 1);
        assert_eq!(outcome.no_asks[0].0, dec!(0.45));
    }
}
