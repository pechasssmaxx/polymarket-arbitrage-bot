//! Moonbag Executor Binary
//!
//! Reads opportunities from queue and executes trades.
//! Also supports --test-order mode to verify Rust order placement works.

use alloy::primitives::Address;
use clap::Parser;
use moonbag_core::Config;
use moonbag_executor::clob_auth::ClobCreds;
use moonbag_executor::order_builder::OrderBuilder;
use moonbag_executor::order_signer::{OrderSigner, SignedOrderRequest};
use rust_decimal::Decimal;
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[derive(Parser, Debug)]
#[command(name = "moonbag-executor")]
#[command(about = "Polymarket moonbag trade executor")]
struct Args {
    /// Path to config file
    #[arg(short, long)]
    config: Option<String>,

    /// Enable dry-run mode (no actual trades)
    #[arg(long, default_value = "false")]
    dry_run: bool,

    /// Log level (debug, info, warn, error)
    #[arg(long, default_value = "info")]
    log_level: String,

    /// Test order placement (sends a GTC order at very low price, then cancels)
    #[arg(long, default_value = "false")]
    test_order: bool,

    /// Token ID to test with (NO token)
    #[arg(long)]
    test_token: Option<String>,

    /// Price for test order (default 0.01)
    #[arg(long, default_value = "0.01")]
    test_price: f64,

    /// Size for test order (default 5.0)
    #[arg(long, default_value = "5.0")]
    test_size: f64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load environment
    dotenvy::dotenv().ok();

    let args = Args::parse();

    // Initialize tracing
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&args.log_level));

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Load config
    let config = Config::from_env()?;
    info!(
        safe_address = %config.polymarket_funder,
        dry_run = args.dry_run,
        "Config loaded"
    );

    // Test order mode
    if args.test_order {
        return test_order_placement(&config, &args).await;
    }

    info!("Starting moonbag executor...");

    // TODO: Initialize executor components
    // - Queue reader
    // - CLOB client (buy/sell)
    // - Safe executor (convert)
    // - Notifier

    info!("Executor started. Press Ctrl+C to stop.");

    // TODO: Run executor loop
    tokio::signal::ctrl_c().await?;
    info!("Shutting down executor...");

    Ok(())
}

/// Test order placement to verify Rust EIP-712 signing works
async fn test_order_placement(config: &Config, args: &Args) -> anyhow::Result<()> {
    info!("=== TEST ORDER MODE ===");

    // Get test token ID
    let token_id = args.test_token.clone().unwrap_or_else(|| {
        // Default: Use a token with active orderbook
        // "When will Bitcoin reach $150k?" - YES token with good liquidity
        "79305227607785128660070965053216693833362599386613208059644360355012081512371".to_string()
    });

    let price = Decimal::try_from(args.test_price)?;
    let size = Decimal::try_from(args.test_size)?;

    info!(token_id = %token_id, price = %price, size = %size, "Test parameters");

    // Check credentials
    let api_key = config.polymarket_api_key.as_ref().ok_or_else(|| {
        anyhow::anyhow!("POLYMARKET_API_KEY not set")
    })?;
    let api_secret = config.polymarket_api_secret.as_ref().ok_or_else(|| {
        anyhow::anyhow!("POLYMARKET_API_SECRET not set")
    })?;
    let passphrase = config.polymarket_passphrase.as_ref().ok_or_else(|| {
        anyhow::anyhow!("POLYMARKET_PASSPHRASE not set")
    })?;

    info!("CLOB credentials found");

    // Create signer
    let funder_addr: Address = config.polymarket_funder.parse()?;
    let order_signer = OrderSigner::new(&config.polygon_private_key, funder_addr, true)?;

    info!(
        maker = %order_signer.maker_address(),
        signer = %order_signer.signer_address(),
        "Order signer created"
    );

    // Create order builder
    let order_builder = OrderBuilder::new(funder_addr, order_signer.signer_address());

    // Build order (BUY NO tokens at low price)
    let order = order_builder.build_buy_order(&token_id, price, size)?;

    info!(
        salt = %order.salt,
        maker_amount = %order.makerAmount,
        taker_amount = %order.takerAmount,
        side = order.side,
        signature_type = order.signatureType,
        "Order built"
    );

    // Sign order
    let payload = order_signer.sign_order(&order).await?;

    info!(
        signature = %payload.signature,
        "Order signed"
    );

    // Build request
    let signed_request = SignedOrderRequest {
        order: payload,
        owner: api_key.clone(),
        order_type: "GTC".to_string(), // GTC so we can cancel it
    };

    // Serialize to see payload (using proper number serialization)
    let body_str = signed_request.to_json_string()?;
    info!("Request body:\n{}", body_str);

    // Create CLOB auth
    let creds = ClobCreds::new(api_key.clone(), api_secret.clone(), passphrase.clone());

    // Send to CLOB API
    let client = reqwest::Client::new();
    let url = format!("{}/order", config.clob_host());
    let path = "/order";
    let method = "POST";
    let poly_address = order_signer.signer_address().to_string();

    let headers = moonbag_executor::clob_auth::L2Headers::new(
        &poly_address,
        &creds,
        method,
        path,
        Some(&body_str),
    )?;

    info!(url = %url, "Sending order to CLOB API...");

    let mut req = client
        .post(&url)
        .body(body_str)
        .header(reqwest::header::CONTENT_TYPE, "application/json");

    req = headers.apply(req);

    let response = req.send().await?;
    let status = response.status();
    let body = response.text().await?;

    if status.is_success() {
        info!(status = %status, body = %body, "ORDER PLACED SUCCESSFULLY!");

        // Parse response to get order ID
        #[derive(serde::Deserialize)]
        struct OrderResponse {
            #[serde(rename = "orderID")]
            order_id: Option<String>,
        }

        if let Ok(resp) = serde_json::from_str::<OrderResponse>(&body) {
            if let Some(order_id) = resp.order_id {
                info!(order_id = %order_id, "Order ID received");

                // Cancel the order
                info!("Cancelling test order...");
                let cancel_result = cancel_order(config, &creds, &order_signer, &order_id).await;
                match cancel_result {
                    Ok(_) => info!("Order cancelled successfully"),
                    Err(e) => error!(error = %e, "Failed to cancel order"),
                }
            }
        }
    } else {
        error!(status = %status, body = %body, "ORDER FAILED!");
        return Err(anyhow::anyhow!("Order placement failed: {} - {}", status, body));
    }

    info!("=== TEST COMPLETE ===");
    Ok(())
}

/// Cancel an order
async fn cancel_order(
    config: &Config,
    creds: &ClobCreds,
    order_signer: &OrderSigner,
    order_id: &str,
) -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    let url = format!("{}/order/{}", config.clob_host(), order_id);
    let path = format!("/order/{}", order_id);
    let method = "DELETE";

    let poly_address = order_signer.signer_address().to_string();

    let headers = moonbag_executor::clob_auth::L2Headers::new(
        &poly_address,
        creds,
        method,
        &path,
        None,
    )?;

    let mut req = client.delete(&url);
    req = headers.apply(req);

    let response = req.send().await?;
    let status = response.status();
    let body = response.text().await?;

    if status.is_success() {
        info!(status = %status, "Order cancelled");
        Ok(())
    } else {
        Err(anyhow::anyhow!("Cancel failed: {} - {}", status, body))
    }
}
