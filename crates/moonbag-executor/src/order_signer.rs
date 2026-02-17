//! EIP-712 Order Signing for Polymarket CLOB
//!
//! Implements order signing compatible with Polymarket's CTF Exchange contracts.
//! Uses EIP-712 typed data signing.
//!
//! Based on:
//! - https://github.com/Polymarket/py-clob-client
//! - https://github.com/Polymarket/clob-client

use alloy::primitives::{Address, U256};
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::Signer;
use alloy::sol;
use alloy::sol_types::SolStruct;
use moonbag_core::{MoonbagError, Result};
use serde::Serialize;

// ─────────────────────────────────────────────────────────────────────────────
// Constants
// ─────────────────────────────────────────────────────────────────────────────

/// EIP-712 domain name
pub const DOMAIN_NAME: &str = "Polymarket CTF Exchange";

/// EIP-712 domain version
pub const DOMAIN_VERSION: &str = "1";

/// Polygon mainnet chain ID
pub const CHAIN_ID: u64 = 137;

/// CTF Exchange contract address (regular markets)
pub const CTF_EXCHANGE: &str = "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E";

/// NegRisk CTF Exchange contract address (NegRisk markets - our case)
pub const NEG_RISK_CTF_EXCHANGE: &str = "0xC5d563A36AE78145C45a50134d48A1215220f80a";

/// Signature type: EOA (standard wallet)
pub const SIG_TYPE_EOA: u8 = 0;

/// Signature type: Polymarket Proxy (when maker == signer)
pub const SIG_TYPE_POLY_PROXY: u8 = 1;

/// Signature type: Polymarket Gnosis Safe (when maker != signer)
pub const SIG_TYPE_POLY_GNOSIS_SAFE: u8 = 2;

/// Determine correct signature type based on maker/signer relationship
pub fn determine_signature_type(maker: Address, signer: Address) -> u8 {
    if maker == signer {
        // Using Polymarket Proxy (EOA with proxy)
        SIG_TYPE_POLY_PROXY
    } else {
        // Using Gnosis Safe (Safe with EOA owner)
        SIG_TYPE_POLY_GNOSIS_SAFE
    }
}

/// Order side: BUY
pub const SIDE_BUY: u8 = 0;

/// Order side: SELL
pub const SIDE_SELL: u8 = 1;

// ─────────────────────────────────────────────────────────────────────────────
// EIP-712 Types via sol! macro
// ─────────────────────────────────────────────────────────────────────────────

sol! {
    /// EIP-712 Order struct matching Polymarket's CTF Exchange
    #[derive(Debug)]
    struct Order {
        /// Random salt for replay protection
        uint256 salt;
        /// Address of the maker (Safe/funder)
        address maker;
        /// Address of the signer (EOA)
        address signer;
        /// Address of the taker (0x0 for public orders)
        address taker;
        /// CTF ERC1155 token ID
        uint256 tokenId;
        /// Amount being paid by maker (USDC for BUY, tokens for SELL)
        uint256 makerAmount;
        /// Amount being received by maker (tokens for BUY, USDC for SELL)
        uint256 takerAmount;
        /// Order expiration timestamp (0 = no expiration)
        uint256 expiration;
        /// Nonce for on-chain cancellation
        uint256 nonce;
        /// Fee rate in basis points (usually 0 for takers)
        uint256 feeRateBps;
        /// Order side (0 = BUY, 1 = SELL)
        uint8 side;
        /// Signature type (0 = EOA, 1 = POLY_PROXY, 2 = POLY_GNOSIS_SAFE)
        uint8 signatureType;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Signed Order Payload (for API)
// ─────────────────────────────────────────────────────────────────────────────

/// Order payload for CLOB API submission (includes signature)
///
/// CRITICAL: The CLOB API expects most numeric fields as JSON STRINGS, not numbers!
/// Only salt and signatureType are sent as numbers.
/// The side field is "BUY" or "SELL" as a string.
#[derive(Debug, Clone, Serialize)]
pub struct OrderPayload {
    /// Salt - small number (fits in u64), serialized as JSON number
    pub salt: u64,
    pub maker: String,
    pub signer: String,
    pub taker: String,
    /// Token ID - must be STRING in JSON (API requirement)
    #[serde(rename = "tokenId")]
    pub token_id: String,
    /// Maker amount - must be STRING in JSON
    #[serde(rename = "makerAmount")]
    pub maker_amount: String,
    /// Taker amount - must be STRING in JSON
    #[serde(rename = "takerAmount")]
    pub taker_amount: String,
    /// Expiration timestamp (0 = no expiration) - must be STRING
    pub expiration: String,
    /// Nonce for cancellation - must be STRING
    pub nonce: String,
    /// Fee rate in basis points - must be STRING
    #[serde(rename = "feeRateBps")]
    pub fee_rate_bps: String,
    /// Order side: "BUY" or "SELL" as string
    pub side: String,
    #[serde(rename = "signatureType")]
    pub signature_type: u8,
    /// EIP-712 signature (hex with 0x prefix) - must be inside order object per API spec
    pub signature: String,
}

impl OrderPayload {
    /// Create OrderPayload from Order and signature
    pub fn from_order_with_signature(order: &Order, signature: String) -> Self {
        Self {
            salt: order.salt.to::<u64>(), // Salt is a number
            maker: order.maker.to_string(),
            signer: order.signer.to_string(),
            taker: order.taker.to_string(),
            token_id: order.tokenId.to_string(),       // String
            maker_amount: order.makerAmount.to_string(), // String
            taker_amount: order.takerAmount.to_string(), // String
            expiration: order.expiration.to_string(),    // String
            nonce: order.nonce.to_string(),              // String
            fee_rate_bps: order.feeRateBps.to_string(),  // String
            side: if order.side == SIDE_BUY { "BUY".to_string() } else { "SELL".to_string() },
            signature_type: order.signatureType,
            signature,
        }
    }

    /// Serialize to JSON string (no special handling needed now)
    pub fn to_json_string(&self) -> std::result::Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

/// Complete signed order request for CLOB API
/// Structure: { "order": { ...fields + signature... }, "owner": "<api_key>", "orderType": "FOK" }
#[derive(Debug, Clone, Serialize)]
pub struct SignedOrderRequest {
    /// Order data (includes signature inside)
    pub order: OrderPayload,
    /// API key that owns this order (required by CLOB API)
    pub owner: String,
    /// Order type: "FOK", "GTC", "GTD"
    #[serde(rename = "orderType")]
    pub order_type: String,
}

impl SignedOrderRequest {
    /// Serialize to JSON string (no special handling needed now)
    pub fn to_json_string(&self) -> std::result::Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Order Signer
// ─────────────────────────────────────────────────────────────────────────────

/// EIP-712 order signer
pub struct OrderSigner {
    /// Local signer (from private key)
    signer: PrivateKeySigner,
    /// Safe/funder address (maker)
    maker: Address,
    /// EOA address (signer)
    signer_address: Address,
    /// Use NegRisk exchange (vs regular CTF exchange)
    use_neg_risk: bool,
}

impl OrderSigner {
    /// Create a new order signer
    ///
    /// # Arguments
    /// * `private_key` - Hex private key (with or without 0x prefix)
    /// * `maker` - Safe/funder address
    /// * `use_neg_risk` - Use NegRisk CTF Exchange (true for our markets)
    pub fn new(private_key: &str, maker: Address, use_neg_risk: bool) -> Result<Self> {
        // Parse private key (remove 0x prefix if present)
        let pk_hex = if private_key.starts_with("0x") {
            &private_key[2..]
        } else {
            private_key
        };

        // Validate private key format
        if pk_hex.len() != 64 || !pk_hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(MoonbagError::InvalidConfig(
                "Invalid private key format (expected 64 hex chars)".to_string(),
            ));
        }

        // Create signer
        let signer: PrivateKeySigner = pk_hex.parse().map_err(|e| {
            MoonbagError::InvalidConfig(format!("Failed to parse private key: {}", e))
        })?;

        let signer_address = signer.address();

        Ok(Self {
            signer,
            maker,
            signer_address,
            use_neg_risk,
        })
    }

    /// Get the EOA signer address
    pub fn signer_address(&self) -> Address {
        self.signer_address
    }

    /// Get the maker (Safe) address
    pub fn maker_address(&self) -> Address {
        self.maker
    }

    /// Sign an order using EIP-712
    ///
    /// Returns the order payload with signature included
    pub async fn sign_order(&self, order: &Order) -> Result<OrderPayload> {
        // Build EIP-712 domain
        let verifying_contract: Address = if self.use_neg_risk {
            NEG_RISK_CTF_EXCHANGE.parse().unwrap()
        } else {
            CTF_EXCHANGE.parse().unwrap()
        };

        let domain = alloy::sol_types::eip712_domain! {
            name: DOMAIN_NAME,
            version: DOMAIN_VERSION,
            chain_id: CHAIN_ID,
            verifying_contract: verifying_contract,
        };

        // Compute signing hash (domain + order struct)
        let signing_hash = order.eip712_signing_hash(&domain);

        // Sign the hash
        let signature = self
            .signer
            .sign_hash(&signing_hash)
            .await
            .map_err(|e| MoonbagError::SigningError(format!("Failed to sign order: {}", e)))?;

        // Convert signature to hex
        let sig_bytes = signature.as_bytes();
        let sig_hex = format!("0x{}", hex::encode(sig_bytes));

        // Build payload with signature included
        let payload = OrderPayload::from_order_with_signature(order, sig_hex);

        Ok(payload)
    }

    /// Create and sign a complete order
    ///
    /// Convenience method that builds the Order struct and signs it.
    pub async fn create_signed_order(
        &self,
        token_id: U256,
        maker_amount: U256,
        taker_amount: U256,
        side: u8,
        owner: String,
        order_type: &str,
    ) -> Result<SignedOrderRequest> {
        let order = Order {
            salt: generate_salt(),
            maker: self.maker,
            signer: self.signer_address,
            taker: Address::ZERO,
            tokenId: token_id,
            makerAmount: maker_amount,
            takerAmount: taker_amount,
            expiration: U256::ZERO, // No expiration
            nonce: U256::ZERO,
            feeRateBps: U256::ZERO,
            side,
            signatureType: determine_signature_type(self.maker, self.signer_address),
        };

        let payload = self.sign_order(&order).await?;

        Ok(SignedOrderRequest {
            order: payload,
            owner,
            order_type: order_type.to_string(),
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Generate a random salt for order uniqueness
///
/// Must produce small numbers that fit in JSON number type.
/// Matches Python py-clob-client: `round(timestamp * random())`
pub fn generate_salt() -> U256 {
    use rand::Rng;
    use std::time::{SystemTime, UNIX_EPOCH};

    // Use seconds (not nanos!) like Python does
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // Multiply by random [0, 1) like Python: round(now * random())
    let random: f64 = rand::thread_rng().gen();
    let salt = (timestamp as f64 * random) as u64;

    U256::from(salt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_salt() {
        let salt1 = generate_salt();
        let salt2 = generate_salt();

        // Salts should be non-zero
        assert!(salt1 > U256::ZERO);
        assert!(salt2 > U256::ZERO);

        // Salts should be different (with high probability due to timestamp)
        // Note: In rapid succession they might be the same, so this is a soft check
    }

    #[test]
    fn test_order_payload_from_order() {
        let order = Order {
            salt: U256::from(12345u64),
            maker: "0x1234567890123456789012345678901234567890"
                .parse()
                .unwrap(),
            signer: "0xabcdefabcdefabcdefabcdefabcdefabcdefabcd"
                .parse()
                .unwrap(),
            taker: Address::ZERO,
            tokenId: U256::from(999u64),
            makerAmount: U256::from(1000000u64),
            takerAmount: U256::from(2000000u64),
            expiration: U256::ZERO,
            nonce: U256::ZERO,
            feeRateBps: U256::ZERO,
            side: SIDE_BUY,
            signatureType: SIG_TYPE_POLY_GNOSIS_SAFE,
        };

        let payload = OrderPayload::from_order_with_signature(&order, "0xtest_sig".to_string());

        assert_eq!(payload.salt, 12345);
        assert_eq!(payload.side, "BUY");
        assert_eq!(payload.signature_type, 2);
        assert_eq!(payload.signature, "0xtest_sig");
    }

    #[tokio::test]
    async fn test_order_signer_creation() {
        // Test with valid private key (32 bytes hex)
        let pk = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let maker: Address = "0x1234567890123456789012345678901234567890"
            .parse()
            .unwrap();

        let signer = OrderSigner::new(pk, maker, true);
        assert!(signer.is_ok());

        let signer = signer.unwrap();
        assert_eq!(signer.maker_address(), maker);
    }

    #[tokio::test]
    async fn test_order_signing() {
        // Test private key (NOT for production!)
        let pk = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let maker: Address = "0x1234567890123456789012345678901234567890"
            .parse()
            .unwrap();

        let signer = OrderSigner::new(pk, maker, true).unwrap();

        let order = Order {
            salt: U256::from(12345u64),
            maker,
            signer: signer.signer_address(),
            taker: Address::ZERO,
            tokenId: U256::from(999u64),
            makerAmount: U256::from(1000000u64),
            takerAmount: U256::from(2000000u64),
            expiration: U256::ZERO,
            nonce: U256::ZERO,
            feeRateBps: U256::ZERO,
            side: SIDE_BUY,
            signatureType: SIG_TYPE_POLY_GNOSIS_SAFE,
        };

        let result = signer.sign_order(&order).await;
        assert!(result.is_ok());

        let payload = result.unwrap();

        // Signature should be hex with 0x prefix (inside payload now)
        assert!(payload.signature.starts_with("0x"));
        assert_eq!(payload.signature.len(), 132); // 0x + 65 bytes * 2

        // Payload should match order
        assert_eq!(payload.salt, 12345);
        assert_eq!(payload.side, "BUY");
    }

    #[test]
    fn test_signed_order_request_shape_matches_clob_api() {
        let payload = OrderPayload {
            salt: 1,
            maker: "0x1234567890123456789012345678901234567890".to_string(),
            signer: "0xabcdefabcdefabcdefabcdefabcdefabcdefabcd".to_string(),
            taker: "0x0000000000000000000000000000000000000000".to_string(),
            token_id: "999".to_string(),
            maker_amount: "1000000".to_string(),
            taker_amount: "2000000".to_string(),
            expiration: "0".to_string(),
            nonce: "0".to_string(),
            fee_rate_bps: "0".to_string(),
            side: "BUY".to_string(),
            signature_type: SIG_TYPE_POLY_GNOSIS_SAFE,
            signature: "0xtest_sig".to_string(),
        };

        let req = SignedOrderRequest {
            order: payload,
            owner: "test-api-key".to_string(),
            order_type: "FOK".to_string(),
        };

        // Test the to_json_string method - API expects strings for most fields
        let json_str = req.to_json_string().unwrap();

        // Verify string fields are serialized correctly
        assert!(json_str.contains("\"tokenId\":\"999\""), "tokenId must be string: {}", json_str);
        assert!(json_str.contains("\"makerAmount\":\"1000000\""), "makerAmount must be string: {}", json_str);
        assert!(json_str.contains("\"takerAmount\":\"2000000\""), "takerAmount must be string: {}", json_str);
        assert!(json_str.contains("\"expiration\":\"0\""), "expiration must be string: {}", json_str);
        assert!(json_str.contains("\"nonce\":\"0\""), "nonce must be string: {}", json_str);
        assert!(json_str.contains("\"feeRateBps\":\"0\""), "feeRateBps must be string: {}", json_str);
        assert!(json_str.contains("\"salt\":1"), "salt must be number: {}", json_str);
        assert!(json_str.contains("\"side\":\"BUY\""), "side must be string: {}", json_str);

        // Verify signature is in order object
        assert!(json_str.contains("\"signature\":\"0xtest_sig\""), "signature must be in order");
        assert!(json_str.contains("\"owner\":\"test-api-key\""), "owner must be present");
        assert!(json_str.contains("\"orderType\":\"FOK\""), "orderType must be present");
    }

    #[test]
    fn test_large_token_id_serialization() {
        // Test with a real Polymarket token ID (256-bit number as string)
        let large_token_id = "48331043336612883890938759509493159234755048973500640148014422747788308965732";

        let payload = OrderPayload {
            salt: 12345,
            maker: "0x1234567890123456789012345678901234567890".to_string(),
            signer: "0xabcdefabcdefabcdefabcdefabcdefabcdefabcd".to_string(),
            taker: "0x0000000000000000000000000000000000000000".to_string(),
            token_id: large_token_id.to_string(),
            maker_amount: "50000".to_string(),
            taker_amount: "5000000".to_string(),
            expiration: "0".to_string(),
            nonce: "0".to_string(),
            fee_rate_bps: "0".to_string(),
            side: "BUY".to_string(),
            signature_type: SIG_TYPE_POLY_GNOSIS_SAFE,
            signature: "0xtest_sig".to_string(),
        };

        let json_str = payload.to_json_string().unwrap();

        // The large token ID should be a STRING (API requires strings for numeric fields)
        assert!(
            json_str.contains(&format!("\"tokenId\":\"{}\"", large_token_id)),
            "Large tokenId must be serialized as string with quotes: {}",
            json_str
        );
    }

    /// Test EIP-712 hash matches Python py-clob-client
    ///
    /// Python test with same parameters produced:
    /// - struct_hash: 0x67cac2576133bf94554896f19a899da3c718d5f0351649b8ccaaa8a5c89f6d29
    /// - signature: 0xe28d90dcec7bfc36e0e973c5022a6a96120d2f51e26215c40b0ad0a0e8771bb773bf10ae0fcf64a849a72c3059ef5186b8326ab16fe7221ab20cdfc2fed5cfdc1b
    #[test]
    fn test_eip712_hash_matches_python() {
        // Fixed parameters matching Python test
        let salt = U256::from(123456789u64);
        let maker: Address = "0xE71A1e37ccd35A8F1FaEf095dB3eBd3F15ABc064".parse().unwrap();
        let signer: Address = "0x38bb0dcD802c7C82875d3B848F51084e2E459151".parse().unwrap();
        let taker: Address = Address::ZERO;
        let token_id: U256 = "71350214161024617954154195455943908791106743541648093809575805337027292856307".parse().unwrap();
        let maker_amount = U256::from(5000u64);
        let taker_amount = U256::from(5000000u64);
        let expiration = U256::ZERO;
        let nonce = U256::ZERO;
        let fee_rate_bps = U256::ZERO;
        let side: u8 = 0; // BUY
        let signature_type: u8 = 2; // POLY_GNOSIS_SAFE

        // Create order
        let order = Order {
            salt,
            maker,
            signer,
            taker,
            tokenId: token_id,
            makerAmount: maker_amount,
            takerAmount: taker_amount,
            expiration,
            nonce,
            feeRateBps: fee_rate_bps,
            side,
            signatureType: signature_type,
        };

        // NegRisk exchange (our case)
        let verifying_contract: Address = NEG_RISK_CTF_EXCHANGE.parse().unwrap();

        // Build domain
        let domain = alloy::sol_types::eip712_domain! {
            name: DOMAIN_NAME,
            version: DOMAIN_VERSION,
            chain_id: CHAIN_ID,
            verifying_contract: verifying_contract,
        };

        // Compute struct hash
        let struct_hash = order.eip712_hash_struct();
        let struct_hash_hex = format!("0x{}", hex::encode(struct_hash));

        // Compute signing hash (domain separator + struct hash)
        let signing_hash = order.eip712_signing_hash(&domain);
        let signing_hash_hex = format!("0x{}", hex::encode(signing_hash));

        // Python produces these hashes with the same parameters (verified with debug_python_eip712.py)
        let python_struct_hash = "0x3dfc3c6e535fee089756e8c575db4879dea94325adf21bd3843b677470228ed6";
        let python_signing_hash = "0x78164816ab444ce041b3d931ad5cf2fae7253dd3f87ec0eea1632557d90dd464";

        println!("=== EIP-712 Hash Comparison ===");
        println!("Rust struct hash:   {}", struct_hash_hex);
        println!("Python struct hash: {}", python_struct_hash);
        println!("Rust signing hash:  {}", signing_hash_hex);
        println!("Python signing hash:{}", python_signing_hash);
        println!("");
        println!("Order fields:");
        println!("  salt: {}", order.salt);
        println!("  maker: {}", order.maker);
        println!("  signer: {}", order.signer);
        println!("  taker: {}", order.taker);
        println!("  tokenId: {}", order.tokenId);
        println!("  makerAmount: {}", order.makerAmount);
        println!("  takerAmount: {}", order.takerAmount);
        println!("  expiration: {}", order.expiration);
        println!("  nonce: {}", order.nonce);
        println!("  feeRateBps: {}", order.feeRateBps);
        println!("  side: {}", order.side);
        println!("  signatureType: {}", order.signatureType);

        assert_eq!(
            struct_hash_hex, python_struct_hash,
            "Rust struct hash must match Python's struct hash"
        );

        assert_eq!(
            signing_hash_hex, python_signing_hash,
            "Rust signing hash must match Python's signing hash"
        );
    }
}
