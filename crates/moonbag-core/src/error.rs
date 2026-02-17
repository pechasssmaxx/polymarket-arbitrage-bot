//! Error types for the moonbag trading system

use rust_decimal::Decimal;
use thiserror::Error;

/// Main error type for moonbag operations
#[derive(Error, Debug)]
pub enum MoonbagError {
    // ─────────────────────────────────────────────────────────────
    // WebSocket Errors
    // ─────────────────────────────────────────────────────────────
    #[error("WebSocket connection failed: {0}")]
    WebSocketConnection(String),

    #[error("WebSocket subscription timeout after {waited_ms}ms")]
    SubscriptionTimeout { waited_ms: u64 },

    #[error("WebSocket message parse error: {0}")]
    MessageParse(String),

    // ─────────────────────────────────────────────────────────────
    // CLOB API Errors
    // ─────────────────────────────────────────────────────────────
    #[error("CLOB API error: {message} (status: {status:?})")]
    ClobApi {
        message: String,
        status: Option<u16>,
    },

    #[error("Order rejected: {reason} (order_id: {order_id:?})")]
    OrderRejected {
        reason: String,
        order_id: Option<String>,
    },

    #[error("FOK order not filled: available {available}, needed {needed}")]
    FokNotFilled { available: Decimal, needed: Decimal },

    #[error("Order timeout: order {order_id} not filled after {waited_ms}ms")]
    OrderTimeout { order_id: String, waited_ms: u64 },

    // ─────────────────────────────────────────────────────────────
    // Blockchain / RPC Errors
    // ─────────────────────────────────────────────────────────────
    #[error("RPC error: {0}")]
    Rpc(String),

    #[error("Transaction failed: {reason} (tx_hash: {tx_hash:?})")]
    TransactionFailed {
        reason: String,
        tx_hash: Option<String>,
    },

    #[error("Receipt timeout after {waited_secs}s (tx_hash: {tx_hash})")]
    ReceiptTimeout { tx_hash: String, waited_secs: u64 },

    // ─────────────────────────────────────────────────────────────
    // Safe Execution Errors
    // ─────────────────────────────────────────────────────────────
    #[error("Safe execution failed: {reason} (tx_hash: {tx_hash:?})")]
    SafeExecution {
        reason: String,
        tx_hash: Option<String>,
    },

    #[error("Safe ownership verification failed: EOA is not owner or threshold > 1")]
    SafeOwnershipInvalid,

    #[error("Safe inner call failed (ExecutionFailure event)")]
    SafeInnerCallFailed { tx_hash: String },

    // ─────────────────────────────────────────────────────────────
    // Settlement Errors
    // ─────────────────────────────────────────────────────────────
    #[error("Settlement timeout after {waited_secs}s: expected {expected} tokens")]
    SettlementTimeout { waited_secs: u64, expected: Decimal },

    #[error("Insufficient balance: have {have}, need {need}")]
    InsufficientBalance { have: Decimal, need: Decimal },

    // ─────────────────────────────────────────────────────────────
    // Verification / Pre-check Errors
    // ─────────────────────────────────────────────────────────────
    #[error("Pre-check failed: {reason}")]
    PreCheckFailed { reason: String },

    #[error("Opportunity too old: {age_secs:.1}s > {max_secs:.1}s")]
    OpportunityTooOld { age_secs: f64, max_secs: f64 },

    #[error("Profit degraded: ${current} < ${min} minimum")]
    ProfitDegraded { current: Decimal, min: Decimal },

    #[error("Liquidity insufficient: available {available}, needed {needed}")]
    LiquidityInsufficient { available: Decimal, needed: Decimal },

    // ─────────────────────────────────────────────────────────────
    // Configuration Errors
    // ─────────────────────────────────────────────────────────────
    #[error("Missing configuration: {0}")]
    MissingConfig(String),

    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),

    // ─────────────────────────────────────────────────────────────
    // Queue Errors
    // ─────────────────────────────────────────────────────────────
    #[error("Queue error: {0}")]
    Queue(String),

    #[error("Queue item not found: {id}")]
    QueueItemNotFound { id: String },

    #[error("Duplicate opportunity in queue: {market_id}")]
    DuplicateOpportunity { market_id: String },

    // ─────────────────────────────────────────────────────────────
    // Contract Errors
    // ─────────────────────────────────────────────────────────────
    #[error("Contract call failed: {function} on {contract}: {reason}")]
    ContractCall {
        contract: String,
        function: String,
        reason: String,
    },

    #[error("ABI encoding error: {0}")]
    AbiEncode(String),

    #[error("Signing error: {0}")]
    SigningError(String),

    // ─────────────────────────────────────────────────────────────
    // I/O Errors
    // ─────────────────────────────────────────────────────────────
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON parse error: {0}")]
    JsonParse(#[from] serde_json::Error),

    // ─────────────────────────────────────────────────────────────
    // Generic Errors
    // ─────────────────────────────────────────────────────────────
    #[error("Internal error: {0}")]
    Internal(String),

    #[error("{0}")]
    Other(String),
}

/// Result type alias for moonbag operations
pub type Result<T> = std::result::Result<T, MoonbagError>;

impl MoonbagError {
    /// Check if this error requires manual intervention
    pub fn needs_manual_intervention(&self) -> bool {
        matches!(
            self,
            Self::SafeInnerCallFailed { .. }
                | Self::SettlementTimeout { .. }
                | Self::InsufficientBalance { .. }
        )
    }

    /// Check if this error is retryable
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::WebSocketConnection(_)
                | Self::SubscriptionTimeout { .. }
                | Self::Rpc(_)
                | Self::ReceiptTimeout { .. }
                | Self::ClobApi { .. }
        )
    }

    /// Get the execution stage where error occurred (for tracing)
    pub fn stage(&self) -> &'static str {
        match self {
            Self::WebSocketConnection(_)
            | Self::SubscriptionTimeout { .. }
            | Self::MessageParse(_) => "websocket",

            Self::ClobApi { .. }
            | Self::OrderRejected { .. }
            | Self::FokNotFilled { .. }
            | Self::OrderTimeout { .. } => "order",

            Self::PreCheckFailed { .. }
            | Self::OpportunityTooOld { .. }
            | Self::ProfitDegraded { .. }
            | Self::LiquidityInsufficient { .. } => "precheck",

            Self::SettlementTimeout { .. } | Self::InsufficientBalance { .. } => "settlement",

            Self::SafeExecution { .. }
            | Self::SafeOwnershipInvalid
            | Self::SafeInnerCallFailed { .. } => "safe",

            Self::ContractCall { .. } | Self::AbiEncode(_) => "contract",

            Self::Rpc(_) | Self::TransactionFailed { .. } | Self::ReceiptTimeout { .. } => "rpc",

            Self::Queue(_) | Self::QueueItemNotFound { .. } | Self::DuplicateOpportunity { .. } => {
                "queue"
            }

            Self::MissingConfig(_) | Self::InvalidConfig(_) => "config",

            Self::Io(_)
            | Self::JsonParse(_)
            | Self::Internal(_)
            | Self::Other(_)
            | Self::SigningError(_) => "internal",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = MoonbagError::FokNotFilled {
            available: Decimal::new(50, 0),
            needed: Decimal::new(100, 0),
        };
        assert!(err.to_string().contains("50"));
        assert!(err.to_string().contains("100"));
    }

    #[test]
    fn test_needs_manual_intervention() {
        let err = MoonbagError::SafeInnerCallFailed {
            tx_hash: "0x123".to_string(),
        };
        assert!(err.needs_manual_intervention());

        let err = MoonbagError::WebSocketConnection("test".to_string());
        assert!(!err.needs_manual_intervention());
    }

    #[test]
    fn test_is_retryable() {
        let err = MoonbagError::Rpc("timeout".to_string());
        assert!(err.is_retryable());

        let err = MoonbagError::SafeOwnershipInvalid;
        assert!(!err.is_retryable());
    }
}
