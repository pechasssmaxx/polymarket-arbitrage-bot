//! NO token buying (GTC orders)
//!
//! Places Good-Till-Cancelled orders to buy NO tokens on the CLOB.
//! Orders stay on the book until filled or manually cancelled.
//!
//! Uses EIP-712 signed orders for the Polymarket CLOB API.
//!
//! ## Order Size Requirements
//!
//! Polymarket requires marketable orders to meet minimum size:
//! - **$1 minimum** OR **5 shares minimum**
//!
//! The analyzer pre-calculates `effective_amount` and verifies liquidity.
//! This module uses that amount directly (with defensive re-verification).

use crate::clob_auth::{ClobCreds, L2Headers};
use crate::order_builder::OrderBuilder;
use crate::order_signer::{OrderSigner, SignedOrderRequest};
use alloy::primitives::Address;
use moonbag_core::models::{calculate_min_order_size, MoonbagOpportunity, SelectedOutcome, MIN_SHARES};
use std::str::FromStr;
use moonbag_core::{Config, MoonbagError, Result};
use rust_decimal::Decimal;
use serde::Deserialize;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Buy executor configuration
#[derive(Debug, Clone)]
pub struct BuyConfig {
    /// Slippage tolerance (e.g., 0.02 = 2%)
    pub slippage: Decimal,
    /// Order timeout in seconds
    pub timeout_secs: u64,
    /// REST API timeout
    pub api_timeout: Duration,
    /// CLOB host URL
    pub clob_host: String,
    /// Dry run mode
    pub dry_run: bool,
    /// GTC poll timeout (how long to wait for "live" orders to fill)
    pub gtc_poll_timeout: Duration,
    /// GTC poll interval
    pub gtc_poll_interval: Duration,
}

impl Default for BuyConfig {
    fn default() -> Self {
        Self {
            slippage: Decimal::new(2, 2), // 2%
            timeout_secs: 30,
            api_timeout: Duration::from_secs(15),
            clob_host: "https://clob.polymarket.com".to_string(),
            dry_run: true,
            gtc_poll_timeout: Duration::from_secs(10), // Wait 10s for live orders to fill
            gtc_poll_interval: Duration::from_millis(500), // Poll every 500ms
        }
    }
}

impl From<&Config> for BuyConfig {
    fn from(config: &Config) -> Self {
        Self {
            slippage: config.slippage_tolerance,
            timeout_secs: 30,
            api_timeout: Duration::from_secs(15),
            clob_host: config.clob_host().to_string(),
            dry_run: config.dry_run,
            gtc_poll_timeout: Duration::from_secs(10), // Wait 10s for live orders to fill
            gtc_poll_interval: Duration::from_millis(500), // Poll every 500ms
        }
    }
}

/// Result of a buy operation for a single outcome
#[derive(Debug, Clone)]
pub struct BuyResult {
    pub success: bool,
    pub token_id: String,
    pub order_id: Option<String>,
    pub filled_amount: Decimal,
    pub avg_price: Decimal,
    pub error: Option<String>,
}

impl BuyResult {
    pub fn success(
        token_id: String,
        order_id: String,
        filled_amount: Decimal,
        avg_price: Decimal,
    ) -> Self {
        Self {
            success: true,
            token_id,
            order_id: Some(order_id),
            filled_amount,
            avg_price,
            error: None,
        }
    }

    pub fn failure(token_id: String, error: impl Into<String>) -> Self {
        Self {
            success: false,
            token_id,
            order_id: None,
            filled_amount: Decimal::ZERO,
            avg_price: Decimal::ZERO,
            error: Some(error.into()),
        }
    }
}

/// Aggregate result of buying all NO tokens
#[derive(Debug, Clone)]
pub struct BuyAllResult {
    pub success: bool,
    pub results: Vec<BuyResult>,
    pub total_filled: Decimal,
    pub total_cost: Decimal,
    pub partial_k: bool,
    pub error: Option<String>,
    /// Amount per token actually used (may differ from requested if adjusted for minimums)
    pub effective_amount: Decimal,
    /// Whether amount was adjusted due to minimum requirements
    pub amount_adjusted: bool,
    /// Minimum filled amount across all successful outcomes (for CONVERT)
    pub min_filled: Decimal,
    /// Maximum filled amount across all successful outcomes
    pub max_filled: Decimal,
    /// Excess shares that can't be converted (max_filled - min_filled per outcome)
    pub excess_shares: Decimal,
}

impl BuyAllResult {
    pub fn new() -> Self {
        Self {
            success: false,
            results: Vec::new(),
            total_filled: Decimal::ZERO,
            total_cost: Decimal::ZERO,
            partial_k: false,
            error: None,
            effective_amount: Decimal::ZERO,
            amount_adjusted: false,
            min_filled: Decimal::MAX,
            max_filled: Decimal::ZERO,
            excess_shares: Decimal::ZERO,
        }
    }

    pub fn add_result(&mut self, result: BuyResult) {
        if result.success {
            self.total_filled += result.filled_amount;
            self.total_cost += result.filled_amount * result.avg_price;
            // Track min/max for CONVERT amount calculation
            if result.filled_amount < self.min_filled {
                self.min_filled = result.filled_amount;
            }
            if result.filled_amount > self.max_filled {
                self.max_filled = result.filled_amount;
            }
        }
        self.results.push(result);
    }

    pub fn finalize(&mut self, expected_k: u8) {
        let successful = self.results.iter().filter(|r| r.success).count();
        self.success = successful == expected_k as usize;
        self.partial_k = successful > 0 && successful < expected_k as usize;

        if !self.success && !self.partial_k {
            self.error = Some("No orders filled".to_string());
            self.min_filled = Decimal::ZERO;
        } else if self.partial_k {
            self.error = Some(format!(
                "Partial fill: {}/{} outcomes",
                successful, expected_k
            ));
        }

        // Calculate excess shares (total that can't be converted)
        // Each outcome has (filled - min_filled) excess shares
        if self.min_filled < Decimal::MAX && self.min_filled > Decimal::ZERO {
            self.excess_shares = self
                .results
                .iter()
                .filter(|r| r.success)
                .map(|r| r.filled_amount - self.min_filled)
                .sum();

            // Log warning if fills are uneven
            if self.max_filled > self.min_filled {
                tracing::warn!(
                    min_filled = %self.min_filled,
                    max_filled = %self.max_filled,
                    excess_shares = %self.excess_shares,
                    "Uneven fills detected - CONVERT will use min_filled, excess shares will remain"
                );
            }
        }
    }

    /// Get the amount that can be safely converted (minimum across all outcomes)
    pub fn convertible_amount(&self) -> Decimal {
        if self.min_filled == Decimal::MAX {
            Decimal::ZERO
        } else {
            self.min_filled
        }
    }
}

impl Default for BuyAllResult {
    fn default() -> Self {
        Self::new()
    }
}

/// CLOB order response (POST /order)
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct OrderResponse {
    #[serde(rename = "orderID")]
    order_id: Option<String>,
    success: Option<bool>,
    #[serde(rename = "errorMsg")]
    error_msg: Option<String>,
    status: Option<String>,
    /// Transaction hashes - present only if order was actually filled
    #[serde(rename = "transactionsHashes", default)]
    transactions_hashes: Vec<String>,
    /// Actual taker amount filled
    #[serde(rename = "takingAmount")]
    taking_amount: Option<String>,
    /// Actual maker amount filled
    #[serde(rename = "makingAmount")]
    making_amount: Option<String>,
}

/// Order status response wrapper (GET /data/order/{order_hash})
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct GetOrderResponse {
    /// The OpenOrder object (may be null if order doesn't exist)
    order: Option<OpenOrder>,
}

/// OpenOrder from GET /data/order response
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct OpenOrder {
    /// Order ID (hash)
    id: Option<String>,
    /// Order status: LIVE, MATCHED, CANCELED, etc.
    status: Option<String>,
    /// Associated trade IDs
    #[serde(rename = "associate_trades", default)]
    associate_trades: Vec<String>,
    /// Size matched (filled amount)
    #[serde(rename = "size_matched")]
    size_matched: Option<String>,
    /// Original size
    #[serde(rename = "original_size")]
    original_size: Option<String>,
    /// Price
    price: Option<String>,
    /// Market (condition ID)
    market: Option<String>,
    /// Token ID (asset_id)
    asset_id: Option<String>,
}

/// Buy executor for NO tokens
pub struct BuyExecutor {
    config: BuyConfig,
    client: reqwest::Client,
    /// CLOB credentials (for authenticated endpoints)
    creds: Option<ClobCreds>,
    /// Safe (funder) address
    funder: String,
    /// EIP-712 order signer
    order_signer: Option<OrderSigner>,
    /// Order builder
    order_builder: Option<OrderBuilder>,
}

impl BuyExecutor {
    /// Create a new buy executor
    pub fn new(
        config: BuyConfig,
        funder: String,
        creds: Option<ClobCreds>,
        order_signer: Option<OrderSigner>,
        order_builder: Option<OrderBuilder>,
    ) -> Self {
        let client = reqwest::Client::builder()
            .timeout(config.api_timeout)
            .build()
            .expect("Failed to create HTTP client");

        Self {
            config,
            client,
            creds,
            funder,
            order_signer,
            order_builder,
        }
    }

    /// Create from global config
    pub fn from_config(config: &Config) -> Self {
        let creds = match (
            &config.polymarket_api_key,
            &config.polymarket_api_secret,
            &config.polymarket_passphrase,
        ) {
            (Some(key), Some(secret), Some(pass))
                if !key.is_empty() && !secret.is_empty() && !pass.is_empty() =>
            {
                Some(ClobCreds::new(key.clone(), secret.clone(), pass.clone()))
            }
            _ => None,
        };

        // Parse funder address
        let funder_addr: Address = config
            .polymarket_funder
            .parse()
            .expect("Invalid POLYMARKET_FUNDER address");

        // Create order signer from private key
        let (order_signer, order_builder) = if !config.polygon_private_key.is_empty() {
            match OrderSigner::new(&config.polygon_private_key, funder_addr, true) {
                Ok(signer) => {
                    let signer_addr = signer.signer_address();
                    let builder = OrderBuilder::new(funder_addr, signer_addr);
                    info!(
                        maker = %funder_addr,
                        signer = %signer_addr,
                        "EIP-712 order signing enabled"
                    );
                    (Some(signer), Some(builder))
                }
                Err(e) => {
                    warn!(error = %e, "Failed to create order signer, EIP-712 signing disabled");
                    (None, None)
                }
            }
        } else {
            warn!("No private key configured, EIP-712 signing disabled");
            (None, None)
        };

        Self::new(
            BuyConfig::from(config),
            config.polymarket_funder.clone(),
            creds,
            order_signer,
            order_builder,
        )
    }

    /// Buy all NO tokens for an opportunity
    pub async fn buy_all(&self, opp: &MoonbagOpportunity) -> Result<BuyAllResult> {
        let mut result = BuyAllResult::new();

        // Calculate minimum required size for each outcome based on price
        // Use impact price directly (what we'd actually pay from orderbook)
        let max_min_required = opp
            .selected_outcomes
            .iter()
            .map(|s| calculate_min_order_size(s.no_impact_price))
            .max()
            .unwrap_or(MIN_SHARES);

        // Effective amount is max of requested amount and minimum required
        let effective_amount = opp.amount.max(max_min_required);
        let amount_adjusted = effective_amount > opp.amount;

        result.effective_amount = effective_amount;
        result.amount_adjusted = amount_adjusted;

        if amount_adjusted {
            info!(
                k = opp.k,
                requested_amount = %opp.amount,
                effective_amount = %effective_amount,
                max_min_required = %max_min_required,
                market = %opp.market_slug,
                "Adjusted order size to meet Polymarket minimums ($1 OR 5 shares)"
            );
        }

        info!(
            k = opp.k,
            amount = %effective_amount,
            market = %opp.market_slug,
            "Starting NO token purchases"
        );

        // Buy each selected outcome with effective amount
        for selected in &opp.selected_outcomes {
            let buy_result = self.buy_one(selected, effective_amount).await;

            match buy_result {
                Ok(r) => {
                    info!(
                        token = %r.token_id,
                        filled = %r.filled_amount,
                        price = %r.avg_price,
                        "Buy completed"
                    );
                    result.add_result(r);
                }
                Err(e) => {
                    warn!(
                        token = %selected.no_token_id,
                        error = %e,
                        "Buy failed"
                    );
                    result.add_result(BuyResult::failure(
                        selected.no_token_id.clone(),
                        e.to_string(),
                    ));

                    // If partial K is not allowed, abort early
                    if !opp.allow_partial_k {
                        result.error = Some(format!("Buy failed and partial_k disabled: {}", e));
                        return Ok(result);
                    }
                }
            }
        }

        result.finalize(opp.k);
        Ok(result)
    }

    /// Buy NO tokens for a single outcome
    async fn buy_one(&self, selected: &SelectedOutcome, amount: Decimal) -> Result<BuyResult> {
        // Validate impact price (Polymarket requires 0 < price < 1)
        if selected.no_impact_price <= Decimal::ZERO {
            return Err(MoonbagError::OrderRejected {
                reason: format!(
                    "Invalid impact price {} (must be > 0)",
                    selected.no_impact_price
                ),
                order_id: None,
            });
        }

        if selected.no_impact_price >= Decimal::ONE {
            return Err(MoonbagError::OrderRejected {
                reason: format!(
                    "Impact price {} >= 1.0 (invalid for NO token)",
                    selected.no_impact_price
                ),
                order_id: None,
            });
        }

        // Use exact best ask price (no_price) - no buffer
        // Any buffer causes price improvement → paying more than necessary
        // If price moves, order may not fill and we retry with fresh prices
        let limit_price = selected.no_price;

        debug!(
            token = %selected.no_token_id,
            amount = %amount,
            vwap = %selected.no_vwap,
            impact = %selected.no_impact_price,
            limit = %limit_price,
            "Placing GTC buy order"
        );

        if self.config.dry_run {
            info!(
                token = %selected.no_token_id,
                "DRY RUN: Would place GTC buy order"
            );
            return Ok(BuyResult::success(
                selected.no_token_id.clone(),
                "dry-run-order-id".to_string(),
                amount,
                selected.no_vwap,
            ));
        }

        // Check if order signer is available
        let (order_signer, order_builder) = match (&self.order_signer, &self.order_builder) {
            (Some(signer), Some(builder)) => (signer, builder),
            _ => {
                return Err(MoonbagError::ClobApi {
                    message: "EIP-712 order signing not configured (missing private key)"
                        .to_string(),
                    status: None,
                });
            }
        };

        // Build the EIP-712 order
        let order = order_builder.build_buy_order(&selected.no_token_id, limit_price, amount)?;

        // Sign the order (signature included in payload)
        let payload = order_signer.sign_order(&order).await?;

        let owner = self
            .creds
            .as_ref()
            .map(|c| c.api_key.clone())
            .ok_or_else(|| MoonbagError::ClobApi {
                message: "CLOB credentials not configured".to_string(),
                status: None,
            })?;

        // Build signed order request
        // GTC (Good-Till-Cancelled) allows orders to stay on the book
        // and potentially get better fills than FOK which cancels immediately
        let signed_request = SignedOrderRequest {
            order: payload,
            owner,
            order_type: "GTC".to_string(),
        };

        // Log human-readable order details (amounts are in 6-decimal raw form)
        let scale = Decimal::from(1_000_000u64);
        let maker_human = Decimal::from(order.makerAmount.as_limbs()[0]) / scale;
        let taker_human = Decimal::from(order.takerAmount.as_limbs()[0]) / scale;
        info!(
            token = %selected.no_token_id,
            question = %selected.question,
            limit_price = %limit_price,
            orderbook_best_ask = %selected.no_price,
            requested_shares = %amount,
            usdc_to_pay = %maker_human,
            tokens_to_get = %taker_human,
            maker_raw = %order.makerAmount,
            taker_raw = %order.takerAmount,
            "Sending GTC order - REQUEST"
        );

        // Place order via API and capture raw response for debugging
        let response = self.place_signed_order_with_raw(&signed_request).await?;

        // Debug: log raw API response
        debug!(
            success = ?response.success,
            order_id = ?response.order_id,
            taking_amount = ?response.taking_amount,
            making_amount = ?response.making_amount,
            tx_hashes = ?response.transactions_hashes.len(),
            "Raw API order response"
        );

        // Check response - verify actual fill
        // success=true is not enough - check for tx hashes or taking_amount
        let has_fill = !response.transactions_hashes.is_empty()
            || response
                .taking_amount
                .as_ref()
                .and_then(|s| s.parse::<u64>().ok())
                .map(|v| v > 0)
                .unwrap_or(false);

        if response.success.unwrap_or(false) && has_fill {
            let order_id = response.order_id.unwrap_or_default();

            // Parse actual filled amount from response
            // API returns human-readable decimal strings like "31.2" (not raw/wei format)
            let actual_filled = response
                .taking_amount
                .as_ref()
                .and_then(|s| s.parse::<Decimal>().ok())
                .unwrap_or(amount); // fallback to requested if not in response

            let actual_cost = response
                .making_amount
                .as_ref()
                .and_then(|s| s.parse::<Decimal>().ok())
                .unwrap_or(amount * selected.no_vwap);

            let actual_price = if actual_filled > Decimal::ZERO {
                actual_cost / actual_filled
            } else {
                selected.no_vwap
            };

            // Log actual fill details with comparison to requested
            info!(
                order_id = %order_id,
                requested = %amount,
                actual_filled = %actual_filled,
                actual_cost = %actual_cost,
                actual_price = %actual_price,
                diff = %(actual_filled - amount),
                "GTC order filled"
            );

            // Check if this is a partial fill (< 90% of requested)
            let fill_ratio = actual_filled / amount;
            if fill_ratio < Decimal::from_str("0.9").unwrap() {
                // Partial fill - wait for more
                warn!(
                    token = %selected.no_token_id,
                    order_id = %order_id,
                    requested = %amount,
                    actual = %actual_filled,
                    fill_pct = %(fill_ratio * Decimal::from(100)),
                    "Partial fill (<90%) - waiting for more fills..."
                );

                // Poll for additional fills
                match self
                    .wait_for_fill(&order_id, &selected.no_token_id, amount, selected.no_price)
                    .await
                {
                    Ok(Some(result)) => {
                        // Got more fills
                        info!(
                            token = %selected.no_token_id,
                            initial_fill = %actual_filled,
                            final_fill = %result.filled_amount,
                            "Order filled after waiting for partial"
                        );
                        return Ok(result);
                    }
                    Ok(None) => {
                        // Timeout - return what we have
                        warn!(
                            token = %selected.no_token_id,
                            order_id = %order_id,
                            filled = %actual_filled,
                            "Partial fill timeout - returning partial amount"
                        );
                    }
                    Err(e) => {
                        warn!(
                            token = %selected.no_token_id,
                            error = %e,
                            filled = %actual_filled,
                            "Error waiting for partial fill - returning partial amount"
                        );
                    }
                }
            }

            // Warn if fill differs significantly from requested
            let diff_pct = ((actual_filled - amount) / amount * Decimal::from(100)).abs();
            if diff_pct > Decimal::from(5) {
                warn!(
                    token = %selected.no_token_id,
                    requested = %amount,
                    actual = %actual_filled,
                    diff_pct = %diff_pct,
                    "Fill amount differs from requested by >5%"
                );
            }

            Ok(BuyResult::success(
                selected.no_token_id.clone(),
                order_id,
                actual_filled,  // Use ACTUAL filled amount, not requested!
                actual_price,
            ))
        } else if response.success.unwrap_or(false) && !has_fill {
            // API said success but no actual fill - order pending on book (status: "live")
            // Wait for it to fill (poll order status for configured timeout)
            let order_id = response.order_id.clone().unwrap_or_default();

            info!(
                token = %selected.no_token_id,
                order_id = %order_id,
                status = ?response.status,
                "GTC order placed but no immediate fill - waiting for fill..."
            );

            // Poll for fill
            match self
                .wait_for_fill(&order_id, &selected.no_token_id, amount, selected.no_price)
                .await
            {
                Ok(Some(result)) => {
                    // Order filled after waiting
                    Ok(result)
                }
                Ok(None) => {
                    // Order didn't fill within timeout
                    warn!(
                        token = %selected.no_token_id,
                        order_id = %order_id,
                        "GTC order did not fill within timeout"
                    );
                    Err(MoonbagError::OrderRejected {
                        reason: "GTC order pending - did not fill within timeout".to_string(),
                        order_id: response.order_id,
                    })
                }
                Err(e) => {
                    // Error polling order status
                    warn!(
                        token = %selected.no_token_id,
                        order_id = %order_id,
                        error = %e,
                        "Error polling GTC order status"
                    );
                    Err(MoonbagError::OrderRejected {
                        reason: format!("GTC order poll error: {}", e),
                        order_id: response.order_id,
                    })
                }
            }
        } else {
            let error = response
                .error_msg
                .unwrap_or_else(|| "Unknown error".to_string());
            Err(MoonbagError::OrderRejected {
                reason: error,
                order_id: response.order_id,
            })
        }
    }

    /// Place a signed order via CLOB API (with raw response logging for debugging)
    async fn place_signed_order_with_raw(&self, signed_order: &SignedOrderRequest) -> Result<OrderResponse> {
        let url = format!("{}/order", self.config.clob_host);
        let path = "/order";
        let method = "POST";

        // Serialize body with proper number handling (U256 as raw JSON numbers)
        // This is critical - Polymarket API requires numeric fields as JSON numbers, not strings
        let body_str = signed_order.to_json_string().map_err(|e| MoonbagError::ClobApi {
            message: format!("Failed to serialize signed order: {}", e),
            status: None,
        })?;

        debug!(body = %body_str, "Sending signed order request");

        // Build request
        let mut req = self
            .client
            .post(&url)
            .body(body_str.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json");

        // Add L2 auth headers if credentials configured
        if let Some(creds) = &self.creds {
            // L2 auth must use the EOA signer address (not the Safe funder).
            let poly_address = self
                .order_signer
                .as_ref()
                .map(|s| s.signer_address().to_string())
                .unwrap_or_else(|| self.funder.clone());

            let headers = L2Headers::new(&poly_address, creds, method, path, Some(&body_str))
                .map_err(|e| MoonbagError::ClobApi {
                    message: format!("Auth failed: {}", e),
                    status: None,
                })?;
            req = headers.apply(req);
        } else {
            return Err(MoonbagError::ClobApi {
                message: "CLOB credentials not configured".to_string(),
                status: None,
            });
        }

        let response = req.send().await.map_err(|e| MoonbagError::ClobApi {
            message: format!("Order request failed: {}", e),
            status: None,
        })?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            warn!(
                status = %status,
                body = %body,
                "GTC order - HTTP ERROR RESPONSE"
            );
            return Err(MoonbagError::ClobApi {
                message: format!("Order error: {} - {}", status, body),
                status: Some(status.as_u16()),
            });
        }

        // Get raw response text first for logging
        let response_text = response.text().await.map_err(|e| MoonbagError::ClobApi {
            message: format!("Failed to read response body: {}", e),
            status: None,
        })?;

        // Log raw response for debugging GTC failures
        info!(
            response = %response_text,
            "GTC order - RAW RESPONSE"
        );

        // Parse response
        serde_json::from_str::<OrderResponse>(&response_text)
            .map_err(|e| MoonbagError::ClobApi {
                message: format!("Order response parse failed: {} (raw: {})", e, response_text),
                status: None,
            })
    }

    /// Get order status by order ID (GET /data/order/{order_hash})
    async fn get_order_status(&self, order_id: &str) -> Result<GetOrderResponse> {
        let url = format!("{}/data/order/{}", self.config.clob_host, order_id);
        let path = format!("/data/order/{}", order_id);
        let method = "GET";

        // Build request
        let mut req = self.client.get(&url);

        // Add L2 auth headers if credentials configured
        if let Some(creds) = &self.creds {
            let poly_address = self
                .order_signer
                .as_ref()
                .map(|s| s.signer_address().to_string())
                .unwrap_or_else(|| self.funder.clone());

            let headers = L2Headers::new(&poly_address, creds, method, &path, None)
                .map_err(|e| MoonbagError::ClobApi {
                    message: format!("Auth failed: {}", e),
                    status: None,
                })?;
            req = headers.apply(req);
        }

        let response = req.send().await.map_err(|e| MoonbagError::ClobApi {
            message: format!("Get order status failed: {}", e),
            status: None,
        })?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(MoonbagError::ClobApi {
                message: format!("Get order status error: {} - {}", status, body),
                status: Some(status.as_u16()),
            });
        }

        response
            .json::<GetOrderResponse>()
            .await
            .map_err(|e| MoonbagError::ClobApi {
                message: format!("Get order response parse failed: {}", e),
                status: None,
            })
    }

    /// Wait for a "live" order to fill (poll until filled or timeout)
    async fn wait_for_fill(
        &self,
        order_id: &str,
        token_id: &str,
        _amount: Decimal, // unused but kept for potential future use
        price: Decimal,
    ) -> Result<Option<BuyResult>> {
        let start = std::time::Instant::now();

        info!(
            order_id = %order_id,
            token_id = %token_id,
            timeout_secs = ?self.config.gtc_poll_timeout.as_secs(),
            "Waiting for GTC order to fill..."
        );

        while start.elapsed() < self.config.gtc_poll_timeout {
            tokio::time::sleep(self.config.gtc_poll_interval).await;

            match self.get_order_status(order_id).await {
                Ok(resp) => {
                    // Unwrap the nested order object
                    let Some(order) = resp.order else {
                        debug!(
                            order_id = %order_id,
                            "Order not found in response"
                        );
                        continue;
                    };

                    let status = order.status.as_deref().unwrap_or("UNKNOWN");

                    // Check if order is filled (MATCHED) or partially filled
                    let size_matched = order
                        .size_matched
                        .as_ref()
                        .and_then(|s| s.parse::<Decimal>().ok())
                        .unwrap_or(Decimal::ZERO);

                    debug!(
                        order_id = %order_id,
                        status = %status,
                        size_matched = %size_matched,
                        elapsed_ms = ?start.elapsed().as_millis(),
                        "Polled order status"
                    );

                    if status == "MATCHED" || size_matched > Decimal::ZERO {
                        // Order filled!
                        let actual_filled = size_matched;
                        let actual_price = order
                            .price
                            .as_ref()
                            .and_then(|s| s.parse::<Decimal>().ok())
                            .unwrap_or(price);

                        info!(
                            order_id = %order_id,
                            actual_filled = %actual_filled,
                            actual_price = %actual_price,
                            elapsed_ms = ?start.elapsed().as_millis(),
                            "GTC order filled after waiting"
                        );

                        return Ok(Some(BuyResult::success(
                            token_id.to_string(),
                            order_id.to_string(),
                            actual_filled,
                            actual_price,
                        )));
                    }

                    if status == "CANCELED" {
                        info!(
                            order_id = %order_id,
                            "GTC order was canceled"
                        );
                        return Ok(None);
                    }
                }
                Err(e) => {
                    // Log error but continue polling
                    debug!(
                        order_id = %order_id,
                        error = %e,
                        "Error polling order status"
                    );
                }
            }
        }

        // Timeout - order still not filled
        info!(
            order_id = %order_id,
            timeout_secs = ?self.config.gtc_poll_timeout.as_secs(),
            "GTC order did not fill within timeout"
        );

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moonbag_core::models::calculate_min_order_size;
    use rust_decimal_macros::dec;

    #[test]
    fn test_calculate_min_order_size_high_price() {
        // Price $0.50 - need ceil(1/0.50) = 2 shares for $1
        let min_size = calculate_min_order_size(dec!(0.50));
        assert_eq!(min_size, dec!(2)); // 2 shares × $0.50 = $1
    }

    #[test]
    fn test_calculate_min_order_size_medium_price() {
        // Price $0.20 - need ceil(1/0.20) = 5 shares for $1
        let min_size = calculate_min_order_size(dec!(0.20));
        assert_eq!(min_size, dec!(5)); // 5 shares × $0.20 = $1
    }

    #[test]
    fn test_calculate_min_order_size_low_price() {
        // Price $0.10 - 5 shares = $0.50, need 10 shares for $1
        let min_size = calculate_min_order_size(dec!(0.10));
        assert_eq!(min_size, dec!(10)); // ceil(1/0.10) = 10
    }

    #[test]
    fn test_calculate_min_order_size_very_low_price() {
        // Price $0.07 - 5 shares = $0.35, need ceil(1/0.07) = 15 shares
        let min_size = calculate_min_order_size(dec!(0.07));
        assert_eq!(min_size, dec!(15)); // ceil(1/0.07) = ceil(14.28) = 15
    }

    #[test]
    fn test_calculate_min_order_size_tiny_price() {
        // Price $0.01 - need 100 shares for $1
        let min_size = calculate_min_order_size(dec!(0.01));
        assert_eq!(min_size, dec!(100)); // ceil(1/0.01) = 100
    }

    #[test]
    fn test_calculate_min_order_size_zero_price() {
        // Price $0.00 - fallback to MIN_SHARES
        let min_size = calculate_min_order_size(dec!(0.00));
        assert_eq!(min_size, dec!(5)); // fallback
    }

    #[test]
    fn test_buy_result() {
        let result = BuyResult::success(
            "token123".to_string(),
            "order456".to_string(),
            dec!(100.0),
            dec!(0.45),
        );
        assert!(result.success);
        assert_eq!(result.filled_amount, dec!(100.0));

        let result = BuyResult::failure("token123".to_string(), "No liquidity");
        assert!(!result.success);
        assert_eq!(result.error, Some("No liquidity".to_string()));
    }

    #[test]
    fn test_buy_all_result() {
        let mut result = BuyAllResult::new();

        result.add_result(BuyResult::success(
            "t1".to_string(),
            "o1".to_string(),
            dec!(100.0),
            dec!(0.45),
        ));
        result.add_result(BuyResult::success(
            "t2".to_string(),
            "o2".to_string(),
            dec!(100.0),
            dec!(0.50),
        ));
        result.add_result(BuyResult::failure("t3".to_string(), "Failed"));

        result.finalize(3);

        assert!(!result.success);
        assert!(result.partial_k);
        assert_eq!(result.total_filled, dec!(200.0));
        assert_eq!(result.total_cost, dec!(95.0)); // 100*0.45 + 100*0.50
    }

    #[test]
    fn test_buy_all_result_with_adjusted_amount() {
        let mut result = BuyAllResult::new();
        result.effective_amount = dec!(15);
        result.amount_adjusted = true;

        assert!(result.amount_adjusted);
        assert_eq!(result.effective_amount, dec!(15));
    }
}
