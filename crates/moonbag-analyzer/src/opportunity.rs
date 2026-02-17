//! Opportunity detection and K-selection algorithm
//!
//! Ported from Python: `moonbag/analyzer.py`
//!
//! IMPORTANT: Uses `effective_no_price()` which prefers /price endpoint market prices
//! over raw orderbook asks. The orderbook has wide spreads (asks at $0.99) but
//! actual market prices can be much lower ($0.24 for favorites).

use crate::vwap::{calculate_vwap_and_impact, total_liquidity};
use moonbag_core::models::{
    calculate_min_order_size, ExecutionMode, MarketState, MoonbagOpportunity, OpportunityStatus,
    OutcomeState, RemainingOutcome, SelectedOutcome, MIN_SHARES,
};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use tracing::{debug, trace};

/// Binary search to find optimal size for K outcomes
/// Returns (optimal_size, profit) where profit >= min_profit, or None if no profitable size
fn find_optimal_size_for_k(
    outcomes: &[&[(Decimal, Decimal)]],  // NO asks for each of K outcomes
    k: usize,
    min_size: Decimal,
    max_size: Decimal,
    min_profit: Decimal,
) -> Option<(Decimal, Decimal)> {
    debug!(
        k = k,
        min_size = %min_size,
        max_size = %max_size,
        min_profit = %min_profit,
        outcome_count = outcomes.len(),
        "Binary search starting"
    );

    if min_size > max_size || max_size <= Decimal::ZERO {
        debug!(
            k = k,
            min_size = %min_size,
            max_size = %max_size,
            "Binary search: invalid range"
        );
        return None;
    }

    // Step size for binary search (0.1 shares precision)
    let precision = Decimal::new(1, 1);

    // Helper: calculate profit for a given size
    let calc_profit = |size: Decimal| -> Option<Decimal> {
        let mut total_vwap_sum = Decimal::ZERO;
        for asks in outcomes {
            let (vwap, _) = calculate_vwap_and_impact(asks, size)?;
            total_vwap_sum += vwap;
        }
        let cost = total_vwap_sum * size;
        let usdc_return = Decimal::from(k - 1) * size;
        Some(usdc_return - cost)
    };

    // First check if max_size is profitable
    if let Some(profit_at_max) = calc_profit(max_size) {
        debug!(
            k = k,
            size = %max_size,
            profit = %profit_at_max,
            min_profit = %min_profit,
            profitable = profit_at_max >= min_profit,
            "Binary search: checking max_size"
        );
        if profit_at_max >= min_profit {
            return Some((max_size, profit_at_max));
        }
    } else {
        debug!(
            k = k,
            size = %max_size,
            "Binary search: no liquidity at max_size"
        );
    }

    // Check if min_size is profitable
    let profit_at_min = match calc_profit(min_size) {
        Some(p) => p,
        None => {
            debug!(
                k = k,
                min_size = %min_size,
                "Binary search: no liquidity at min_size"
            );
            return None;
        }
    };

    debug!(
        k = k,
        min_size = %min_size,
        profit_at_min = %profit_at_min,
        min_profit = %min_profit,
        profitable = profit_at_min >= min_profit,
        "Binary search: checking min_size"
    );

    if profit_at_min < min_profit {
        return None;  // Even minimum size is unprofitable
    }

    // Binary search for largest profitable size
    let mut low = min_size;
    let mut high = max_size;
    let mut best_size = min_size;
    let mut best_profit = profit_at_min;
    let mut iterations = 0;

    // Limit iterations (log2(10000) ≈ 14)
    for _ in 0..20 {
        if high - low < precision {
            break;
        }
        iterations += 1;

        let mid = (low + high) / Decimal::TWO;

        if let Some(profit) = calc_profit(mid) {
            if profit >= min_profit {
                // Mid is profitable, try larger
                best_size = mid;
                best_profit = profit;
                low = mid + precision;
            } else {
                // Mid is unprofitable, try smaller
                high = mid - precision;
            }
        } else {
            // No liquidity at mid, try smaller
            high = mid - precision;
        }
    }

    debug!(
        k = k,
        optimal_size = %best_size.round_dp(2),
        profit = %best_profit.round_dp(4),
        iterations = iterations,
        search_range_min = %min_size.round_dp(2),
        search_range_max = %max_size.round_dp(2),
        "Binary search RESULT"
    );

    Some((best_size, best_profit))
}

/// Moonbag opportunity analyzer
///
/// Uses market prices from /price endpoint (preferred) or VWAP from orderbook.
pub struct MoonbagAnalyzer {
    /// Minimum REAL profit (default $0.01)
    pub min_profit: Decimal,
    /// Minimum moonbags (0 = profit alone is ok)
    pub min_moonbags: u8,
    /// Minimum liquidity per outcome
    pub min_liquidity: Decimal,
    /// YES price below this = moonbag (0.5 cent)
    pub moonbag_threshold: Decimal,
    /// Require moonbags to have YES bid >= this (0 disables)
    pub min_moonbag_bid: Decimal,
    /// Max moonbags allowed below min_moonbag_bid
    pub max_dead_moonbags: u8,
    /// Limit outcomes that receive YES (0 = USDC-only)
    pub max_remaining_outcomes: Option<u8>,
    /// Minimum selected NO outcomes (K)
    pub min_k: u8,
    /// Cap number of selected NO outcomes (K)
    pub max_k: Option<u8>,
    /// Gas price in gwei
    pub gas_price_gwei: Decimal,
    /// POL/MATIC price in USD
    pub pol_usd: Decimal,
    /// Gas multiplier
    pub gas_multiplier: Decimal,
    /// Slippage tolerance for FOK check
    pub slippage_tolerance: Decimal,
}

impl Default for MoonbagAnalyzer {
    fn default() -> Self {
        Self {
            min_profit: Decimal::new(1, 2), // $0.01
            min_moonbags: 0,
            min_liquidity: Decimal::new(5, 0),     // $5
            moonbag_threshold: Decimal::new(5, 3), // 0.005
            min_moonbag_bid: Decimal::ZERO,
            max_dead_moonbags: 0,
            max_remaining_outcomes: None,
            min_k: 2,
            max_k: None,
            gas_price_gwei: Decimal::new(35, 0),
            pol_usd: Decimal::new(10, 2), // $0.10 (matches Python)
            gas_multiplier: Decimal::ONE,
            slippage_tolerance: Decimal::new(2, 2), // 0.02
        }
    }
}

impl MoonbagAnalyzer {
    /// Create a new analyzer with custom settings
    pub fn new() -> Self {
        Self::default()
    }

    /// Find the best opportunity in a market
    pub fn find_opportunity(
        &self,
        market: &MarketState,
        target_size: Decimal,
    ) -> Option<MoonbagOpportunity> {
        self.find_opportunities(market, target_size, 1)
            .into_iter()
            .next()
    }

    /// Find profitable moonbag opportunities in a market (sorted by profit desc)
    ///
    /// OPTIMIZED: O(M log M + K) instead of O(M² * K)
    /// - Per-outcome metrics computed once
    /// - Prefix sums for K iteration
    /// - MoonbagOpportunity built only for top candidates
    pub fn find_opportunities(
        &self,
        market: &MarketState,
        target_size: Decimal,
        max_results: usize,
    ) -> Vec<MoonbagOpportunity> {
        let m = market.outcome_count();
        if m < 2 || max_results == 0 {
            return vec![];
        }

        let outcomes: Vec<&OutcomeState> = market.outcomes.values().collect();

        let all_convertible = market.convertible_indices.is_empty();
        let mut convertible_lookup = [false; 256];
        if !all_convertible {
            for &idx in &market.convertible_indices {
                convertible_lookup[idx as usize] = true;
            }
        }

        #[inline]
        fn is_convertible(all: bool, lookup: &[bool; 256], idx: u8) -> bool {
            all || lookup[idx as usize]
        }

        #[inline]
        fn has_liquidity(levels: &[(Decimal, Decimal)], target_size: Decimal) -> bool {
            if levels.is_empty() {
                return false;
            }
            let mut available = Decimal::ZERO;
            for &(_, size) in levels {
                available += size;
                if available >= target_size {
                    return true;
                }
            }
            false
        }

        // Precompute total remaining-side values for fast subtraction in the K loop.
        let sell_haircut = Decimal::new(98, 2); // 0.98
        let min_moonbag_bid_enabled = self.min_moonbag_bid > Decimal::ZERO;
        let mut total_yes_revenue = Decimal::ZERO;
        let mut total_moonbags: usize = 0;
        let mut total_dead_moonbags: usize = 0;

        for &o in &outcomes {
            let convertible = is_convertible(all_convertible, &convertible_lookup, o.convert_index);
            // Use YES VWAP instead of best bid (matches Python)
            let yes_vwap = calculate_vwap_and_impact(&o.yes_bids, target_size).map(|(v, _)| v);

            let sells_yes =
                convertible && yes_vwap.filter(|&p| p >= self.moonbag_threshold).is_some();

            if sells_yes {
                let vwap = yes_vwap.unwrap();
                total_yes_revenue += vwap * target_size * sell_haircut;
            } else {
                total_moonbags += 1;
                if min_moonbag_bid_enabled && o.best_yes_bid() < self.min_moonbag_bid {
                    total_dead_moonbags += 1;
                }
            }
        }

        struct ScoredOutcome<'a> {
            outcome: &'a OutcomeState,
            score: Decimal,
            no_price: Decimal,
            /// VWAP and impact for target_size (may need recalc for effective_amount)
            vwap_and_impact: Option<(Decimal, Decimal)>,
            /// Minimum order size based on no_price
            min_order_size: Decimal,
            /// Total available liquidity from NO asks orderbook
            available_liquidity: Decimal,
            yes_revenue_if_remaining: Decimal,
            moonbag_if_remaining: bool,
            dead_moonbag_if_remaining: bool,
        }

        let mut scored: Vec<ScoredOutcome<'_>> = outcomes
            .iter()
            .filter(|o| is_convertible(all_convertible, &convertible_lookup, o.convert_index))
            .filter_map(|&o| {
                let no_price = o.effective_no_price()?;
                if no_price > Decimal::ONE {
                    return None;
                }

                // Calculate minimum order size for this outcome
                let min_order_size = calculate_min_order_size(no_price);

                // Use YES VWAP for both scoring and revenue (matches Python)
                let yes_vwap = calculate_vwap_and_impact(&o.yes_bids, target_size).map(|(v, _)| v);

                // Heuristic YES value (per-share) for selection scoring
                let yes_value = yes_vwap
                    .filter(|&v| v >= self.moonbag_threshold)
                    .map(|v| v * sell_haircut)
                    .unwrap_or(Decimal::ZERO);

                // Remaining-side per-outcome values (for fast totals)
                let sells_yes = yes_vwap.filter(|&p| p >= self.moonbag_threshold).is_some();

                let yes_revenue_if_remaining = if sells_yes {
                    yes_vwap.unwrap() * target_size * sell_haircut
                } else {
                    Decimal::ZERO
                };

                let moonbag_if_remaining = !sells_yes;
                let dead_moonbag_if_remaining = min_moonbag_bid_enabled
                    && moonbag_if_remaining
                    && o.best_yes_bid() < self.min_moonbag_bid;

                // Calculate available liquidity from NO asks orderbook
                let available_liquidity = total_liquidity(&o.no_asks);

                Some(ScoredOutcome {
                    outcome: o,
                    score: no_price + yes_value,
                    no_price,
                    vwap_and_impact: calculate_vwap_and_impact(&o.no_asks, target_size),
                    min_order_size,
                    available_liquidity,
                    yes_revenue_if_remaining,
                    moonbag_if_remaining,
                    dead_moonbag_if_remaining,
                })
            })
            .collect();

        if scored.len() < self.min_k as usize {
            return vec![];
        }

        scored.sort_by(|a, b| a.score.cmp(&b.score));

        // Gas cost removed - negligible on Polygon (<1 cent)

        let max_k = self
            .max_k
            .map(|k| k as usize)
            .unwrap_or(scored.len())
            .min(scored.len());

        #[derive(Debug, Clone)]
        struct Candidate {
            k: usize,
            profit: Decimal,
            no_cost: Decimal,
            usdc_return: Decimal,
            yes_revenue: Decimal,
            /// Effective amount after min order size adjustment
            effective_amount: Decimal,
        }

        let mut candidates = Vec::new();
        let mut selected_no_vwap_sum = Decimal::ZERO; // Use VWAP, not top-ask!
        let mut selected_yes_revenue_if_remaining_sum = Decimal::ZERO;
        let mut selected_moonbags = 0usize;
        let mut selected_dead_moonbags = 0usize;
        let mut all_selected_fillable = true;
        let mut index_set_supported = true;
        let mut max_min_order_size = MIN_SHARES; // Track max min order size across K
        let mut min_liquidity_across_k = Decimal::MAX; // Track min available liquidity across K

        for (i, s) in scored.iter().enumerate().take(max_k) {
            let k = i + 1;

            // Track max min order size across all selected outcomes
            if s.min_order_size > max_min_order_size {
                max_min_order_size = s.min_order_size;
            }

            // Track min available liquidity across all selected outcomes
            // CONVERT requires equal amounts, so max fillable = min(liquidity across K)
            if s.available_liquidity < min_liquidity_across_k {
                min_liquidity_across_k = s.available_liquidity;
            }

            trace!(
                k = k,
                outcome = %s.outcome.question,
                no_price = %s.no_price,
                outcome_liquidity = %s.available_liquidity,
                outcome_min_order = %s.min_order_size,
                min_liq_across_k = %min_liquidity_across_k,
                max_min_order_k = %max_min_order_size,
                "Tracking liquidity per outcome"
            );

            // Skip if no liquidity available
            if min_liquidity_across_k <= Decimal::ZERO {
                continue;
            }

            // Use VWAP for cost calculation (matches Python), fallback to top-ask
            let no_vwap = s.vwap_and_impact.map(|(v, _)| v).unwrap_or(s.no_price);
            selected_no_vwap_sum += no_vwap;
            selected_yes_revenue_if_remaining_sum += s.yes_revenue_if_remaining;

            if s.moonbag_if_remaining {
                selected_moonbags += 1;
                if s.dead_moonbag_if_remaining {
                    selected_dead_moonbags += 1;
                }
            }

            all_selected_fillable &= s.vwap_and_impact.is_some();
            index_set_supported &= s.outcome.convert_index < 128;

            if k < self.min_k as usize {
                continue;
            }

            if !all_selected_fillable || !index_set_supported {
                break;
            }

            if let Some(max_rem) = self.max_remaining_outcomes {
                if m - k > max_rem as usize {
                    continue;
                }
            }

            let remaining_moonbags = total_moonbags.saturating_sub(selected_moonbags);
            if remaining_moonbags < self.min_moonbags as usize {
                continue;
            }

            let remaining_dead_moonbags =
                total_dead_moonbags.saturating_sub(selected_dead_moonbags);
            if min_moonbag_bid_enabled && remaining_dead_moonbags > self.max_dead_moonbags as usize
            {
                continue;
            }

            // Collect NO asks for binary search
            let no_asks_for_k: Vec<&[(Decimal, Decimal)]> = scored[..k]
                .iter()
                .map(|s| s.outcome.no_asks.as_slice())
                .collect();

            // Binary search for optimal size
            let search_result = find_optimal_size_for_k(
                &no_asks_for_k,
                k,
                max_min_order_size,           // min_size
                min_liquidity_across_k,       // max_size (capped by available liquidity)
                self.min_profit,
            );

            let (optimal_size, profit) = match search_result {
                Some((size, profit)) => (size, profit),
                None => {
                    trace!(
                        k = k,
                        min_size = %max_min_order_size,
                        max_size = %min_liquidity_across_k,
                        min_profit = %self.min_profit,
                        "No profitable size found via binary search"
                    );
                    continue;
                }
            };

            // Calculate final costs with optimal size
            let mut vwap_sum = Decimal::ZERO;
            for j in 0..k {
                let (vwap, _) = calculate_vwap_and_impact(&scored[j].outcome.no_asks, optimal_size)
                    .unwrap_or((scored[j].no_price, scored[j].no_price));
                vwap_sum += vwap;
            }
            let final_no_cost = vwap_sum * optimal_size;
            let usdc_return = Decimal::from(k - 1) * optimal_size;

            debug!(
                k = k,
                m = m,
                optimal_size = %optimal_size.round_dp(2),
                no_cost = %final_no_cost.round_dp(2),
                usdc_return = %usdc_return.round_dp(2),
                profit = %profit.round_dp(4),
                min_liquidity = %min_liquidity_across_k.round_dp(2),
                max_min_order = %max_min_order_size.round_dp(2),
                moonbags = remaining_moonbags,
                "CANDIDATE: binary search optimal size"
            );

            candidates.push(Candidate {
                k,
                profit,
                no_cost: final_no_cost,
                usdc_return,
                yes_revenue: Decimal::ZERO,  // Not selling moonbags
                effective_amount: optimal_size,
            });
        }

        candidates.sort_by(|a, b| b.profit.cmp(&a.profit));
        if candidates.len() > max_results {
            candidates.truncate(max_results);
        }

        let mut opportunities = Vec::with_capacity(candidates.len());

        for cand in candidates {
            let k = cand.k;
            let selected = &scored[..k];

            let mut index_set: u128 = 0;
            for s in selected {
                let shift = s.outcome.convert_index;
                if shift >= 128 {
                    index_set = 0;
                    break;
                }
                index_set |= 1u128 << shift;
            }

            let mut selected_by_index = [false; 256];
            for s in selected {
                selected_by_index[s.outcome.convert_index as usize] = true;
            }

            let mut remaining_outcomes = Vec::with_capacity(m.saturating_sub(k));
            let mut moonbag_names = Vec::new();

            for &o in &outcomes {
                if selected_by_index[o.convert_index as usize] {
                    continue;
                }

                let convertible =
                    is_convertible(all_convertible, &convertible_lookup, o.convert_index);
                let yes_price = o.effective_yes_price().unwrap_or_else(|| o.best_yes_bid());

                remaining_outcomes.push(RemainingOutcome {
                    condition_id: o.condition_id.clone(),
                    yes_token_id: o.yes_token_id.clone(),
                    no_token_id: o.no_token_id.clone(),
                    question: o.question.clone(),
                    yes_bid: yes_price,
                    yes_vwap: Some(yes_price),
                    alphabetical_index: o.convert_index,
                    convertible,
                });

                let yes_price_opt = o.effective_yes_price();
                let sells_yes = convertible
                    && yes_price_opt
                        .filter(|&p| p >= self.moonbag_threshold)
                        .is_some()
                    && has_liquidity(&o.yes_bids, cand.effective_amount);

                if !sells_yes {
                    moonbag_names.push(o.question.chars().take(30).collect());
                }
            }

            // Use effective_amount for VWAP/impact if it differs from target_size
            let selected_outcomes: Vec<SelectedOutcome> = selected
                .iter()
                .map(|s| {
                    // Recalculate vwap/impact with effective_amount if needed
                    let (vwap, impact) = if cand.effective_amount > target_size {
                        calculate_vwap_and_impact(&s.outcome.no_asks, cand.effective_amount)
                            .unwrap_or((s.no_price, s.no_price))
                    } else {
                        s.vwap_and_impact.unwrap_or((s.no_price, s.no_price))
                    };
                    SelectedOutcome {
                        condition_id: s.outcome.condition_id.clone(),
                        no_token_id: s.outcome.no_token_id.clone(),
                        question: s.outcome.question.clone(),
                        no_price: s.no_price,
                        no_vwap: vwap,
                        no_impact_price: impact,
                        alphabetical_index: s.outcome.convert_index,
                    }
                })
                .collect();

            let opp = MoonbagOpportunity {
                neg_risk_market_id: market.neg_risk_market_id.clone(),
                market_title: market.title.clone(),
                market_slug: market.slug.clone(),
                k: k as u8,
                m: m as u8,
                index_set,
                amount: cand.effective_amount,
                amount_raw: (cand.effective_amount * Decimal::new(1_000_000, 0))
                    .to_u64()
                    .unwrap_or(0),
                selected_outcomes,
                remaining_outcomes,
                no_cost: cand.no_cost,
                usdc_return: cand.usdc_return,
                yes_revenue: Decimal::ZERO,  // Not selling moonbags
                estimated_gas: 0,  // Gas negligible on Polygon
                gas_cost: Decimal::ZERO,
                guaranteed_profit: cand.profit,
                moonbag_count: moonbag_names.len() as u8,
                moonbag_names,
                timestamp: chrono::Utc::now(),
                status: OpportunityStatus::Pending,
                mode: ExecutionMode::Profit,
                max_cycles: 1,
                max_total_loss: Decimal::ZERO,
                pre_check_min_profit: None,
                allow_partial_k: true,
                convert_cap: Decimal::ZERO,
                sell_yes: false,  // Keep moonbags, don't sell
                sell_yes_min_bid: Decimal::ONE,  // Never sell (bid would need to be $1)
                sell_yes_cap_to_converted: false,
            };

            debug!(
                market = %market.title,
                k = k,
                profit = %opp.guaranteed_profit,
                "Found opportunity"
            );

            opportunities.push(opp);
        }

        opportunities
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn make_market() -> MarketState {
        let mut market = MarketState::new(
            "Test Election".to_string(),
            "test-election".to_string(),
            "0x123abc".to_string(),
        );

        // Outcome A: cheap NO, no YES bids
        let mut a = OutcomeState::new(
            "Candidate A".to_string(),
            "cond_a".to_string(),
            "yes_a".to_string(),
            "no_a".to_string(),
            0,
        );
        a.no_asks = vec![(dec!(0.40), dec!(100.0)), (dec!(0.42), dec!(100.0))];
        a.yes_bids = vec![(dec!(0.001), dec!(50.0))];
        market.upsert_outcome(a);

        // Outcome B: medium NO, small YES bids
        let mut b = OutcomeState::new(
            "Candidate B".to_string(),
            "cond_b".to_string(),
            "yes_b".to_string(),
            "no_b".to_string(),
            1,
        );
        b.no_asks = vec![(dec!(0.45), dec!(100.0)), (dec!(0.47), dec!(100.0))];
        b.yes_bids = vec![(dec!(0.002), dec!(50.0))];
        market.upsert_outcome(b);

        // Outcome C: expensive NO, good YES bids (moonbag)
        let mut c = OutcomeState::new(
            "Candidate C".to_string(),
            "cond_c".to_string(),
            "yes_c".to_string(),
            "no_c".to_string(),
            2,
        );
        c.no_asks = vec![(dec!(0.85), dec!(100.0))];
        c.yes_bids = vec![(dec!(0.10), dec!(100.0))];
        market.upsert_outcome(c);

        market
    }

    #[test]
    fn test_find_opportunity() {
        let market = make_market();
        let analyzer = MoonbagAnalyzer {
            min_profit: dec!(-10.0), // Allow losses for testing
            ..Default::default()
        };

        let opp = analyzer.find_opportunity(&market, dec!(10.0));
        assert!(opp.is_some());

        let opp = opp.unwrap();
        // With the test prices:
        // K=2: cost=8.5, return=10+1=11, profit=2.5
        // K=3: cost=17, return=20, profit=3.0 (more profitable!)
        // So K=3 is selected
        assert_eq!(opp.k, 3);
        assert_eq!(opp.m, 3);
        // index_set = 2^0 + 2^1 + 2^2 = 7 (all outcomes selected)
        assert_eq!(opp.index_set, 7);
    }

    #[test]
    fn test_no_opportunity_when_expensive() {
        let mut market = make_market();

        // Make all NO very expensive
        for outcome in market.outcomes.values_mut() {
            outcome.no_asks = vec![(dec!(0.95), dec!(100.0))];
        }

        let analyzer = MoonbagAnalyzer::default();
        let opp = analyzer.find_opportunity(&market, dec!(10.0));
        assert!(opp.is_none());
    }
}
