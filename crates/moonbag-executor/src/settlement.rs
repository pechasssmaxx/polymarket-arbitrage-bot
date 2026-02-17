//! Settlement waiting (balance polling)
//!
//! Waits for NO tokens to appear in the Safe wallet after CLOB orders fill.
//! Uses ERC1155 balance polling with configurable timeout.

use moonbag_contracts::Erc1155Client;
use moonbag_core::models::MoonbagOpportunity;
use moonbag_core::{Config, MoonbagError, Result};
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::{debug, info};

/// Settlement configuration
#[derive(Debug, Clone)]
pub struct SettlementConfig {
    /// Maximum wait time for settlement
    pub max_wait: Duration,
    /// Poll interval
    pub poll_interval: Duration,
    /// RPC URL for balance checks
    pub rpc_url: String,
    /// Wallet address to check
    pub wallet: String,
    /// Dry run mode
    pub dry_run: bool,
}

impl Default for SettlementConfig {
    fn default() -> Self {
        Self {
            max_wait: Duration::from_secs(180),
            poll_interval: Duration::from_secs(2),
            rpc_url: String::new(),
            wallet: String::new(),
            dry_run: true,
        }
    }
}

impl From<&Config> for SettlementConfig {
    fn from(config: &Config) -> Self {
        Self {
            max_wait: Duration::from_secs(config.settlement_max_wait_secs),
            poll_interval: Duration::from_millis(config.settlement_poll_interval_ms),
            rpc_url: config.polygon_rpc_url.clone(),
            wallet: config.polymarket_funder.clone(),
            dry_run: config.dry_run,
        }
    }
}

/// Result of settlement wait
#[derive(Debug, Clone)]
pub struct SettlementResult {
    pub success: bool,
    /// Actual balances found per token
    pub balances: HashMap<String, u64>,
    /// Total tokens confirmed
    pub total_amount: Decimal,
    /// Wait time in seconds
    pub wait_secs: f64,
    pub error: Option<String>,
}

impl SettlementResult {
    pub fn success(balances: HashMap<String, u64>, wait_secs: f64) -> Self {
        let total_amount: u64 = balances.values().sum();
        Self {
            success: true,
            balances,
            total_amount: Decimal::from(total_amount) / Decimal::new(1_000_000, 0), // USDC decimals
            wait_secs,
            error: None,
        }
    }

    pub fn failure(error: impl Into<String>, wait_secs: f64) -> Self {
        Self {
            success: false,
            balances: HashMap::new(),
            total_amount: Decimal::ZERO,
            wait_secs,
            error: Some(error.into()),
        }
    }
}

/// Settlement waiter for NO token balances
pub struct SettlementWaiter {
    config: SettlementConfig,
    erc1155: Option<Erc1155Client>,
}

impl SettlementWaiter {
    /// Create a new settlement waiter
    pub fn new(config: SettlementConfig) -> Result<Self> {
        let erc1155 = if !config.rpc_url.is_empty() && !config.dry_run {
            Some(Erc1155Client::new(&config.rpc_url)?)
        } else {
            None
        };

        Ok(Self { config, erc1155 })
    }

    /// Create from global config
    pub fn from_config(config: &Config) -> Result<Self> {
        Self::new(SettlementConfig::from(config))
    }

    /// Wait for NO tokens to appear in wallet
    ///
    /// Returns when all expected NO tokens have non-zero balance,
    /// or when timeout is reached.
    pub async fn wait_for_settlement(
        &self,
        opp: &MoonbagOpportunity,
        expected_amount_raw: u64,
    ) -> Result<SettlementResult> {
        let start = Instant::now();

        // Get list of NO token IDs we're waiting for
        let token_ids: Vec<String> = opp
            .selected_outcomes
            .iter()
            .map(|o| o.no_token_id.clone())
            .collect();

        info!(
            k = opp.k,
            expected = expected_amount_raw,
            tokens = ?token_ids,
            "Waiting for settlement"
        );

        if self.config.dry_run {
            info!("DRY RUN: Simulating successful settlement");
            let mut balances = HashMap::new();
            for token_id in &token_ids {
                balances.insert(token_id.clone(), expected_amount_raw);
            }
            return Ok(SettlementResult::success(balances, 0.0));
        }

        let erc1155 = self
            .erc1155
            .as_ref()
            .ok_or_else(|| MoonbagError::InvalidConfig("ERC1155 client not configured".into()))?;

        // Poll until all tokens have balance or timeout
        loop {
            let elapsed = start.elapsed();

            if elapsed >= self.config.max_wait {
                return Err(MoonbagError::SettlementTimeout {
                    waited_secs: elapsed.as_secs(),
                    expected: Decimal::from(expected_amount_raw) / Decimal::new(1_000_000, 0),
                });
            }

            // Check balances
            let balances = erc1155
                .balance_of_batch(&self.config.wallet, &token_ids)
                .await?;

            // Check if all tokens have sufficient balance
            let mut all_settled = true;
            let mut missing_tokens = Vec::new();

            for token_id in &token_ids {
                let balance = balances.get(token_id).copied().unwrap_or(0);
                if balance < expected_amount_raw {
                    all_settled = false;
                    missing_tokens.push(token_id.clone());
                }
            }

            if all_settled {
                let wait_secs = elapsed.as_secs_f64();
                info!(
                    wait_secs = %wait_secs,
                    "Settlement complete"
                );
                return Ok(SettlementResult::success(balances, wait_secs));
            }

            debug!(
                elapsed_secs = elapsed.as_secs(),
                missing = ?missing_tokens,
                "Still waiting for settlement"
            );

            tokio::time::sleep(self.config.poll_interval).await;
        }
    }

    /// Get current balance for a single token
    pub async fn get_balance(&self, token_id: &str) -> Result<u64> {
        if self.config.dry_run {
            return Ok(0);
        }

        let erc1155 = self
            .erc1155
            .as_ref()
            .ok_or_else(|| MoonbagError::InvalidConfig("ERC1155 client not configured".into()))?;

        erc1155.balance_of(&self.config.wallet, token_id).await
    }

    /// Get minimum balance across all NO tokens
    ///
    /// This determines how many tokens can be converted.
    pub async fn get_convertible_amount(&self, opp: &MoonbagOpportunity) -> Result<u64> {
        if self.config.dry_run {
            return Ok(opp.amount_raw);
        }

        let erc1155 = self
            .erc1155
            .as_ref()
            .ok_or_else(|| MoonbagError::InvalidConfig("ERC1155 client not configured".into()))?;

        let token_ids: Vec<String> = opp
            .selected_outcomes
            .iter()
            .map(|o| o.no_token_id.clone())
            .collect();

        let balances = erc1155
            .balance_of_batch(&self.config.wallet, &token_ids)
            .await?;

        // Return minimum balance across all tokens
        let min_balance = balances.values().copied().min().unwrap_or(0);
        Ok(min_balance)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_settlement_result() {
        let mut balances = HashMap::new();
        balances.insert("token1".to_string(), 1_000_000);
        balances.insert("token2".to_string(), 1_000_000);

        let result = SettlementResult::success(balances, 5.5);
        assert!(result.success);
        assert_eq!(result.total_amount, Decimal::new(2, 0)); // 2.0 tokens

        let result = SettlementResult::failure("Timeout", 180.0);
        assert!(!result.success);
    }
}
