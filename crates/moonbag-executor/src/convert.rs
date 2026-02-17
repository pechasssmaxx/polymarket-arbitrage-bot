//! CONVERT execution orchestration
//!
//! Calls NegRiskAdapter.convertPositions() via Python relayer script to:
//! - Burn K NO tokens
//! - Receive (K-1) USDC
//! - Receive 1 YES token per outcome (moonbags!)
//!
//! The Python script (scripts/convert_retry.py) handles:
//! - Polymarket Builder Relayer API for gasless transactions
//! - Automatic retry on timeout (up to 20 attempts)
//! - JSON output for Rust to parse

use crate::buy::BuyAllResult;
use moonbag_core::models::{MoonbagOpportunity, SelectedOutcome};
use moonbag_core::{Config, MoonbagError, Result};
use rust_decimal::Decimal;
use serde::Deserialize;
use std::process::Stdio;
use tokio::process::Command;
use tracing::{debug, info, warn};

/// Convert executor configuration
#[derive(Debug, Clone)]
pub struct ConvertConfig {
    /// Dry run mode
    pub dry_run: bool,
    /// Path to convert_retry.py script
    pub script_path: String,
    /// Max retry attempts
    pub max_retries: u8,
}

impl Default for ConvertConfig {
    fn default() -> Self {
        Self {
            dry_run: true,
            script_path: "scripts/convert_retry.py".to_string(),
            max_retries: 20,
        }
    }
}

impl From<&Config> for ConvertConfig {
    fn from(config: &Config) -> Self {
        Self {
            dry_run: config.dry_run,
            script_path: "scripts/convert_retry.py".to_string(),
            max_retries: 20,
        }
    }
}

/// Response from convert_retry.py script
#[derive(Debug, Deserialize)]
struct PythonConvertResponse {
    success: bool,
    #[serde(default)]
    tx_hash: Option<String>,
    #[serde(default)]
    tx_id: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    k: Option<u8>,
    #[serde(default)]
    amount: Option<u64>,
    #[serde(default)]
    usdc_received: Option<f64>,
    #[serde(default)]
    error: Option<String>,
}

/// Result of CONVERT operation
#[derive(Debug, Clone)]
pub struct ConvertExecutorResult {
    pub success: bool,
    /// Transaction hash
    pub tx_hash: Option<String>,
    /// Amount converted (in tokens, not raw)
    pub amount_converted: Decimal,
    /// Amount in raw units
    pub amount_raw: u64,
    /// USDC returned = (K-1) * amount
    pub usdc_returned: Decimal,
    /// Gas used
    pub gas_used: Option<u64>,
    pub error: Option<String>,
}

impl ConvertExecutorResult {
    pub fn success(tx_hash: String, amount_raw: u64, k: u8, gas_used: Option<u64>) -> Self {
        let amount = Decimal::from(amount_raw) / Decimal::new(1_000_000, 0);
        let usdc_returned = amount * Decimal::from(k - 1);

        Self {
            success: true,
            tx_hash: Some(tx_hash),
            amount_converted: amount,
            amount_raw,
            usdc_returned,
            gas_used,
            error: None,
        }
    }

    pub fn failure(error: impl Into<String>) -> Self {
        Self {
            success: false,
            tx_hash: None,
            amount_converted: Decimal::ZERO,
            amount_raw: 0,
            usdc_returned: Decimal::ZERO,
            gas_used: None,
            error: Some(error.into()),
        }
    }

    pub fn dry_run(amount_raw: u64, k: u8) -> Self {
        let amount = Decimal::from(amount_raw) / Decimal::new(1_000_000, 0);
        let usdc_returned = amount * Decimal::from(k - 1);

        Self {
            success: true,
            tx_hash: Some("dry-run-tx-hash".to_string()),
            amount_converted: amount,
            amount_raw,
            usdc_returned,
            gas_used: None,
            error: None,
        }
    }
}

/// CONVERT executor
pub struct ConvertExecutor {
    config: ConvertConfig,
}

impl ConvertExecutor {
    /// Create a new CONVERT executor
    pub fn new(config: ConvertConfig) -> Self {
        Self { config }
    }

    /// Create from global config
    pub fn from_config(config: &Config) -> Self {
        Self::new(ConvertConfig::from(config))
    }

    /// Calculate index_set from convert indices
    ///
    /// Each outcome has a `convert_index` (0..M-1).
    /// index_set is a bitmap: `sum(1 << convert_index for each outcome)`
    pub fn calculate_index_set(indices: &[u8]) -> u128 {
        indices.iter().fold(0u128, |acc, &i| acc | (1u128 << i))
    }

    /// Get convert indices from successfully bought outcomes
    ///
    /// Matches bought token IDs to their convert_index from selected_outcomes
    pub fn get_bought_indices(
        buy_result: &BuyAllResult,
        selected_outcomes: &[SelectedOutcome],
    ) -> Vec<u8> {
        buy_result
            .results
            .iter()
            .filter(|r| r.success)
            .filter_map(|r| {
                selected_outcomes
                    .iter()
                    .find(|o| o.no_token_id == r.token_id)
                    .map(|o| o.alphabetical_index)
            })
            .collect()
    }

    /// Execute CONVERT on an opportunity
    ///
    /// # Arguments
    /// * `opp` - The opportunity containing market and outcome info
    /// * `amount_raw` - Amount to convert (may be less than opp.amount_raw if capped)
    pub async fn execute(
        &self,
        opp: &MoonbagOpportunity,
        amount_raw: u64,
    ) -> Result<ConvertExecutorResult> {
        self.execute_with_index_set(opp, amount_raw, opp.index_set, opp.k).await
    }

    /// Execute CONVERT with specific index_set and K
    ///
    /// Use this when you need to provide a custom index_set (e.g., after partial fill)
    ///
    /// # Arguments
    /// * `opp` - The opportunity containing market info
    /// * `amount_raw` - Amount to convert
    /// * `index_set` - Bitmap of outcomes to convert (calculated from bought indices)
    /// * `k` - Number of outcomes in index_set
    pub async fn execute_with_index_set(
        &self,
        opp: &MoonbagOpportunity,
        amount_raw: u64,
        index_set: u128,
        k: u8,
    ) -> Result<ConvertExecutorResult> {
        info!(
            market = %opp.market_slug,
            market_id = %opp.neg_risk_market_id,
            k = k,
            index_set = index_set,
            amount_raw = amount_raw,
            "Executing CONVERT via Python relayer"
        );

        // Apply convert cap if specified
        let final_amount = if opp.convert_cap > Decimal::ZERO {
            let cap_raw = (opp.convert_cap * Decimal::new(1_000_000, 0))
                .to_string()
                .parse::<u64>()
                .unwrap_or(amount_raw);
            amount_raw.min(cap_raw)
        } else {
            amount_raw
        };

        debug!(
            original_amount = amount_raw,
            capped_amount = final_amount,
            convert_cap = %opp.convert_cap,
            "Amount after cap"
        );

        if self.config.dry_run {
            info!(
                market_id = %opp.neg_risk_market_id,
                index_set = index_set,
                amount = final_amount,
                k = k,
                "DRY RUN: Would call convertPositions via Python"
            );
            return Ok(ConvertExecutorResult::dry_run(final_amount, k));
        }

        // Call Python convert_retry.py script
        let result = self
            .call_python_convert(&opp.neg_risk_market_id, index_set, final_amount)
            .await?;

        if result.success {
            let tx_hash = result.tx_hash.unwrap_or_default();
            info!(
                tx_hash = %tx_hash,
                amount = final_amount,
                k = k,
                usdc = ?result.usdc_received,
                "CONVERT successful"
            );
            Ok(ConvertExecutorResult::success(tx_hash, final_amount, k, None))
        } else {
            let error = result.error.unwrap_or_else(|| "Unknown error".to_string());
            warn!(
                error = %error,
                tx_hash = ?result.tx_hash,
                "CONVERT failed"
            );
            Err(MoonbagError::SafeExecution {
                reason: error,
                tx_hash: result.tx_hash,
            })
        }
    }

    /// Execute CONVERT after buy phase, using actually bought indices
    ///
    /// This is the recommended method - it calculates index_set from actual buys
    ///
    /// # Arguments
    /// * `opp` - The opportunity
    /// * `buy_result` - Result from buy phase (contains which orders succeeded)
    /// * `amount_raw` - Amount to convert
    pub async fn execute_from_buy_result(
        &self,
        opp: &MoonbagOpportunity,
        buy_result: &BuyAllResult,
        amount_raw: u64,
    ) -> Result<ConvertExecutorResult> {
        // Get indices of successfully bought outcomes
        let bought_indices = Self::get_bought_indices(buy_result, &opp.selected_outcomes);
        let k = bought_indices.len() as u8;

        if k < 2 {
            return Err(MoonbagError::Internal(format!(
                "Need at least 2 bought outcomes for CONVERT, got {}",
                k
            )));
        }

        // Calculate index_set from bought indices
        let index_set = Self::calculate_index_set(&bought_indices);

        info!(
            bought_indices = ?bought_indices,
            calculated_index_set = index_set,
            k = k,
            "Calculated index_set from buy results"
        );

        self.execute_with_index_set(opp, amount_raw, index_set, k).await
    }

    /// Call Python convert_retry.py script
    async fn call_python_convert(
        &self,
        market_id: &str,
        index_set: u128,
        amount: u64,
    ) -> Result<PythonConvertResponse> {
        let script_path = &self.config.script_path;

        info!(
            script = %script_path,
            market_id = %market_id,
            index_set = index_set,
            amount = amount,
            max_retries = self.config.max_retries,
            "Calling Python convert script"
        );

        let output = Command::new("python3")
            .arg(script_path)
            .arg(market_id)
            .arg(index_set.to_string())
            .arg(amount.to_string())
            .arg(self.config.max_retries.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| MoonbagError::Internal(format!("Failed to execute Python script: {}", e)))?;

        // Log stderr (contains progress info)
        let stderr = String::from_utf8_lossy(&output.stderr);
        for line in stderr.lines() {
            debug!(target: "convert_python", "{}", line);
        }

        // Parse JSON from stdout (last line)
        let stdout = String::from_utf8_lossy(&output.stdout);
        let json_line = stdout
            .lines()
            .last()
            .ok_or_else(|| MoonbagError::Internal("No output from Python script".to_string()))?;

        debug!(json_output = %json_line, "Python script output");

        let response: PythonConvertResponse = serde_json::from_str(json_line).map_err(|e| {
            MoonbagError::Internal(format!(
                "Failed to parse Python output '{}': {}",
                json_line, e
            ))
        })?;

        Ok(response)
    }

    /// Calculate estimated gas for CONVERT
    ///
    /// Gas scales with number of outcomes (M):
    /// Base: 500k + M * 100k
    pub fn estimate_gas(m: u8) -> u64 {
        500_000 + (m as u64) * 100_000
    }

    /// Calculate gas cost in USD
    pub fn gas_cost_usd(gas: u64, gas_price_gwei: Decimal, pol_price: Decimal) -> Decimal {
        let gas_cost_wei = Decimal::from(gas) * gas_price_gwei * Decimal::new(1_000_000_000, 0);
        // 10^18 wei per POL
        let wei_per_pol = Decimal::from(1_000_000_000_000_000_000u64);
        let gas_cost_pol = gas_cost_wei / wei_per_pol;
        gas_cost_pol * pol_price
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_convert_result() {
        let result = ConvertExecutorResult::success(
            "0xabc123".to_string(),
            100_000_000, // 100 tokens in raw
            3,           // K=3
            Some(1_500_000),
        );

        assert!(result.success);
        assert_eq!(result.amount_converted, dec!(100.0));
        assert_eq!(result.usdc_returned, dec!(200.0)); // (3-1) * 100

        let result = ConvertExecutorResult::failure("Gas estimation failed");
        assert!(!result.success);
    }

    #[test]
    fn test_estimate_gas() {
        assert_eq!(ConvertExecutor::estimate_gas(5), 1_000_000);
        assert_eq!(ConvertExecutor::estimate_gas(10), 1_500_000);
    }

    #[test]
    fn test_gas_cost_usd() {
        let gas = 1_500_000u64;
        let gas_price = dec!(35); // 35 gwei
        let pol_price = dec!(0.50);

        let cost = ConvertExecutor::gas_cost_usd(gas, gas_price, pol_price);
        // Expected: 1.5M * 35 gwei = 52.5M gwei = 0.0525 POL * $0.50 = $0.02625
        assert!(cost > dec!(0.02) && cost < dec!(0.03));
    }
}
