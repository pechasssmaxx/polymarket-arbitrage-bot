//! Moonbag Executor - Trade execution pipeline
//!
//! Executes moonbag opportunities through the full pipeline:
//! 1. Pre-check: Verify opportunity is still valid
//! 2. Buy: Place GTC orders for NO tokens
//! 3. Settlement: Wait for tokens to appear in wallet
//! 4. Convert: Call NegRiskAdapter.convertPositions via Safe
//! 5. Sell: Place FAK orders for sellable YES tokens
//!
//! ## Usage
//!
//! ```ignore
//! use moonbag_executor::Executor;
//! use moonbag_core::Config;
//!
//! let config = Config::from_env()?;
//! let executor = Executor::from_config(&config)?;
//!
//! let result = executor.execute(&opportunity).await?;
//! if result.success {
//!     println!("Profit: ${}", result.profit);
//! }
//! ```

pub mod buy;
pub mod clob_auth;
pub mod convert;
pub mod order_builder;
pub mod order_signer;
pub mod pre_check;
pub mod sell;
pub mod settlement;

use buy::BuyExecutor;
use convert::ConvertExecutor;
use pre_check::PreChecker;
use sell::{SellAllResult, SellExecutor};
use settlement::SettlementWaiter;

use moonbag_core::models::{ExecutionResult, MoonbagOpportunity};
use moonbag_core::{Config, Result};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::time::Instant;
use tracing::{debug, error, info, warn};

/// Executor configuration
#[derive(Debug, Clone)]
pub struct ExecutorConfig {
    /// Skip pre-check verification
    pub skip_precheck: bool,
    /// Dry run mode (no real trades)
    pub dry_run: bool,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            skip_precheck: false,
            dry_run: true,
        }
    }
}

impl From<&Config> for ExecutorConfig {
    fn from(config: &Config) -> Self {
        Self {
            skip_precheck: config.skip_precheck,
            dry_run: config.dry_run,
        }
    }
}

/// Main executor for moonbag opportunities
pub struct Executor {
    config: ExecutorConfig,
    pre_checker: PreChecker,
    buy_executor: BuyExecutor,
    settlement_waiter: SettlementWaiter,
    convert_executor: ConvertExecutor,
    sell_executor: SellExecutor,
}

impl Executor {
    /// Create a new executor from global config
    pub fn from_config(config: &Config) -> Result<Self> {
        Ok(Self {
            config: ExecutorConfig::from(config),
            pre_checker: PreChecker::from_config(config),
            buy_executor: BuyExecutor::from_config(config),
            settlement_waiter: SettlementWaiter::from_config(config)?,
            convert_executor: ConvertExecutor::from_config(config),
            sell_executor: SellExecutor::from_config(config),
        })
    }

    /// Execute a single opportunity through the full pipeline
    ///
    /// Returns ExecutionResult with success/failure status, profit, and notes.
    pub async fn execute(&self, opp: &MoonbagOpportunity) -> ExecutionResult {
        let start = Instant::now();

        info!(
            market = %opp.market_slug,
            k = opp.k,
            m = opp.m,
            amount = %opp.amount,
            expected_profit = %opp.guaranteed_profit,
            "Starting execution"
        );

        // Stage 1: Pre-check
        if !self.config.skip_precheck {
            match self.pre_checker.verify(opp).await {
                Ok(result) => {
                    info!(
                        current_profit = %result.current_profit,
                        delta = %result.profit_delta,
                        "Pre-check passed"
                    );
                }
                Err(e) => {
                    warn!(error = %e, "Pre-check failed");
                    return ExecutionResult::failure(e.to_string(), "precheck");
                }
            }
        } else {
            debug!("Skipping pre-check");
        }

        // Stage 2: Buy NO tokens
        let buy_result = match self.buy_executor.buy_all(opp).await {
            Ok(r) => r,
            Err(e) => {
                error!(error = %e, "Buy stage failed");
                return ExecutionResult::failure(e.to_string(), "buy");
            }
        };

        if !buy_result.success && !buy_result.partial_k {
            return ExecutionResult::failure(
                buy_result
                    .error
                    .unwrap_or_else(|| "No orders filled".to_string()),
                "buy",
            );
        }

        let partial_k = buy_result.partial_k;
        let filled_outcomes = buy_result.results.iter().filter(|r| r.success).count();

        if partial_k {
            warn!(filled = filled_outcomes, expected = opp.k, "Partial K fill");
            if !opp.allow_partial_k {
                return ExecutionResult::failure(
                    format!(
                        "Partial fill {}/{} and partial_k disabled",
                        filled_outcomes, opp.k
                    ),
                    "buy",
                );
            }
        }

        // Calculate effective amount for settlement (may be adjusted from original)
        // Convert Decimal to raw (6 decimals = multiply by 1_000_000)
        let effective_amount_raw = (buy_result.effective_amount * Decimal::new(1_000_000, 0))
            .to_u64()
            .unwrap_or(opp.amount_raw);

        if buy_result.amount_adjusted {
            info!(
                original_raw = opp.amount_raw,
                effective_raw = effective_amount_raw,
                "Using adjusted amount for convert"
            );
        }

        // Stage 3: Brief pause before CONVERT
        // CLOB settlements are atomic - if buy succeeded, tokens are already in wallet
        // Just wait 2 seconds for any RPC propagation delay
        info!("Buy complete, waiting 2s before CONVERT...");
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        // Use MINIMUM filled amount across outcomes for CONVERT
        // CONVERT requires equal amounts on all K outcomes - we can only convert the min
        let convertible = buy_result.convertible_amount();
        let convertible_amount = (convertible * Decimal::new(1_000_000, 0))
            .to_u64()
            .unwrap_or(effective_amount_raw);

        // Log if there are excess shares that won't be converted
        if buy_result.excess_shares > Decimal::ZERO {
            warn!(
                min_filled = %buy_result.min_filled,
                max_filled = %buy_result.max_filled,
                excess_shares = %buy_result.excess_shares,
                convertible_amount = convertible_amount,
                "Uneven fills: only converting min_filled, excess shares will remain unconverted"
            );
        }

        // Stage 4: CONVERT
        // Use execute_from_buy_result to calculate index_set from ACTUALLY BOUGHT outcomes
        // This is critical - we must only convert outcomes that were successfully purchased
        let convert_result = match self
            .convert_executor
            .execute_from_buy_result(opp, &buy_result, convertible_amount)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                error!(error = %e, "CONVERT failed");
                return ExecutionResult::failure_manual(e.to_string(), "convert");
            }
        };

        if !convert_result.success {
            return ExecutionResult::failure_manual(
                convert_result
                    .error
                    .unwrap_or_else(|| "CONVERT failed".to_string()),
                "convert",
            );
        }

        let converted_amount = convert_result.amount_converted;

        // Stage 5: Sell YES tokens
        let sell_result = match self.sell_executor.sell_all(opp, converted_amount).await {
            Ok(r) => r,
            Err(e) => {
                // Sell failure is not critical - we still made profit from CONVERT
                warn!(error = %e, "Sell stage failed (non-critical)");
                SellAllResult::new()
            }
        };

        // Calculate actual profit
        let usdc_returned = convert_result.usdc_returned;
        let yes_revenue = sell_result.total_revenue;
        let no_cost = buy_result.total_cost;
        let actual_profit = (usdc_returned + yes_revenue) - no_cost - opp.gas_cost;

        let elapsed = start.elapsed();

        info!(
            usdc_returned = %usdc_returned,
            yes_revenue = %yes_revenue,
            no_cost = %no_cost,
            gas_cost = %opp.gas_cost,
            actual_profit = %actual_profit,
            expected_profit = %opp.guaranteed_profit,
            elapsed_ms = elapsed.as_millis(),
            moonbags = opp.moonbag_count,
            "Execution complete"
        );

        let mut result = ExecutionResult::success(actual_profit, convert_result.tx_hash);
        result.converted_amount = converted_amount;
        result.partial_k = partial_k;

        // Add notes
        result = result.with_note(format!("K={}/{}", filled_outcomes, opp.k));
        result = result.with_note(format!("Moonbags: {}", opp.moonbag_count));
        if partial_k {
            result = result.with_note("PARTIAL_K");
        }
        if self.config.dry_run {
            result = result.with_note("DRY_RUN");
        }

        result
    }

    /// Execute with retry logic
    ///
    /// Retries on transient errors up to max_retries times.
    pub async fn execute_with_retry(
        &self,
        opp: &MoonbagOpportunity,
        max_retries: u8,
    ) -> ExecutionResult {
        let mut last_result = ExecutionResult::failure("No attempts made", "init");

        for attempt in 0..=max_retries {
            if attempt > 0 {
                info!(attempt = attempt, "Retrying execution");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }

            last_result = self.execute(opp).await;

            if last_result.success {
                return last_result;
            }

            // Don't retry if manual intervention needed
            if last_result.needs_manual_intervention {
                warn!("Manual intervention needed, not retrying");
                break;
            }

            // Don't retry on certain errors
            let stage = &last_result.stage;
            if stage == "precheck" {
                // Precheck failures are expected (opportunity no longer valid), don't retry
                break;
            }
            if stage == "buy" {
                // Buy failures: only safe to retry if ZERO outcomes were purchased
                // If any outcomes were bought, retrying would cause double-buy or leave us
                // holding partial position with no CONVERT
                // For now, don't retry buy failures - need smarter partial state tracking
                warn!(stage = %stage, "Buy failed - not retrying (partial state risk)");
                break;
            }
            // Other failures (convert, sell) may be transient - allow retry
        }

        last_result
    }

    /// Execute multiple cycles (for farm mode)
    ///
    /// Runs up to max_cycles executions, stopping on failure or total loss.
    pub async fn execute_cycles(&self, opp: &MoonbagOpportunity) -> Vec<ExecutionResult> {
        let mut results = Vec::new();
        let mut total_profit = Decimal::ZERO;

        let max_cycles = opp.max_cycles.max(1);

        for cycle in 0..max_cycles {
            info!(
                cycle = cycle + 1,
                max_cycles = max_cycles,
                total_profit = %total_profit,
                "Starting cycle"
            );

            let result = self.execute(opp).await;
            total_profit += result.profit;

            let success = result.success;
            let needs_manual = result.needs_manual_intervention;
            results.push(result);

            if !success {
                warn!(cycle = cycle + 1, "Cycle failed, stopping");
                break;
            }

            if needs_manual {
                warn!("Manual intervention needed, stopping cycles");
                break;
            }

            // Check total loss limit
            if opp.max_total_loss > Decimal::ZERO && total_profit <= -opp.max_total_loss {
                warn!(
                    total_profit = %total_profit,
                    max_total_loss = %opp.max_total_loss,
                    "Max total loss reached, stopping"
                );
                break;
            }
        }

        info!(
            cycles = results.len(),
            total_profit = %total_profit,
            "Cycles complete"
        );

        results
    }
}

// Re-export key types
pub use buy::{BuyConfig, BuyResult};
pub use convert::{ConvertConfig, ConvertExecutorResult};
pub use pre_check::{PreCheckConfig, PreCheckResult};
pub use sell::{SellConfig, SellResult};
pub use settlement::{SettlementConfig, SettlementResult};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_executor_config_default() {
        let config = ExecutorConfig::default();
        assert!(config.dry_run);
        assert!(!config.skip_precheck);
    }
}
