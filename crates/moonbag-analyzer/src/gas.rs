//! Gas estimation for CONVERT operations

use rust_decimal::Decimal;

/// Base gas for CONVERT operation
///
/// Measured from real transactions (e.g., tx 0x32be05a4... used 291k for K=7)
/// Previous estimate (500k) was 3-4x too high.
const BASE_GAS: u64 = 150_000;

/// Gas per outcome in the market
///
/// Measured: 291k total for 7 outcomes → ~20k per outcome after base
/// Previous estimate (100k) was 5x too high.
const PER_OUTCOME_GAS: u64 = 20_000;

/// Estimate gas for CONVERT based on market size.
///
/// Formula: base 150k + 20k per outcome
/// - K=5:  150k + 100k = 250k gas
/// - K=7:  150k + 140k = 290k gas (verified: tx 0x32be05a4... used 291k)
/// - K=10: 150k + 200k = 350k gas
#[inline]
pub const fn estimate_convert_gas(outcome_count: usize) -> u64 {
    BASE_GAS + (outcome_count as u64 * PER_OUTCOME_GAS)
}

/// Convert gas to USD cost.
///
/// # Arguments
/// * `gas` - Gas units
/// * `gas_price_gwei` - Gas price in gwei
/// * `pol_usd` - POL/MATIC price in USD
#[inline]
pub fn estimate_gas_cost_usd(gas: u64, gas_price_gwei: Decimal, pol_usd: Decimal) -> Decimal {
    let gas_dec = Decimal::from(gas);
    let gwei_to_pol = Decimal::new(1, 9); // 1e-9
    let gas_cost_pol = gas_dec * gas_price_gwei * gwei_to_pol;
    gas_cost_pol * pol_usd
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_estimate_convert_gas() {
        // New estimates based on real tx data
        assert_eq!(estimate_convert_gas(5), 250_000);   // 150k + 5*20k
        assert_eq!(estimate_convert_gas(7), 290_000);   // 150k + 7*20k ≈ 291k actual
        assert_eq!(estimate_convert_gas(10), 350_000);  // 150k + 10*20k
    }

    #[test]
    fn test_gas_cost_usd() {
        // 1M gas @ 35 gwei, POL = $0.50
        // = 1_000_000 * 35 * 1e-9 * 0.50
        // = 0.035 * 0.50 = $0.0175
        let cost = estimate_gas_cost_usd(1_000_000, dec!(35), dec!(0.50));
        assert_eq!(cost, dec!(0.0175));
    }
}
