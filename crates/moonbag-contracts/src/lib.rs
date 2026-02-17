//! Moonbag Contracts - On-chain contract interactions
//!
//! Provides:
//! - Gnosis Safe v1.3.0 transaction execution
//! - NegRiskAdapter convertPositions
//! - ERC1155 balance checking
//!
//! ## Usage
//!
//! ```ignore
//! use moonbag_contracts::{NegRiskAdapterClient, SafeExecutor};
//!
//! let client = NegRiskAdapterClient::new(
//!     "https://your-polygon-rpc-url",
//!     "your-private-key",
//!     "0xYourSafeAddress",
//! ).await?;
//!
//! // Convert positions
//! let result = client.convert_positions(
//!     "0xMarketId...",
//!     0b11,  // K=2, outcomes 0 and 1
//!     1000000,  // 1.0 USDC worth (6 decimals)
//!     None,  // Default gas
//! ).await?;
//! ```

pub mod addresses;
pub mod erc1155;
pub mod neg_risk_adapter;
pub mod safe_executor;

pub use addresses::*;
pub use erc1155::Erc1155Client;
pub use neg_risk_adapter::{ConvertResult, NegRiskAdapterClient};
pub use safe_executor::{SafeExecutor, SafeExecutorConfig, SafeTxResult};
