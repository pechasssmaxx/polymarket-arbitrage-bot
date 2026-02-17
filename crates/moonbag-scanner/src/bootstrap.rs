//! Bootstrap orderbook snapshots and market prices from REST API
//!
//! Two bootstrapping modes:
//! 1. Orderbook bootstrap via POST /books (raw orderbook depth)
//! 2. Price bootstrap via GET /price (top-of-book prices; use `side=sell` for best ask)
//!
//! IMPORTANT: Use price bootstrap for accurate analysis. The orderbook has wide
//! spreads (bids at $0.01, asks at $0.99) but actual market prices are different.

use crate::orderbook::OrderbookManager;
use moonbag_core::models::PriceLevel;
use moonbag_core::Result;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

/// CLOB API base URL
const DEFAULT_CLOB_API_BASE_URL: &str = "https://clob.polymarket.com";

/// Request body for POST /books
#[derive(Debug, Serialize)]
struct BookRequest {
    token_id: String,
}

/// Response from POST /books
#[derive(Debug, Deserialize)]
struct BookResponse {
    asset_id: Option<String>,
    #[serde(alias = "tokenId")]
    token_id: Option<String>,
    bids: Option<Vec<BookLevel>>,
    asks: Option<Vec<BookLevel>>,
}

/// Book level from REST API
#[derive(Debug, Deserialize)]
struct BookLevel {
    price: Option<String>,
    size: Option<String>,
}

impl BookLevel {
    fn to_price_level(&self) -> Option<PriceLevel> {
        let price = Decimal::from_str(self.price.as_ref()?).ok()?;
        let size = Decimal::from_str(self.size.as_ref()?).ok()?;
        if size <= Decimal::ZERO {
            return None;
        }
        Some((price, size))
    }
}

/// Response from GET /price
#[derive(Debug, Deserialize)]
struct PriceResponse {
    price: Option<String>,
}

/// Market price for a token (from /price endpoint)
#[derive(Debug, Clone)]
pub struct TokenPrice {
    pub token_id: String,
    pub price: Decimal,
}

/// Result of price bootstrap
#[derive(Debug, Clone, Default)]
pub struct PriceBootstrapResult {
    /// Token ID -> price mapping
    pub prices: HashMap<String, Decimal>,
    pub success_count: usize,
    pub failed_count: usize,
}

/// Bootstrap configuration
#[derive(Debug, Clone)]
pub struct BootstrapConfig {
    /// CLOB API base URL (override for testing or alternative hosts)
    pub clob_api_base_url: String,
    /// Batch size for /books requests
    pub batch_size: usize,
    /// Concurrent requests
    pub concurrency: usize,
    /// Request timeout
    pub timeout: Duration,
    /// Only fetch NO token books (sufficient for USDC-only edges)
    pub only_no_tokens: bool,
}

impl Default for BootstrapConfig {
    fn default() -> Self {
        Self {
            clob_api_base_url: DEFAULT_CLOB_API_BASE_URL.to_string(),
            batch_size: 200,
            concurrency: 4,
            timeout: Duration::from_secs(10),
            only_no_tokens: true,
        }
    }
}

/// Bootstrap orderbooks from REST API
pub struct Bootstrapper {
    config: BootstrapConfig,
    client: reqwest::Client,
}

impl Bootstrapper {
    /// Create a new bootstrapper
    pub fn new(config: BootstrapConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .expect("Failed to create HTTP client");

        Self { config, client }
    }

    /// Create with default config
    pub fn with_defaults() -> Self {
        Self::new(BootstrapConfig::default())
    }

    /// Fetch orderbooks for all tokens and update the orderbook manager
    ///
    /// Returns the number of tokens successfully bootstrapped.
    pub async fn bootstrap(&self, orderbook: &Arc<OrderbookManager>) -> Result<usize> {
        let tokens = if self.config.only_no_tokens {
            orderbook.no_token_ids()
        } else {
            orderbook.all_token_ids()
        };

        self.bootstrap_tokens(&tokens, orderbook.clone()).await
    }

    /// Fetch orderbooks for specific tokens
    pub async fn bootstrap_batch(
        &self,
        tokens: &[String],
        orderbook: Arc<OrderbookManager>,
    ) -> BootstrapResult {
        let total = tokens.len();
        if total == 0 {
            return BootstrapResult {
                success_count: 0,
                failed_count: 0,
            };
        }

        let started = Instant::now();
        let semaphore = Arc::new(Semaphore::new(self.config.concurrency));
        let mut handles = Vec::new();
        let mut completed = 0usize;

        // Process in batches
        for batch in tokens.chunks(self.config.batch_size) {
            let batch = batch.to_vec();
            let sem = semaphore.clone();
            let client = self.client.clone();
            let ob = orderbook.clone();
            let base_url = self.config.clob_api_base_url.clone();

            let handle = tokio::spawn(async move {
                let _permit = sem.acquire().await.unwrap();
                fetch_batch(&client, &base_url, &ob, batch).await
            });

            handles.push(handle);
        }

        // Collect results
        for handle in handles {
            match handle.await {
                Ok(count) => completed += count,
                Err(e) => warn!("Bootstrap batch failed: {}", e),
            }
        }

        let elapsed = started.elapsed();
        let rate = if elapsed.as_secs_f64() > 0.0 {
            completed as f64 / elapsed.as_secs_f64()
        } else {
            0.0
        };

        debug!(
            "Bootstrap: {}/{} tokens in {:.1}s ({:.0} tok/s)",
            completed,
            total,
            elapsed.as_secs_f64(),
            rate
        );

        BootstrapResult {
            success_count: completed,
            failed_count: total - completed,
        }
    }

    async fn bootstrap_tokens(
        &self,
        tokens: &[String],
        orderbook: Arc<OrderbookManager>,
    ) -> Result<usize> {
        let result = self.bootstrap_batch(tokens, orderbook).await;
        Ok(result.success_count)
    }

    /// Fetch actual market prices from /price endpoint
    ///
    /// This is the CORRECT way to get prices for analysis!
    /// The orderbook has wide spreads, but /price returns the actual market price.
    pub async fn bootstrap_prices(&self, tokens: &[String]) -> PriceBootstrapResult {
        let total = tokens.len();
        if total == 0 {
            return PriceBootstrapResult::default();
        }

        let started = Instant::now();
        let semaphore = Arc::new(Semaphore::new(self.config.concurrency * 2)); // More concurrency for simple GET
        let mut handles = Vec::new();

        // Fetch prices in parallel
        for token in tokens {
            let token = token.clone();
            let sem = semaphore.clone();
            let client = self.client.clone();
            let base_url = self.config.clob_api_base_url.clone();

            let handle = tokio::spawn(async move {
                let _permit = sem.acquire().await.unwrap();
                fetch_price(&client, &base_url, &token).await
            });

            handles.push(handle);
        }

        // Collect results
        let mut prices = HashMap::new();
        let mut success_count = 0;
        let mut failed_count = 0;

        for handle in handles {
            match handle.await {
                Ok(Some(tp)) => {
                    prices.insert(tp.token_id, tp.price);
                    success_count += 1;
                }
                Ok(None) => failed_count += 1,
                Err(_) => failed_count += 1,
            }
        }

        let elapsed = started.elapsed();
        info!(
            "Price bootstrap: {}/{} tokens in {:.1}s ({:.0} tok/s)",
            success_count,
            total,
            elapsed.as_secs_f64(),
            success_count as f64 / elapsed.as_secs_f64().max(0.001)
        );

        PriceBootstrapResult {
            prices,
            success_count,
            failed_count,
        }
    }

    /// Fetch prices for all NO tokens in the orderbook manager
    pub async fn bootstrap_no_prices(
        &self,
        orderbook: &Arc<OrderbookManager>,
    ) -> PriceBootstrapResult {
        let tokens = orderbook.no_token_ids();
        self.bootstrap_prices(&tokens).await
    }
}

/// Fetch a single price from /price endpoint
async fn fetch_price(
    client: &reqwest::Client,
    base_url: &str,
    token_id: &str,
) -> Option<TokenPrice> {
    let result = client
        .get(format!("{}/price", base_url))
        // NOTE: Polymarket's /price `side` refers to the resting order side.
        // To estimate the price to BUY immediately, we need the best ASK, i.e. `side=sell`.
        .query(&[("token_id", token_id), ("side", "sell")])
        .send()
        .await;

    let response = match result {
        Ok(r) if r.status().is_success() => r,
        _ => return None,
    };

    let data: PriceResponse = response.json().await.ok()?;
    let price = Decimal::from_str(data.price.as_ref()?).ok()?;

    Some(TokenPrice {
        token_id: token_id.to_string(),
        price,
    })
}

/// Result of bootstrap operation
#[derive(Debug, Clone)]
pub struct BootstrapResult {
    pub success_count: usize,
    pub failed_count: usize,
}

/// Fetch a batch of orderbooks
async fn fetch_batch(
    client: &reqwest::Client,
    base_url: &str,
    orderbook: &Arc<OrderbookManager>,
    tokens: Vec<String>,
) -> usize {
    let body: Vec<BookRequest> = tokens
        .iter()
        .map(|t| BookRequest {
            token_id: t.clone(),
        })
        .collect();

    let result = client
        .post(format!("{}/books", base_url))
        .json(&body)
        .send()
        .await;

    let response = match result {
        Ok(r) => r,
        Err(e) => {
            debug!("Bootstrap request failed: {}", e);
            return 0;
        }
    };

    if !response.status().is_success() {
        debug!("Bootstrap request returned {}", response.status());
        return 0;
    }

    let books: Vec<BookResponse> = match response.json().await {
        Ok(b) => b,
        Err(e) => {
            debug!("Failed to parse bootstrap response: {}", e);
            return 0;
        }
    };

    let mut count = 0;

    for book in books {
        let token_id = book.asset_id.or(book.token_id).unwrap_or_default();

        if token_id.is_empty() {
            continue;
        }

        let bids: Vec<PriceLevel> = book
            .bids
            .unwrap_or_default()
            .iter()
            .filter_map(|l| l.to_price_level())
            .collect();

        let asks: Vec<PriceLevel> = book
            .asks
            .unwrap_or_default()
            .iter()
            .filter_map(|l| l.to_price_level())
            .collect();

        orderbook.update_orderbook(&token_id, bids, asks);
        count += 1;
    }

    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use std::str::FromStr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn test_book_level_parsing() {
        let level = BookLevel {
            price: Some("0.45".to_string()),
            size: Some("100".to_string()),
        };
        let parsed = level.to_price_level().unwrap();
        assert_eq!(parsed.0, Decimal::new(45, 2));
        assert_eq!(parsed.1, Decimal::new(100, 0));
    }

    #[tokio::test]
    async fn test_bootstrap_prices_uses_best_ask_side() {
        // Stand up a tiny HTTP server and assert the scanner requests `/price` with `side=sell`
        // (best ask, i.e. price to BUY immediately).
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{}", addr);

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();

            let mut req = Vec::new();
            let mut buf = [0u8; 1024];
            loop {
                let n = socket.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                req.extend_from_slice(&buf[..n]);
                if req.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
                if req.len() > 16 * 1024 {
                    break;
                }
            }

            let req = String::from_utf8_lossy(&req);
            assert!(req.starts_with("GET /price?"), "unexpected request: {req}");
            assert!(req.contains("side=sell"), "expected side=sell, got: {req}");

            let body = r#"{"price":"0.42"}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(resp.as_bytes()).await.unwrap();
        });

        let mut config = BootstrapConfig::default();
        config.clob_api_base_url = base_url;
        let bootstrapper = Bootstrapper::new(config);

        let token = "token123".to_string();
        let result = bootstrapper.bootstrap_prices(&[token.clone()]).await;

        assert_eq!(
            result.prices.get(&token).copied(),
            Some(Decimal::from_str("0.42").unwrap())
        );

        server.await.unwrap();
    }
}
