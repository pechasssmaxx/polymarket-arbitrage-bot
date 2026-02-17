//! Contract addresses for Polygon mainnet

use alloy::primitives::Address;
use std::str::FromStr;

/// NegRiskAdapter contract address
pub const NEG_RISK_ADAPTER: &str = "0xd91E80cF2E7be2e162c6513ceD06f1dD0dA35296";

/// CTF (Conditional Token Framework) ERC1155 address
pub const CTF_ADDRESS: &str = "0x4D97DCd97eC945f40cF65F87097ACe5EA0476045";

/// NegRiskCTFExchange address
pub const NEG_RISK_CTF_EXCHANGE: &str = "0xC5d563A36AE78145C45a50134d48A1215220f80a";

/// Wrapped Collateral (USDC wrapper) address
pub const WRAPPED_COLLATERAL: &str = "0x3A3BD7bb9528E159577F7C2e685CC81A765002E2";

/// USDC address on Polygon
pub const USDC_ADDRESS: &str = "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174";

/// Get NegRiskAdapter as Address
pub fn neg_risk_adapter_address() -> Address {
    Address::from_str(NEG_RISK_ADAPTER).expect("Invalid NEG_RISK_ADAPTER address")
}

/// Get CTF as Address
pub fn ctf_address() -> Address {
    Address::from_str(CTF_ADDRESS).expect("Invalid CTF_ADDRESS")
}

/// Get NegRiskCTFExchange as Address
pub fn ctf_exchange_address() -> Address {
    Address::from_str(NEG_RISK_CTF_EXCHANGE).expect("Invalid NEG_RISK_CTF_EXCHANGE")
}
