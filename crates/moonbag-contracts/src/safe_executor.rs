//! Gnosis Safe v1.3.0 transaction execution
//!
//! Executes on-chain transactions through a Gnosis Safe smart wallet.
//! This is needed because CLOB operations put tokens in the Smart Wallet,
//! and on-chain operations like NegRiskAdapter.convertPositions need to be
//! called FROM the Smart Wallet.
//!
//! TODO: Full alloy-rs implementation pending API stabilization

use alloy::primitives::{Address, Bytes, U256};
use moonbag_core::{MoonbagError, Result};
use std::str::FromStr;
use std::time::Duration;
use tracing::{info, warn};

/// Result of a Safe transaction
#[derive(Debug, Clone)]
pub struct SafeTxResult {
    pub success: bool,
    pub tx_hash: Option<String>,
    pub error: Option<String>,
    pub gas_used: Option<u64>,
}

impl SafeTxResult {
    pub fn success(tx_hash: String) -> Self {
        Self {
            success: true,
            tx_hash: Some(tx_hash),
            error: None,
            gas_used: None,
        }
    }

    pub fn failure(error: String) -> Self {
        Self {
            success: false,
            tx_hash: None,
            error: Some(error),
            gas_used: None,
        }
    }

    pub fn with_gas_used(mut self, gas: u64) -> Self {
        self.gas_used = Some(gas);
        self
    }
}

/// Configuration for SafeExecutor
#[derive(Debug, Clone)]
pub struct SafeExecutorConfig {
    /// Gas limit for Safe transactions
    pub default_gas: u64,
    /// Timeout for receipt waiting
    pub receipt_timeout: Duration,
    /// Poll interval for receipt
    pub poll_interval: Duration,
}

impl Default for SafeExecutorConfig {
    fn default() -> Self {
        Self {
            default_gas: 3_500_000,
            receipt_timeout: Duration::from_secs(300),
            poll_interval: Duration::from_secs(3),
        }
    }
}

/// Gnosis Safe v1.3.0 transaction executor
///
/// Implements the Safe transaction flow:
/// 1. Build Safe tx hash via getTransactionHash()
/// 2. Sign with EOA private key
/// 3. Execute via execTransaction()
/// 4. Wait for receipt and parse ExecutionSuccess/ExecutionFailure events
#[allow(dead_code)]
pub struct SafeExecutor {
    rpc_url: String,
    private_key: String,
    safe_address: Address,
    eoa_address: Address,
    config: SafeExecutorConfig,
    verified: bool,
}

impl SafeExecutor {
    /// Create a new Safe executor
    pub async fn new(rpc_url: &str, private_key: &str, safe_address: &str) -> Result<Self> {
        Self::with_config(
            rpc_url,
            private_key,
            safe_address,
            SafeExecutorConfig::default(),
        )
        .await
    }

    /// Create with custom config
    pub async fn with_config(
        rpc_url: &str,
        private_key: &str,
        safe_address: &str,
        config: SafeExecutorConfig,
    ) -> Result<Self> {
        // Parse private key (remove 0x prefix if present)
        let pk = if private_key.starts_with("0x") {
            &private_key[2..]
        } else {
            private_key
        };

        // Validate private key format (64 hex chars)
        if pk.len() != 64 || !pk.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(MoonbagError::InvalidConfig(
                "Invalid private key format".to_string(),
            ));
        }

        // Derive EOA address from private key
        // TODO: Use alloy signer to derive address
        let eoa_address = Address::ZERO; // Placeholder

        let safe_address = Address::from_str(safe_address)
            .map_err(|e| MoonbagError::InvalidConfig(format!("Invalid Safe address: {}", e)))?;

        info!(
            safe = %safe_address,
            rpc = %rpc_url,
            "SafeExecutor created"
        );

        Ok(Self {
            rpc_url: rpc_url.to_string(),
            private_key: pk.to_string(),
            safe_address,
            eoa_address,
            config,
            verified: false,
        })
    }

    /// Get EOA address
    pub fn eoa_address(&self) -> Address {
        self.eoa_address
    }

    /// Get Safe address
    pub fn safe_address(&self) -> Address {
        self.safe_address
    }

    /// Verify that our EOA owns the Safe
    ///
    /// TODO: Implement via RPC calls to Safe.getOwners() and Safe.getThreshold()
    pub async fn verify_ownership(&mut self) -> Result<bool> {
        // TODO: Implement full verification
        // For now, assume verified
        warn!("Safe ownership verification not implemented - assuming valid");
        self.verified = true;
        Ok(true)
    }

    /// Execute a transaction from the Safe
    ///
    /// TODO: Implement full Safe transaction flow:
    /// 1. Get nonce via Safe.nonce()
    /// 2. Build tx hash via Safe.getTransactionHash()
    /// 3. Sign hash with EOA
    /// 4. Call Safe.execTransaction()
    /// 5. Wait for receipt
    /// 6. Check ExecutionSuccess/ExecutionFailure events
    pub async fn execute(
        &mut self,
        to: Address,
        data: Bytes,
        value: U256,
        gas: Option<u64>,
    ) -> Result<SafeTxResult> {
        // Verify ownership if not done
        if !self.verified {
            if !self.verify_ownership().await? {
                return Ok(SafeTxResult::failure(
                    "Failed to verify Safe ownership".to_string(),
                ));
            }
        }

        let gas = gas.unwrap_or(self.config.default_gas);

        info!(
            to = %to,
            data_len = data.len(),
            value = %value,
            gas = gas,
            "Safe execute called"
        );

        // TODO: Implement full execution flow
        Err(MoonbagError::Internal(
            "SafeExecutor.execute() not fully implemented - use Python executor".to_string(),
        ))
    }

    /// Execute a transaction with raw calldata
    pub async fn execute_raw(
        &mut self,
        to: &str,
        data: &[u8],
        value: u64,
        gas: Option<u64>,
    ) -> Result<SafeTxResult> {
        let to = Address::from_str(to)
            .map_err(|e| MoonbagError::InvalidConfig(format!("Invalid address: {}", e)))?;

        self.execute(to, Bytes::copy_from_slice(data), U256::from(value), gas)
            .await
    }
}
