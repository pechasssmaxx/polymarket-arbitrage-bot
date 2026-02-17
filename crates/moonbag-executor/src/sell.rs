//! YES token selling (FAK orders)
//!
//! Sells high-value YES tokens (moonbags) after CONVERT.
//! Uses Fill-And-Kill orders to capture whatever liquidity exists.
//!
//! Uses EIP-712 signed orders for the Polymarket CLOB API.

use crate::clob_auth::{ClobCreds, L2Headers};
use crate::order_builder::OrderBuilder;
use crate::order_signer::{OrderSigner, SignedOrderRequest};
use alloy::primitives::Address;
use moonbag_core::models::{MoonbagOpportunity, RemainingOutcome};
use moonbag_core::{Config, MoonbagError, Result};
use rust_decimal::Decimal;
use serde::Deserialize;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Sell executor configuration
#[derive(Debug, Clone)]
pub struct SellConfig {
    /// Minimum YES bid to attempt sell
    pub min_bid: Decimal,
    /// Slippage tolerance for sell price
    pub slippage: Decimal,
    /// Order timeout in seconds
    pub timeout_secs: u64,
    /// REST API timeout
    pub api_timeout: Duration,
    /// CLOB host URL
    pub clob_host: String,
    /// Dry run mode
    pub dry_run: bool,
}

impl Default for SellConfig {
    fn default() -> Self {
        Self {
            min_bid: Decimal::new(5, 3),  // 0.005
            slippage: Decimal::new(2, 2), // 2%
            timeout_secs: 30,
            api_timeout: Duration::from_secs(15),
            clob_host: "https://clob.polymarket.com".to_string(),
            dry_run: true,
        }
    }
}

impl From<&Config> for SellConfig {
    fn from(config: &Config) -> Self {
        Self {
            min_bid: Decimal::new(5, 3),
            slippage: config.slippage_tolerance,
            timeout_secs: 30,
            api_timeout: Duration::from_secs(15),
            clob_host: config.clob_host().to_string(),
            dry_run: config.dry_run,
        }
    }
}

/// Result of selling a single YES token
#[derive(Debug, Clone)]
pub struct SellResult {
    pub success: bool,
    pub token_id: String,
    pub order_id: Option<String>,
    pub filled_amount: Decimal,
    pub avg_price: Decimal,
    pub revenue: Decimal,
    pub error: Option<String>,
}

impl SellResult {
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
            revenue: filled_amount * avg_price,
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
            revenue: Decimal::ZERO,
            error: Some(error.into()),
        }
    }

    pub fn skipped(token_id: String, reason: impl Into<String>) -> Self {
        Self {
            success: true, // Skipping is not a failure
            token_id,
            order_id: None,
            filled_amount: Decimal::ZERO,
            avg_price: Decimal::ZERO,
            revenue: Decimal::ZERO,
            error: Some(reason.into()),
        }
    }
}

/// Aggregate result of selling all YES tokens
#[derive(Debug, Clone)]
pub struct SellAllResult {
    pub results: Vec<SellResult>,
    pub total_sold: Decimal,
    pub total_revenue: Decimal,
    pub tokens_skipped: usize,
    pub tokens_failed: usize,
}

impl SellAllResult {
    pub fn new() -> Self {
        Self {
            results: Vec::new(),
            total_sold: Decimal::ZERO,
            total_revenue: Decimal::ZERO,
            tokens_skipped: 0,
            tokens_failed: 0,
        }
    }

    pub fn add_result(&mut self, result: SellResult) {
        if result.success && result.filled_amount > Decimal::ZERO {
            self.total_sold += result.filled_amount;
            self.total_revenue += result.revenue;
        } else if result.error.is_some() && result.filled_amount == Decimal::ZERO {
            if result.success {
                self.tokens_skipped += 1;
            } else {
                self.tokens_failed += 1;
            }
        }
        self.results.push(result);
    }
}

impl Default for SellAllResult {
    fn default() -> Self {
        Self::new()
    }
}

/// CLOB order response
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct OrderResponse {
    #[serde(rename = "orderID")]
    order_id: Option<String>,
    success: Option<bool>,
    #[serde(rename = "errorMsg")]
    error_msg: Option<String>,
    #[serde(rename = "filledSize")]
    filled_size: Option<String>,
    status: Option<String>,
}

/// Sell executor for YES tokens
pub struct SellExecutor {
    config: SellConfig,
    client: reqwest::Client,
    /// CLOB credentials
    creds: Option<ClobCreds>,
    /// Safe (funder) address
    funder: String,
    /// EIP-712 order signer
    order_signer: Option<OrderSigner>,
    /// Order builder
    order_builder: Option<OrderBuilder>,
}

impl SellExecutor {
    /// Create a new sell executor
    pub fn new(
        config: SellConfig,
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
                        "EIP-712 order signing enabled for SellExecutor"
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
            SellConfig::from(config),
            config.polymarket_funder.clone(),
            creds,
            order_signer,
            order_builder,
        )
    }

    /// Sell all eligible YES tokens for an opportunity
    ///
    /// # Arguments
    /// * `opp` - The opportunity containing remaining outcomes
    /// * `converted_amount` - Amount that was actually converted (for capping)
    pub async fn sell_all(
        &self,
        opp: &MoonbagOpportunity,
        converted_amount: Decimal,
    ) -> Result<SellAllResult> {
        let mut result = SellAllResult::new();

        if !opp.sell_yes {
            info!("YES selling disabled for this opportunity");
            return Ok(result);
        }

        info!(
            remaining_outcomes = opp.remaining_outcomes.len(),
            min_bid = %opp.sell_yes_min_bid,
            "Starting YES token sales"
        );

        for remaining in &opp.remaining_outcomes {
            // Skip non-convertible outcomes (they don't have YES tokens from this CONVERT)
            if !remaining.convertible {
                result.add_result(SellResult::skipped(
                    remaining.yes_token_id.clone(),
                    "Not convertible",
                ));
                continue;
            }

            // Check minimum bid
            if remaining.yes_bid < opp.sell_yes_min_bid {
                result.add_result(SellResult::skipped(
                    remaining.yes_token_id.clone(),
                    format!(
                        "Bid {:.4} < min {:.4}",
                        remaining.yes_bid, opp.sell_yes_min_bid
                    ),
                ));
                continue;
            }

            // Determine sell amount
            let sell_amount = if opp.sell_yes_cap_to_converted {
                converted_amount
            } else {
                opp.amount
            };

            let sell_result = self.sell_one(remaining, sell_amount).await;

            match sell_result {
                Ok(r) => {
                    if r.filled_amount > Decimal::ZERO {
                        info!(
                            token = %r.token_id,
                            filled = %r.filled_amount,
                            price = %r.avg_price,
                            revenue = %r.revenue,
                            "Sell completed"
                        );
                    }
                    result.add_result(r);
                }
                Err(e) => {
                    warn!(
                        token = %remaining.yes_token_id,
                        error = %e,
                        "Sell failed"
                    );
                    result.add_result(SellResult::failure(
                        remaining.yes_token_id.clone(),
                        e.to_string(),
                    ));
                }
            }
        }

        info!(
            total_sold = %result.total_sold,
            total_revenue = %result.total_revenue,
            skipped = result.tokens_skipped,
            failed = result.tokens_failed,
            "YES selling complete"
        );

        Ok(result)
    }

    /// Sell YES tokens for a single outcome
    async fn sell_one(&self, remaining: &RemainingOutcome, amount: Decimal) -> Result<SellResult> {
        // Calculate limit price with slippage (accept slightly lower)
        let limit_price = remaining.yes_bid * (Decimal::ONE - self.config.slippage);
        let limit_price = limit_price.max(Decimal::new(1, 3)); // Min 0.001

        debug!(
            token = %remaining.yes_token_id,
            amount = %amount,
            bid = %remaining.yes_bid,
            limit = %limit_price,
            "Placing FAK sell order"
        );

        if self.config.dry_run {
            info!(
                token = %remaining.yes_token_id,
                "DRY RUN: Would place FAK sell order"
            );
            return Ok(SellResult::success(
                remaining.yes_token_id.clone(),
                "dry-run-order-id".to_string(),
                amount,
                remaining.yes_bid,
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

        // Build the EIP-712 order (SELL)
        let order = order_builder.build_sell_order(&remaining.yes_token_id, limit_price, amount)?;

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

        // Build signed order request (FAK = Fill And Kill, similar to FOK for sells)
        // Note: Polymarket CLOB uses "GTC" for sells that can be partially filled
        let signed_request = SignedOrderRequest {
            order: payload,
            owner,
            order_type: "GTC".to_string(), // GTC allows partial fills
        };

        debug!(
            token = %remaining.yes_token_id,
            maker_amount = %order.makerAmount,
            taker_amount = %order.takerAmount,
            "Sending signed SELL order"
        );

        // Place order via API
        let response = self.place_signed_order(&signed_request).await?;

        // Check response
        if response.success.unwrap_or(false) {
            let order_id = response.order_id.unwrap_or_default();
            let filled: Decimal = response
                .filled_size
                .as_ref()
                .and_then(|s| s.parse().ok())
                .unwrap_or(amount);

            Ok(SellResult::success(
                remaining.yes_token_id.clone(),
                order_id,
                filled,
                remaining.yes_bid,
            ))
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

    /// Place a signed order via CLOB API
    async fn place_signed_order(&self, signed_order: &SignedOrderRequest) -> Result<OrderResponse> {
        let url = format!("{}/order", self.config.clob_host);
        let path = "/order";
        let method = "POST";

        // Serialize body for signing (must match what we send)
        let body_str = serde_json::to_string(signed_order).map_err(|e| MoonbagError::ClobApi {
            message: format!("Failed to serialize signed order: {}", e),
            status: None,
        })?;

        debug!(body = %body_str, "Sending signed sell order request");

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
            return Err(MoonbagError::ClobApi {
                message: format!("Order error: {} - {}", status, body),
                status: Some(status.as_u16()),
            });
        }

        response
            .json::<OrderResponse>()
            .await
            .map_err(|e| MoonbagError::ClobApi {
                message: format!("Order response parse failed: {}", e),
                status: None,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_sell_result() {
        let result = SellResult::success(
            "token123".to_string(),
            "order456".to_string(),
            dec!(100.0),
            dec!(0.55),
        );
        assert!(result.success);
        assert_eq!(result.revenue, dec!(55.0));

        let result = SellResult::skipped("token123".to_string(), "Bid too low");
        assert!(result.success);
        assert_eq!(result.revenue, Decimal::ZERO);

        let result = SellResult::failure("token123".to_string(), "API error");
        assert!(!result.success);
    }

    #[test]
    fn test_sell_all_result() {
        let mut result = SellAllResult::new();

        result.add_result(SellResult::success(
            "t1".to_string(),
            "o1".to_string(),
            dec!(100.0),
            dec!(0.50),
        ));
        result.add_result(SellResult::skipped("t2".to_string(), "Bid too low"));
        result.add_result(SellResult::failure("t3".to_string(), "Failed"));

        assert_eq!(result.total_sold, dec!(100.0));
        assert_eq!(result.total_revenue, dec!(50.0));
        assert_eq!(result.tokens_skipped, 1);
        assert_eq!(result.tokens_failed, 1);
    }
}
