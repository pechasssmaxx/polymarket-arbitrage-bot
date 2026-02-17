//! Quick check filter for fast opportunity pre-screening
//!
//! Uses market prices from /price endpoint (preferred) or orderbook for O(M) screening
//! before running full VWAP analysis.
//!
//! NOTE: This is a FAST filter - it does NOT verify orderbook liquidity depth.
//! The full analyzer (opportunity.rs) performs liquidity checks to ensure
//! we can actually fill the target size before reporting an opportunity.

use moonbag_core::models::MarketState;
use rust_decimal::Decimal;

/// Perform quick profitability check using market prices.
///
/// OPTIMIZED: O(M log M) - no HashSet, no per-K allocations, prefix-sum style.
///
/// # Returns
/// true if ANY K configuration could be profitable
pub fn quick_check(
    market: &MarketState,
    _target_size: Decimal,
    min_profit_per_share: Decimal,
    min_k: u8,
    max_k: Option<u8>,
    moonbag_threshold: Decimal,
) -> bool {
    let m = market.outcome_count();
    if m < 2 {
        return false;
    }

    // Convertible lookup (fast path, no HashSet alloc)
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

    let sell_haircut = Decimal::new(98, 2); // 0.98

    // Approx model:
    // profit_per_share(K) = (K-1) + total_yes_value - sum_k(no_price + yes_value)
    let mut total_yes_value = Decimal::ZERO;
    let mut scores: Vec<Decimal> = Vec::with_capacity(m);

    for o in market.outcomes.values() {
        if !is_convertible(all_convertible, &convertible_lookup, o.convert_index) {
            continue;
        }

        let yes_value = o
            .effective_yes_price()
            .filter(|&p| p >= moonbag_threshold)
            .map(|p| p * sell_haircut)
            .unwrap_or(Decimal::ZERO);

        total_yes_value += yes_value;

        // Skip outcomes with no real NO liquidity (no asks).
        // This avoids selecting outcomes that can't be bought immediately.
        if o.no_asks.is_empty() {
            continue;
        }

        let Some(no_price) = o.effective_no_price() else {
            continue;
        };
        if no_price > Decimal::ONE {
            continue;
        }

        scores.push(no_price + yes_value);
    }

    let min_k_val = min_k as usize;
    if scores.len() < min_k_val {
        return false;
    }

    scores.sort_unstable_by(|a, b| a.cmp(b));

    let max_k_val = max_k
        .map(|k| k as usize)
        .unwrap_or(scores.len())
        .min(scores.len());

    let mut selected_sum = Decimal::ZERO;
    for (i, score) in scores.iter().take(max_k_val).enumerate() {
        selected_sum += *score;

        let k = i + 1;
        if k < min_k_val {
            continue;
        }

        // (K-1) == i
        let approx_profit = Decimal::from(i as u64) + total_yes_value - selected_sum;
        if approx_profit >= min_profit_per_share {
            return true;
        }
    }

    false
}

/// Same as quick_check but returns the best approx_profit seen (for debugging)
pub fn quick_check_with_best_profit(
    market: &MarketState,
    _target_size: Decimal,
    min_k: u8,
    max_k: Option<u8>,
    moonbag_threshold: Decimal,
) -> Option<Decimal> {
    let m = market.outcome_count();
    if m < 2 {
        return None;
    }

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

    let sell_haircut = Decimal::new(98, 2);

    let mut total_yes_value = Decimal::ZERO;
    let mut scores: Vec<Decimal> = Vec::with_capacity(m);

    for o in market.outcomes.values() {
        if !is_convertible(all_convertible, &convertible_lookup, o.convert_index) {
            continue;
        }

        let yes_value = o
            .effective_yes_price()
            .filter(|&p| p >= moonbag_threshold)
            .map(|p| p * sell_haircut)
            .unwrap_or(Decimal::ZERO);

        total_yes_value += yes_value;

        // Skip outcomes with no real NO liquidity (no asks).
        if o.no_asks.is_empty() {
            continue;
        }

        let Some(no_price) = o.effective_no_price() else {
            continue;
        };
        if no_price > Decimal::ONE {
            continue;
        }

        scores.push(no_price + yes_value);
    }

    let min_k_val = min_k as usize;
    if scores.len() < min_k_val {
        return None;
    }

    scores.sort_unstable_by(|a, b| a.cmp(b));

    let max_k_val = max_k
        .map(|k| k as usize)
        .unwrap_or(scores.len())
        .min(scores.len());

    let mut best_profit: Option<Decimal> = None;
    let mut selected_sum = Decimal::ZERO;

    for (i, score) in scores.iter().take(max_k_val).enumerate() {
        selected_sum += *score;

        let k = i + 1;
        if k < min_k_val {
            continue;
        }

        let approx_profit = Decimal::from(i as u64) + total_yes_value - selected_sum;
        best_profit = Some(best_profit.map_or(approx_profit, |b| b.max(approx_profit)));
    }

    best_profit
}

#[cfg(test)]
mod tests {
    use super::*;
    use moonbag_core::models::OutcomeState;
    use rust_decimal_macros::dec;

    fn make_outcome(question: &str, no_ask: Decimal, yes_bid: Decimal) -> OutcomeState {
        let mut o = OutcomeState::new(
            question.to_string(),
            format!("cond_{question}"),
            format!("yes_{question}"),
            format!("no_{question}"),
            0,
        );
        o.no_asks = vec![(no_ask, dec!(100.0))];
        o.yes_bids = vec![(yes_bid, dec!(100.0))];
        o
    }

    #[test]
    fn test_quick_check_profitable() {
        let mut market = MarketState::new(
            "Test Market".to_string(),
            "test".to_string(),
            "0x123".to_string(),
        );

        // K=2 profitable: buy NO at 0.40 + 0.45 = 0.85, get (2-1) = 1.0 back
        // Profit per share = 1.0 - 0.85 = 0.15
        market.upsert_outcome(make_outcome("A", dec!(0.40), dec!(0.01)));
        market.upsert_outcome(make_outcome("B", dec!(0.45), dec!(0.01)));
        market.upsert_outcome(make_outcome("C", dec!(0.90), dec!(0.10)));

        assert!(quick_check(
            &market,
            dec!(10.0),
            dec!(0.05),
            2,
            None,
            dec!(0.005)
        ));
    }

    #[test]
    fn test_quick_check_not_profitable() {
        let mut market = MarketState::new(
            "Test Market".to_string(),
            "test".to_string(),
            "0x123".to_string(),
        );

        // K=2: buy at 0.55 + 0.60 = 1.15, get 1.0 back = -0.15 loss
        market.upsert_outcome(make_outcome("A", dec!(0.55), dec!(0.01)));
        market.upsert_outcome(make_outcome("B", dec!(0.60), dec!(0.01)));
        market.upsert_outcome(make_outcome("C", dec!(0.90), dec!(0.01)));

        assert!(!quick_check(
            &market,
            dec!(10.0),
            dec!(0.05),
            2,
            None,
            dec!(0.005)
        ));
    }
}
