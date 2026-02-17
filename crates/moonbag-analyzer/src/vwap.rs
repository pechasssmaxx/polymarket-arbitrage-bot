//! VWAP (Volume Weighted Average Price) calculation
//!
//! Optimized for speed - target <100μs per calculation

use moonbag_core::models::PriceLevel;
use rust_decimal::Decimal;

/// Calculate Volume Weighted Average Price for a target size.
///
/// # Arguments
/// * `levels` - Price levels sorted appropriately (asks ascending, bids descending)
/// * `target_size` - How many tokens we want to buy/sell
///
/// # Returns
/// VWAP or None if insufficient liquidity
///
/// # Example
/// ```
/// use moonbag_analyzer::vwap::calculate_vwap;
/// use rust_decimal_macros::dec;
///
/// let asks = vec![(dec!(0.45), dec!(50.0)), (dec!(0.50), dec!(50.0))];
/// let vwap = calculate_vwap(&asks, dec!(100.0));
/// assert_eq!(vwap, Some(dec!(0.475)));
/// ```
#[inline]
pub fn calculate_vwap(levels: &[PriceLevel], target_size: Decimal) -> Option<Decimal> {
    if levels.is_empty() || target_size <= Decimal::ZERO {
        return None;
    }

    let mut total_cost = Decimal::ZERO;
    let mut remaining = target_size;

    for &(price, size) in levels {
        if remaining <= Decimal::ZERO {
            break;
        }

        let fill_size = if size < remaining { size } else { remaining };
        total_cost += price * fill_size;
        remaining -= fill_size;
    }

    if remaining > Decimal::ZERO {
        // Insufficient liquidity
        return None;
    }

    Some(total_cost / target_size)
}

/// Calculate VWAP and impact (marginal) price for a target size.
///
/// # Arguments
/// * `levels` - Price levels sorted appropriately
/// * `target_size` - How many tokens we want to buy/sell
///
/// # Returns
/// (VWAP, impact_price) or None if insufficient liquidity
///
/// For asks (buys), impact price is the HIGHEST ask needed to fill.
/// For bids (sells), impact price is the LOWEST bid needed to fill.
#[inline]
pub fn calculate_vwap_and_impact(
    levels: &[PriceLevel],
    target_size: Decimal,
) -> Option<(Decimal, Decimal)> {
    if levels.is_empty() || target_size <= Decimal::ZERO {
        return None;
    }

    let mut total_cost = Decimal::ZERO;
    let mut remaining = target_size;
    let mut impact_price = Decimal::ZERO;

    for &(price, size) in levels {
        if remaining <= Decimal::ZERO {
            break;
        }

        let fill_size = if size < remaining { size } else { remaining };
        total_cost += price * fill_size;
        remaining -= fill_size;
        impact_price = price;
    }

    if remaining > Decimal::ZERO {
        return None;
    }

    Some((total_cost / target_size, impact_price))
}

/// Check if a FOK order can fill at acceptable price.
///
/// FOK (Fill-Or-Kill) requires the ENTIRE order to fill immediately.
/// We need enough SIZE at or below our max_price * (1 + slippage).
///
/// # Arguments
/// * `levels` - Ask levels sorted ascending by price
/// * `target_size` - How many tokens we need
/// * `max_price` - Maximum price we're willing to pay (before slippage)
/// * `slippage` - Allowed slippage (e.g., 0.02 for 2%)
///
/// # Returns
/// (can_fill, available_size)
#[inline]
pub fn check_fok_fillable(
    levels: &[PriceLevel],
    target_size: Decimal,
    max_price: Decimal,
    slippage: Decimal,
) -> (bool, Decimal) {
    if levels.is_empty() {
        return (false, Decimal::ZERO);
    }

    let limit_price = max_price * (Decimal::ONE + slippage);
    let mut available = Decimal::ZERO;

    for &(price, size) in levels {
        if price > limit_price {
            break;
        }
        available += size;
    }

    (available >= target_size, available)
}

/// Check total liquidity available in orderbook (ignoring prices).
///
/// This is useful when orderbook has wide spreads (asks at $0.99) but
/// we know the actual market price from /price endpoint. We want to verify
/// there's enough DEPTH even if the prices are not accurate.
///
/// # Arguments
/// * `levels` - Order book levels (bids or asks)
///
/// # Returns
/// Total size available across all levels
#[inline]
pub fn total_liquidity(levels: &[PriceLevel]) -> Decimal {
    levels.iter().map(|(_, size)| size).sum()
}

/// Check if orderbook has sufficient liquidity to fill target size.
///
/// Returns (has_liquidity, available_size, depth_levels)
#[inline]
pub fn check_liquidity(levels: &[PriceLevel], target_size: Decimal) -> (bool, Decimal, usize) {
    let available: Decimal = levels.iter().map(|(_, size)| size).sum();
    (available >= target_size, available, levels.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_vwap_single_level() {
        let levels = vec![(dec!(0.50), dec!(100.0))];
        assert_eq!(calculate_vwap(&levels, dec!(50.0)), Some(dec!(0.50)));
    }

    #[test]
    fn test_vwap_multiple_levels() {
        let levels = vec![
            (dec!(0.45), dec!(50.0)),
            (dec!(0.50), dec!(50.0)),
            (dec!(0.55), dec!(50.0)),
        ];
        // (0.45*50 + 0.50*50) / 100 = 0.475
        assert_eq!(calculate_vwap(&levels, dec!(100.0)), Some(dec!(0.475)));
    }

    #[test]
    fn test_vwap_insufficient_liquidity() {
        let levels = vec![(dec!(0.50), dec!(10.0))];
        assert_eq!(calculate_vwap(&levels, dec!(100.0)), None);
    }

    #[test]
    fn test_vwap_empty_levels() {
        let levels: Vec<PriceLevel> = vec![];
        assert_eq!(calculate_vwap(&levels, dec!(100.0)), None);
    }

    #[test]
    fn test_vwap_and_impact() {
        let levels = vec![
            (dec!(0.45), dec!(50.0)),
            (dec!(0.50), dec!(30.0)),
            (dec!(0.55), dec!(20.0)),
        ];
        // Fill 100: 50@0.45 + 30@0.50 + 20@0.55
        // Cost = 22.5 + 15 + 11 = 48.5
        // VWAP = 48.5 / 100 = 0.485
        // Impact = 0.55 (highest price needed)
        let result = calculate_vwap_and_impact(&levels, dec!(100.0));
        assert!(result.is_some());
        let (vwap, impact) = result.unwrap();
        assert_eq!(vwap, dec!(0.485));
        assert_eq!(impact, dec!(0.55));
    }

    #[test]
    fn test_fok_fillable() {
        let levels = vec![
            (dec!(0.45), dec!(50.0)),
            (dec!(0.50), dec!(50.0)),
            (dec!(0.55), dec!(50.0)),
        ];

        // Can fill 100 at max 0.50 with 2% slippage (limit = 0.51)
        let (can_fill, available) =
            check_fok_fillable(&levels, dec!(100.0), dec!(0.50), dec!(0.02));
        assert!(can_fill);
        assert_eq!(available, dec!(100.0));

        // Cannot fill 100 at max 0.45 with 2% slippage (limit = 0.459)
        let (can_fill, available) =
            check_fok_fillable(&levels, dec!(100.0), dec!(0.45), dec!(0.02));
        assert!(!can_fill);
        assert_eq!(available, dec!(50.0));
    }

    #[test]
    fn test_vwap_exact_fill() {
        // Edge case: exactly enough liquidity
        let levels = vec![(dec!(0.50), dec!(100.0))];
        assert_eq!(calculate_vwap(&levels, dec!(100.0)), Some(dec!(0.50)));
    }

    #[test]
    fn test_vwap_zero_target() {
        let levels = vec![(dec!(0.50), dec!(100.0))];
        assert_eq!(calculate_vwap(&levels, dec!(0.0)), None);
    }
}
