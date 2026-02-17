//! Order Builder for Polymarket CLOB
//!
//! Converts price/size (Decimal) to EIP-712 Order structs with proper
//! makerAmount/takerAmount calculations.

use alloy::primitives::{Address, U256};
use moonbag_core::Result;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;

use crate::order_signer::{determine_signature_type, generate_salt, Order, SIDE_BUY, SIDE_SELL};

// ─────────────────────────────────────────────────────────────────────────────
// Constants
// ─────────────────────────────────────────────────────────────────────────────

/// Scale factor for 6 decimals (both USDC and CTF tokens use 6 decimals)
const SCALE_FACTOR: u64 = 1_000_000;

// ─────────────────────────────────────────────────────────────────────────────
// Order Builder
// ─────────────────────────────────────────────────────────────────────────────

/// Builder for creating properly formatted orders
pub struct OrderBuilder {
    /// Safe/funder address (maker)
    maker: Address,
    /// EOA address (signer)
    signer: Address,
}

impl OrderBuilder {
    /// Create a new order builder
    ///
    /// # Arguments
    /// * `maker` - Safe/funder address
    /// * `signer` - EOA signer address
    pub fn new(maker: Address, signer: Address) -> Self {
        Self { maker, signer }
    }

    /// Build a BUY order
    ///
    /// For BUY orders:
    /// - makerAmount = USDC to pay (price * size * 10^6)
    /// - takerAmount = tokens to receive (size * 10^6)
    ///
    /// IMPORTANT: Polymarket GTC orders require:
    /// - maker_amount (USDC): max 4 decimals
    /// - taker_amount (tokens): max 2 decimals
    ///
    /// # Arguments
    /// * `token_id` - CTF token ID (as string, will be parsed)
    /// * `price` - Price per token (0.0 to 1.0)
    /// * `size` - Number of tokens to buy
    pub fn build_buy_order(&self, token_id: &str, price: Decimal, size: Decimal) -> Result<Order> {
        let token_id_u256 = parse_token_id(token_id)?;

        // BUY: pay USDC, receive tokens
        // Round to Polymarket GTC precision requirements:
        // - Tokens (taker): 2 decimals (GTC requirement)
        // - USDC (maker): 5 decimals (API validates exact: price × size)
        let size_rounded = size.round_dp(2);
        let usdc_human = (price * size_rounded).round_dp(5);

        // makerAmount = USDC in wei (6 decimals)
        // takerAmount = tokens in wei (6 decimals)
        let usdc_amount = usdc_human * Decimal::from(SCALE_FACTOR);
        let token_amount = size_rounded * Decimal::from(SCALE_FACTOR);

        let maker_amount = decimal_to_u256(usdc_amount)?;
        let taker_amount = decimal_to_u256(token_amount)?;

        Ok(Order {
            salt: generate_salt(),
            maker: self.maker,
            signer: self.signer,
            taker: Address::ZERO,
            tokenId: token_id_u256,
            makerAmount: maker_amount,
            takerAmount: taker_amount,
            expiration: U256::ZERO,
            nonce: U256::ZERO,
            feeRateBps: U256::ZERO,
            side: SIDE_BUY,
            signatureType: determine_signature_type(self.maker, self.signer),
        })
    }

    /// Build a SELL order
    ///
    /// For SELL orders:
    /// - makerAmount = tokens to sell (size * 10^6)
    /// - takerAmount = USDC to receive (price * size * 10^6)
    ///
    /// IMPORTANT: Polymarket requires:
    /// - maker_amount (tokens): max 2 decimals
    /// - taker_amount (USDC): max 4 decimals
    ///
    /// # Arguments
    /// * `token_id` - CTF token ID (as string, will be parsed)
    /// * `price` - Price per token (0.0 to 1.0)
    /// * `size` - Number of tokens to sell
    pub fn build_sell_order(&self, token_id: &str, price: Decimal, size: Decimal) -> Result<Order> {
        let token_id_u256 = parse_token_id(token_id)?;

        // SELL: pay tokens, receive USDC
        // Round to Polymarket precision requirements:
        // - Tokens (maker): 2 decimals
        // - USDC (taker): 4 decimals
        let size_rounded = size.round_dp(2);
        let usdc_human = (price * size_rounded).round_dp(4);

        // makerAmount = tokens in wei (6 decimals)
        // takerAmount = USDC in wei (6 decimals)
        let token_amount = size_rounded * Decimal::from(SCALE_FACTOR);
        let usdc_amount = usdc_human * Decimal::from(SCALE_FACTOR);

        let maker_amount = decimal_to_u256(token_amount)?;
        let taker_amount = decimal_to_u256(usdc_amount)?;

        Ok(Order {
            salt: generate_salt(),
            maker: self.maker,
            signer: self.signer,
            taker: Address::ZERO,
            tokenId: token_id_u256,
            makerAmount: maker_amount,
            takerAmount: taker_amount,
            expiration: U256::ZERO,
            nonce: U256::ZERO,
            feeRateBps: U256::ZERO,
            side: SIDE_SELL,
            signatureType: determine_signature_type(self.maker, self.signer),
        })
    }

    /// Get maker address
    pub fn maker(&self) -> Address {
        self.maker
    }

    /// Get signer address
    pub fn signer(&self) -> Address {
        self.signer
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Parse token ID string to U256
fn parse_token_id(token_id: &str) -> Result<U256> {
    // Token IDs are large numbers, parse directly
    token_id
        .parse::<U256>()
        .map_err(|e| moonbag_core::MoonbagError::InvalidConfig(format!("Invalid token ID: {}", e)))
}

/// Convert Decimal to U256
fn decimal_to_u256(value: Decimal) -> Result<U256> {
    // Truncate to integer (we've already scaled by 10^6)
    let int_value = value.trunc();

    // Convert to u128 first, then to U256
    let u128_value = int_value.to_u128().ok_or_else(|| {
        moonbag_core::MoonbagError::InvalidConfig(format!("Value too large for U256: {}", value))
    })?;

    Ok(U256::from(u128_value))
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn test_build_buy_order() {
        let maker: Address = "0x1234567890123456789012345678901234567890"
            .parse()
            .unwrap();
        let signer: Address = "0xabcdefabcdefabcdefabcdefabcdefabcdefabcd"
            .parse()
            .unwrap();

        let builder = OrderBuilder::new(maker, signer);

        // Buy 100 tokens at price 0.50
        let price = Decimal::from_str("0.50").unwrap();
        let size = Decimal::from_str("100").unwrap();

        let order = builder
            .build_buy_order("12345678901234567890", price, size)
            .unwrap();

        // makerAmount = 0.50 * 100 * 10^6 = 50_000_000 USDC wei
        assert_eq!(order.makerAmount, U256::from(50_000_000u64));

        // takerAmount = 100 * 10^6 = 100_000_000 token wei
        assert_eq!(order.takerAmount, U256::from(100_000_000u64));

        assert_eq!(order.side, SIDE_BUY);
        assert_eq!(order.maker, maker);
        assert_eq!(order.signer, signer);
    }

    #[test]
    fn test_build_sell_order() {
        let maker: Address = "0x1234567890123456789012345678901234567890"
            .parse()
            .unwrap();
        let signer: Address = "0xabcdefabcdefabcdefabcdefabcdefabcdefabcd"
            .parse()
            .unwrap();

        let builder = OrderBuilder::new(maker, signer);

        // Sell 100 tokens at price 0.75
        let price = Decimal::from_str("0.75").unwrap();
        let size = Decimal::from_str("100").unwrap();

        let order = builder
            .build_sell_order("12345678901234567890", price, size)
            .unwrap();

        // makerAmount = 100 * 10^6 = 100_000_000 token wei
        assert_eq!(order.makerAmount, U256::from(100_000_000u64));

        // takerAmount = 0.75 * 100 * 10^6 = 75_000_000 USDC wei
        assert_eq!(order.takerAmount, U256::from(75_000_000u64));

        assert_eq!(order.side, SIDE_SELL);
    }

    #[test]
    fn test_small_amounts() {
        let maker: Address = "0x1234567890123456789012345678901234567890"
            .parse()
            .unwrap();
        let signer: Address = "0xabcdefabcdefabcdefabcdefabcdefabcdefabcd"
            .parse()
            .unwrap();

        let builder = OrderBuilder::new(maker, signer);

        // Buy 0.5 tokens at price 0.10
        let price = Decimal::from_str("0.10").unwrap();
        let size = Decimal::from_str("0.5").unwrap();

        let order = builder
            .build_buy_order("12345678901234567890", price, size)
            .unwrap();

        // makerAmount = 0.10 * 0.5 * 10^6 = 50_000 USDC wei
        assert_eq!(order.makerAmount, U256::from(50_000u64));

        // takerAmount = 0.5 * 10^6 = 500_000 token wei
        assert_eq!(order.takerAmount, U256::from(500_000u64));
    }

    #[test]
    fn test_parse_large_token_id() {
        let token_id =
            "48331043336612883890938759509493159234755048973500640148014422747788308965732";
        let result = parse_token_id(token_id);
        assert!(result.is_ok());
    }
}
