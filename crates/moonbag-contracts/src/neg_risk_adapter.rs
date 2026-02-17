//! NegRiskAdapter contract client
//!
//! Provides the convertPositions function for the moonbag strategy.
//! This converts K NO tokens into (K-1) USDC + YES tokens.

use crate::addresses::neg_risk_adapter_address;
use crate::safe_executor::SafeExecutor;
use alloy::primitives::{Address, Bytes, FixedBytes, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use moonbag_core::{MoonbagError, Result};
use tracing::{debug, info};

// NegRiskAdapter ABI
sol! {
    #[allow(missing_docs)]
    interface INegRiskAdapter {
        /// Convert positions (the core moonbag operation)
        /// @param marketId The neg risk market ID (bytes32)
        /// @param indexSet Bitmap of which outcome positions to convert
        /// @param amount Amount of each position to convert (raw, no decimals)
        function convertPositions(
            bytes32 marketId,
            uint256 indexSet,
            uint256 amount
        ) external;
    }
}

/// Result of a CONVERT operation
#[derive(Debug, Clone)]
pub struct ConvertResult {
    pub success: bool,
    pub tx_hash: Option<String>,
    pub error: Option<String>,
    pub amount: u64,
    pub gas_used: Option<u64>,
}

impl ConvertResult {
    pub fn success(tx_hash: String, amount: u64, gas_used: Option<u64>) -> Self {
        Self {
            success: true,
            tx_hash: Some(tx_hash),
            error: None,
            amount,
            gas_used,
        }
    }

    pub fn failure(error: String) -> Self {
        Self {
            success: false,
            tx_hash: None,
            error: Some(error),
            amount: 0,
            gas_used: None,
        }
    }
}

/// NegRiskAdapter client for convertPositions
pub struct NegRiskAdapterClient {
    safe_executor: SafeExecutor,
}

impl NegRiskAdapterClient {
    /// Create a new NegRiskAdapter client
    pub async fn new(rpc_url: &str, private_key: &str, safe_address: &str) -> Result<Self> {
        let safe_executor = SafeExecutor::new(rpc_url, private_key, safe_address).await?;

        Ok(Self { safe_executor })
    }

    /// Get the Safe address
    pub fn safe_address(&self) -> Address {
        self.safe_executor.safe_address()
    }

    /// Ensure CTF approval for NegRiskAdapter
    ///
    /// TODO: Implement CTF.setApprovalForAll(adapter, true) via Safe
    pub async fn ensure_approval(&mut self) -> Result<bool> {
        // TODO: Implement approval check and set
        Ok(true)
    }

    /// Call convertPositions on NegRiskAdapter
    ///
    /// # Arguments
    /// * `market_id` - The neg risk market ID (hex string, with or without 0x prefix)
    /// * `index_set` - Bitmap of which outcomes to convert (e.g., 0b11 for first 2)
    /// * `amount` - Amount per position (in raw units, no decimals)
    /// * `gas` - Optional gas limit (default: 2_000_000)
    ///
    /// # Returns
    /// ConvertResult with transaction details
    pub async fn convert_positions(
        &mut self,
        market_id: &str,
        index_set: u128,
        amount: u64,
        gas: Option<u64>,
    ) -> Result<ConvertResult> {
        // Parse market ID
        let market_id_clean = if market_id.starts_with("0x") {
            &market_id[2..]
        } else {
            market_id
        };

        let market_id_bytes: FixedBytes<32> = market_id_clean
            .parse()
            .map_err(|e| MoonbagError::InvalidConfig(format!("Invalid market ID: {}", e)))?;

        info!(
            market_id = %market_id,
            index_set = index_set,
            amount = amount,
            "Calling convertPositions"
        );

        // Encode the call
        let convert_call = INegRiskAdapter::convertPositionsCall {
            marketId: market_id_bytes,
            indexSet: U256::from(index_set),
            amount: U256::from(amount),
        };

        let calldata = Bytes::from(convert_call.abi_encode());

        debug!(calldata_len = calldata.len(), "convertPositions calldata");

        // Execute via Safe
        let adapter_addr = neg_risk_adapter_address();
        let gas = gas.unwrap_or(2_000_000);

        let result = self
            .safe_executor
            .execute(adapter_addr, calldata, U256::ZERO, Some(gas))
            .await?;

        if result.success {
            Ok(ConvertResult::success(
                result.tx_hash.unwrap_or_default(),
                amount,
                result.gas_used,
            ))
        } else {
            Ok(ConvertResult {
                success: false,
                tx_hash: result.tx_hash,
                error: result.error,
                amount,
                gas_used: result.gas_used,
            })
        }
    }

    /// Calculate the index_set for K consecutive outcomes starting from 0
    ///
    /// For K=2: returns 0b11 = 3
    /// For K=3: returns 0b111 = 7
    pub fn index_set_for_k(k: u8) -> u64 {
        (1u64 << k) - 1
    }

    /// Calculate index_set from a list of outcome indices
    ///
    /// For indices [0, 2, 3]: returns 0b1101 = 13
    pub fn index_set_from_indices(indices: &[u8]) -> u64 {
        indices.iter().fold(0u64, |acc, &i| acc | (1u64 << i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_index_set_for_k() {
        assert_eq!(NegRiskAdapterClient::index_set_for_k(2), 0b11);
        assert_eq!(NegRiskAdapterClient::index_set_for_k(3), 0b111);
        assert_eq!(NegRiskAdapterClient::index_set_for_k(4), 0b1111);
    }

    #[test]
    fn test_index_set_from_indices() {
        assert_eq!(NegRiskAdapterClient::index_set_from_indices(&[0, 1]), 0b11);
        assert_eq!(
            NegRiskAdapterClient::index_set_from_indices(&[0, 2, 3]),
            0b1101
        );
    }
}
