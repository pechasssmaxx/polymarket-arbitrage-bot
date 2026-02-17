//! One-time latency benchmark for all scanner processes
//!
//! Measures:
//! 1. JSON parsing (markets.json)
//! 2. Market registration
//! 3. REST bootstrap (orderbook fetch)
//! 4. WebSocket connection + subscription
//! 5. Message parsing
//! 6. Orderbook updates
//! 7. Quick check (per market)
//! 8. Full VWAP analysis (per market)
//! 9. End-to-end detection
//! 10. CLOB API endpoints
//! 11. Parallel orderbook fetching
//! 12. RPC latency (Polygon)
//! 13. Crypto signing (ECDSA)

use moonbag_analyzer::{quick_check, MoonbagAnalyzer};
use moonbag_core::models::{MarketState, StoredMarket};
use rust_decimal::Decimal;
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};

struct LatencyStats {
    samples: Vec<Duration>,
}

impl LatencyStats {
    fn new() -> Self {
        Self {
            samples: Vec::new(),
        }
    }

    fn record(&mut self, d: Duration) {
        self.samples.push(d);
    }

    fn count(&self) -> usize {
        self.samples.len()
    }

    fn min(&self) -> Duration {
        self.samples.iter().min().copied().unwrap_or(Duration::ZERO)
    }

    fn max(&self) -> Duration {
        self.samples.iter().max().copied().unwrap_or(Duration::ZERO)
    }

    fn mean(&self) -> Duration {
        if self.samples.is_empty() {
            return Duration::ZERO;
        }
        let total: Duration = self.samples.iter().sum();
        total / self.samples.len() as u32
    }

    fn percentile(&self, p: f64) -> Duration {
        if self.samples.is_empty() {
            return Duration::ZERO;
        }
        let mut sorted = self.samples.clone();
        sorted.sort();
        let idx = ((p / 100.0) * (sorted.len() - 1) as f64).round() as usize;
        sorted[idx.min(sorted.len() - 1)]
    }

    fn total(&self) -> Duration {
        self.samples.iter().sum()
    }
}

fn format_duration(d: Duration) -> String {
    let micros = d.as_micros();
    if micros < 1000 {
        format!("{:>6}μs", micros)
    } else if micros < 1_000_000 {
        format!("{:>6.2}ms", micros as f64 / 1000.0)
    } else {
        format!("{:>6.2}s ", micros as f64 / 1_000_000.0)
    }
}

fn print_stats(name: &str, stats: &LatencyStats) {
    println!(
        "  {:<25} n={:<6} min={} avg={} p50={} p95={} p99={} max={} total={}",
        name,
        stats.count(),
        format_duration(stats.min()),
        format_duration(stats.mean()),
        format_duration(stats.percentile(50.0)),
        format_duration(stats.percentile(95.0)),
        format_duration(stats.percentile(99.0)),
        format_duration(stats.max()),
        format_duration(stats.total()),
    );
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("╔══════════════════════════════════════════════════════════════════╗");
    println!("║           MOONBAG SCANNER LATENCY BENCHMARK                      ║");
    println!("╚══════════════════════════════════════════════════════════════════╝\n");

    // =========================================================================
    // 1. JSON PARSING (markets.json)
    // =========================================================================
    println!("① JSON PARSING (markets.json)");
    println!("─────────────────────────────────────────────────────────────────────");

    let start = Instant::now();
    let content = tokio::fs::read_to_string("moonbag/markets.json").await?;
    let file_read_time = start.elapsed();

    let start = Instant::now();
    let markets: Vec<StoredMarket> = serde_json::from_str(&content)?;
    let json_parse_time = start.elapsed();

    println!(
        "  File size:        {:.2} MB",
        content.len() as f64 / 1_000_000.0
    );
    println!("  Markets:          {}", markets.len());
    println!("  File read:        {}", format_duration(file_read_time));
    println!("  JSON parse:       {}", format_duration(json_parse_time));
    println!(
        "  Total:            {}",
        format_duration(file_read_time + json_parse_time)
    );
    println!(
        "  Per market:       {}",
        format_duration((file_read_time + json_parse_time) / markets.len() as u32)
    );
    println!();

    // =========================================================================
    // 2. MARKET REGISTRATION (StoredMarket → MarketState)
    // =========================================================================
    println!("② MARKET REGISTRATION (StoredMarket → MarketState)");
    println!("─────────────────────────────────────────────────────────────────────");

    let mut registration_stats = LatencyStats::new();
    let mut market_states: Vec<MarketState> = Vec::with_capacity(markets.len());

    for stored in &markets {
        let start = Instant::now();
        let state = stored.to_market_state();
        registration_stats.record(start.elapsed());
        market_states.push(state);
    }

    print_stats("to_market_state()", &registration_stats);
    println!();

    // =========================================================================
    // 3. REST BOOTSTRAP (fetch orderbooks)
    // =========================================================================
    println!("③ REST BOOTSTRAP (fetch orderbooks via POST /books)");
    println!("─────────────────────────────────────────────────────────────────────");

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

    // Sample 100 random NO tokens for benchmark
    let sample_tokens: Vec<String> = markets
        .iter()
        .take(50)
        .flat_map(|m| m.outcomes.iter().map(|o| o.no_token_id.clone()))
        .take(200)
        .collect();

    println!(
        "  Sampling {} tokens in batches of 50...",
        sample_tokens.len()
    );

    let mut batch_stats = LatencyStats::new();
    let mut parse_stats = LatencyStats::new();

    for batch in sample_tokens.chunks(50) {
        let body: Vec<serde_json::Value> = batch
            .iter()
            .map(|t| serde_json::json!({"token_id": t}))
            .collect();

        let start = Instant::now();
        let resp = client
            .post("https://clob.polymarket.com/books")
            .json(&body)
            .send()
            .await?;
        let fetch_time = start.elapsed();
        batch_stats.record(fetch_time);

        let start = Instant::now();
        let _books: serde_json::Value = resp.json().await?;
        parse_stats.record(start.elapsed());
    }

    print_stats("HTTP fetch (50 tokens)", &batch_stats);
    print_stats("Response JSON parse", &parse_stats);
    println!(
        "  Estimated full bootstrap ({} tokens): {}",
        markets.iter().map(|m| m.outcomes.len()).sum::<usize>() * 2,
        format_duration(
            batch_stats.mean()
                * (markets.iter().map(|m| m.outcomes.len()).sum::<usize>() as u32 / 50)
        )
    );
    println!();

    // =========================================================================
    // 4. WEBSOCKET CONNECTION + SUBSCRIPTION
    // =========================================================================
    println!("④ WEBSOCKET CONNECTION + SUBSCRIPTION");
    println!("─────────────────────────────────────────────────────────────────────");

    use tokio_tungstenite::connect_async;

    let ws_url = "wss://ws-subscriptions-clob.polymarket.com/ws/market";

    let start = Instant::now();
    let (mut ws_stream, _) = connect_async(ws_url).await?;
    let connect_time = start.elapsed();

    println!("  WebSocket connect: {}", format_duration(connect_time));

    // Subscribe to a batch of tokens
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let sub_tokens: Vec<&str> = sample_tokens.iter().take(100).map(|s| s.as_str()).collect();
    let sub_msg = serde_json::json!({
        "type": "subscribe",
        "channel": "market",
        "assets_ids": sub_tokens,
    });

    let start = Instant::now();
    ws_stream
        .send(Message::Text(sub_msg.to_string().into()))
        .await?;
    let send_time = start.elapsed();

    println!(
        "  Subscribe send (100 tokens): {}",
        format_duration(send_time)
    );

    // Wait for first message
    let start = Instant::now();
    let mut first_msg_time = Duration::ZERO;
    let mut msg_count = 0;
    let mut msg_parse_stats = LatencyStats::new();

    let timeout = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(msg) = ws_stream.next().await {
            if let Ok(Message::Text(text)) = msg {
                if first_msg_time == Duration::ZERO {
                    first_msg_time = start.elapsed();
                }

                let parse_start = Instant::now();
                let _: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
                msg_parse_stats.record(parse_start.elapsed());

                msg_count += 1;
                if msg_count >= 50 {
                    break;
                }
            }
        }
    })
    .await;

    if timeout.is_ok() {
        println!(
            "  First message received: {}",
            format_duration(first_msg_time)
        );
        print_stats("WS message JSON parse", &msg_parse_stats);
    } else {
        println!("  Timeout waiting for messages (5s)");
    }

    // Close websocket
    let _ = ws_stream.close(None).await;
    println!();

    // =========================================================================
    // 5. QUICK CHECK (per market)
    // =========================================================================
    println!("⑤ QUICK CHECK (per market, best-price filter)");
    println!("─────────────────────────────────────────────────────────────────────");

    // Add fake orderbook data for benchmarking
    let mut test_markets: Vec<MarketState> = market_states.iter().cloned().collect();
    for market in &mut test_markets {
        for outcome in market.outcomes.values_mut() {
            outcome.no_asks = vec![(Decimal::new(45, 2), Decimal::new(100, 0))];
            outcome.yes_bids = vec![(Decimal::new(5, 2), Decimal::new(100, 0))];
        }
    }

    let mut quick_check_stats = LatencyStats::new();
    let mut pass_count = 0;

    for market in &test_markets {
        let start = Instant::now();
        let passed = quick_check(
            market,
            Decimal::new(10, 0),
            Decimal::ZERO,
            2,
            None,
            Decimal::new(5, 3),
        );
        quick_check_stats.record(start.elapsed());
        if passed {
            pass_count += 1;
        }
    }

    print_stats("quick_check()", &quick_check_stats);
    println!(
        "  Pass rate: {}/{} ({:.1}%)",
        pass_count,
        test_markets.len(),
        100.0 * pass_count as f64 / test_markets.len() as f64
    );
    println!();

    // =========================================================================
    // 6. FULL VWAP ANALYSIS (per market)
    // =========================================================================
    println!("⑥ FULL VWAP ANALYSIS (per market)");
    println!("─────────────────────────────────────────────────────────────────────");

    let analyzer = MoonbagAnalyzer {
        min_profit: Decimal::ZERO,
        min_moonbags: 0,
        min_k: 2,
        max_k: None,
        ..Default::default()
    };

    let mut analysis_stats = LatencyStats::new();
    let mut opp_count = 0;

    for market in &test_markets {
        let start = Instant::now();
        let opp = analyzer.find_opportunity(market, Decimal::new(10, 0));
        analysis_stats.record(start.elapsed());
        if opp.is_some() {
            opp_count += 1;
        }
    }

    print_stats("find_opportunity()", &analysis_stats);
    println!(
        "  Opportunities found: {}/{}",
        opp_count,
        test_markets.len()
    );
    println!();

    // =========================================================================
    // 7. END-TO-END DETECTION (update → check → analyze)
    // =========================================================================
    println!("⑦ END-TO-END DETECTION (orderbook update → opportunity)");
    println!("─────────────────────────────────────────────────────────────────────");

    let mut e2e_stats = LatencyStats::new();

    // Simulate receiving an orderbook update and processing it
    for market in test_markets.iter().take(500) {
        let start = Instant::now();

        // 1. Simulate orderbook update (already done, just measure access)
        let _outcome_count = market.outcomes.len();

        // 2. Quick check
        let passed = quick_check(
            market,
            Decimal::new(10, 0),
            Decimal::ZERO,
            2,
            None,
            Decimal::new(5, 3),
        );

        // 3. Full analysis (only if quick check passes)
        if passed {
            let _opp = analyzer.find_opportunity(market, Decimal::new(10, 0));
        }

        e2e_stats.record(start.elapsed());
    }

    print_stats("End-to-end detection", &e2e_stats);
    println!();

    // =========================================================================
    // 8. THROUGHPUT ESTIMATES
    // =========================================================================
    println!("⑧ THROUGHPUT ESTIMATES");
    println!("─────────────────────────────────────────────────────────────────────");

    let avg_e2e = e2e_stats.mean();
    let markets_per_sec = if avg_e2e.as_nanos() > 0 {
        1_000_000_000.0 / avg_e2e.as_nanos() as f64
    } else {
        0.0
    };

    println!("  Markets/second:     {:.0}", markets_per_sec);
    println!(
        "  Max WS msg/second:  {:.0} (assuming 1 market per msg)",
        markets_per_sec
    );
    println!();

    // =========================================================================
    // 9. CLOB API ENDPOINTS LATENCY
    // =========================================================================
    println!("⑨ CLOB API ENDPOINTS LATENCY");
    println!("─────────────────────────────────────────────────────────────────────");

    // Sample token for testing
    let sample_token = &sample_tokens[0];
    let _sample_market_id = &markets[0].neg_risk_market_id;

    // GET /book (single orderbook)
    println!("  Testing CLOB endpoints (5 samples each)...\n");

    let mut get_book_stats = LatencyStats::new();
    for _ in 0..5 {
        let start = Instant::now();
        let _ = client
            .get(format!(
                "https://clob.polymarket.com/book?token_id={}",
                sample_token
            ))
            .send()
            .await;
        get_book_stats.record(start.elapsed());
    }
    print_stats("GET /book (single)", &get_book_stats);

    // POST /books (batch - 1 token)
    let mut post_books_1_stats = LatencyStats::new();
    for _ in 0..5 {
        let start = Instant::now();
        let _ = client
            .post("https://clob.polymarket.com/books")
            .json(&vec![serde_json::json!({"token_id": sample_token})])
            .send()
            .await;
        post_books_1_stats.record(start.elapsed());
    }
    print_stats("POST /books (1 token)", &post_books_1_stats);

    // POST /books (batch - 10 tokens)
    let mut post_books_10_stats = LatencyStats::new();
    let tokens_10: Vec<_> = sample_tokens.iter().take(10).collect();
    for _ in 0..5 {
        let body: Vec<_> = tokens_10
            .iter()
            .map(|t| serde_json::json!({"token_id": t}))
            .collect();
        let start = Instant::now();
        let _ = client
            .post("https://clob.polymarket.com/books")
            .json(&body)
            .send()
            .await;
        post_books_10_stats.record(start.elapsed());
    }
    print_stats("POST /books (10 tokens)", &post_books_10_stats);

    // POST /books (batch - 50 tokens)
    let mut post_books_50_stats = LatencyStats::new();
    let tokens_50: Vec<_> = sample_tokens.iter().take(50).collect();
    for _ in 0..5 {
        let body: Vec<_> = tokens_50
            .iter()
            .map(|t| serde_json::json!({"token_id": t}))
            .collect();
        let start = Instant::now();
        let _ = client
            .post("https://clob.polymarket.com/books")
            .json(&body)
            .send()
            .await;
        post_books_50_stats.record(start.elapsed());
    }
    print_stats("POST /books (50 tokens)", &post_books_50_stats);

    // POST /books (batch - 100 tokens)
    let mut post_books_100_stats = LatencyStats::new();
    let tokens_100: Vec<_> = sample_tokens.iter().take(100).collect();
    for _ in 0..5 {
        let body: Vec<_> = tokens_100
            .iter()
            .map(|t| serde_json::json!({"token_id": t}))
            .collect();
        let start = Instant::now();
        let _ = client
            .post("https://clob.polymarket.com/books")
            .json(&body)
            .send()
            .await;
        post_books_100_stats.record(start.elapsed());
    }
    print_stats("POST /books (100 tokens)", &post_books_100_stats);

    // GET /price
    let mut get_price_stats = LatencyStats::new();
    for _ in 0..5 {
        let start = Instant::now();
        let _ = client
            .get(format!(
                "https://clob.polymarket.com/price?token_id={}&side=buy",
                sample_token
            ))
            .send()
            .await;
        get_price_stats.record(start.elapsed());
    }
    print_stats("GET /price", &get_price_stats);

    // GET /prices (batch)
    let mut get_prices_stats = LatencyStats::new();
    for _ in 0..5 {
        let start = Instant::now();
        let _ = client
            .get("https://clob.polymarket.com/prices")
            .query(&[(
                "token_ids",
                tokens_10
                    .iter()
                    .map(|t| t.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
            )])
            .send()
            .await;
        get_prices_stats.record(start.elapsed());
    }
    print_stats("GET /prices (10 tokens)", &get_prices_stats);

    // GET /midpoint
    let mut get_midpoint_stats = LatencyStats::new();
    for _ in 0..5 {
        let start = Instant::now();
        let _ = client
            .get(format!(
                "https://clob.polymarket.com/midpoint?token_id={}",
                sample_token
            ))
            .send()
            .await;
        get_midpoint_stats.record(start.elapsed());
    }
    print_stats("GET /midpoint", &get_midpoint_stats);

    // GET /spread
    let mut get_spread_stats = LatencyStats::new();
    for _ in 0..5 {
        let start = Instant::now();
        let _ = client
            .get(format!(
                "https://clob.polymarket.com/spread?token_id={}",
                sample_token
            ))
            .send()
            .await;
        get_spread_stats.record(start.elapsed());
    }
    print_stats("GET /spread", &get_spread_stats);

    // GET /tick-size
    let mut get_tick_stats = LatencyStats::new();
    for _ in 0..5 {
        let start = Instant::now();
        let _ = client
            .get(format!(
                "https://clob.polymarket.com/tick-size?token_id={}",
                sample_token
            ))
            .send()
            .await;
        get_tick_stats.record(start.elapsed());
    }
    print_stats("GET /tick-size", &get_tick_stats);

    // GET /neg-risk
    let mut get_neg_risk_stats = LatencyStats::new();
    for _ in 0..5 {
        let start = Instant::now();
        let _ = client
            .get(format!(
                "https://clob.polymarket.com/neg-risk?token_id={}",
                sample_token
            ))
            .send()
            .await;
        get_neg_risk_stats.record(start.elapsed());
    }
    print_stats("GET /neg-risk", &get_neg_risk_stats);

    println!();

    // =========================================================================
    // 10. GAMMA API ENDPOINTS LATENCY (market metadata)
    // =========================================================================
    println!("⑩ GAMMA API ENDPOINTS LATENCY");
    println!("─────────────────────────────────────────────────────────────────────");

    // GET /events
    let mut get_events_stats = LatencyStats::new();
    for _ in 0..3 {
        let start = Instant::now();
        let _ = client
            .get("https://gamma-api.polymarket.com/events?limit=10&active=true")
            .send()
            .await;
        get_events_stats.record(start.elapsed());
    }
    print_stats("GET /events (10)", &get_events_stats);

    // GET /markets
    let mut get_markets_stats = LatencyStats::new();
    for _ in 0..3 {
        let start = Instant::now();
        let _ = client
            .get("https://gamma-api.polymarket.com/markets?limit=10&active=true")
            .send()
            .await;
        get_markets_stats.record(start.elapsed());
    }
    print_stats("GET /markets (10)", &get_markets_stats);

    println!();

    // =========================================================================
    // 11. PARALLEL ORDERBOOK FETCHING
    // =========================================================================
    println!("⑪ PARALLEL ORDERBOOK FETCHING");
    println!("─────────────────────────────────────────────────────────────────────");

    // 5x parallel orderbooks (like Python benchmark)
    let mut parallel_5_stats = LatencyStats::new();
    let tokens_5: Vec<_> = sample_tokens.iter().take(5).collect();

    for _ in 0..5 {
        let start = Instant::now();
        let futures: Vec<_> = tokens_5
            .iter()
            .map(|token| {
                let client = client.clone();
                let token = token.to_string();
                async move {
                    client
                        .get(format!(
                            "https://clob.polymarket.com/book?token_id={}",
                            token
                        ))
                        .send()
                        .await
                }
            })
            .collect();

        let _ = futures_util::future::join_all(futures).await;
        parallel_5_stats.record(start.elapsed());
    }
    print_stats("5x Parallel GET /book", &parallel_5_stats);
    println!(
        "  Per orderbook (parallel): {}",
        format_duration(parallel_5_stats.mean() / 5)
    );

    // 10x parallel
    let mut parallel_10_stats = LatencyStats::new();
    let tokens_10_par: Vec<_> = sample_tokens.iter().take(10).collect();

    for _ in 0..3 {
        let start = Instant::now();
        let futures: Vec<_> = tokens_10_par
            .iter()
            .map(|token| {
                let client = client.clone();
                let token = token.to_string();
                async move {
                    client
                        .get(format!(
                            "https://clob.polymarket.com/book?token_id={}",
                            token
                        ))
                        .send()
                        .await
                }
            })
            .collect();

        let _ = futures_util::future::join_all(futures).await;
        parallel_10_stats.record(start.elapsed());
    }
    print_stats("10x Parallel GET /book", &parallel_10_stats);
    println!(
        "  Per orderbook (parallel): {}",
        format_duration(parallel_10_stats.mean() / 10)
    );

    // 20x parallel
    let mut parallel_20_stats = LatencyStats::new();
    let tokens_20: Vec<_> = sample_tokens.iter().take(20).collect();

    for _ in 0..3 {
        let start = Instant::now();
        let futures: Vec<_> = tokens_20
            .iter()
            .map(|token| {
                let client = client.clone();
                let token = token.to_string();
                async move {
                    client
                        .get(format!(
                            "https://clob.polymarket.com/book?token_id={}",
                            token
                        ))
                        .send()
                        .await
                }
            })
            .collect();

        let _ = futures_util::future::join_all(futures).await;
        parallel_20_stats.record(start.elapsed());
    }
    print_stats("20x Parallel GET /book", &parallel_20_stats);
    println!(
        "  Per orderbook (parallel): {}",
        format_duration(parallel_20_stats.mean() / 20)
    );

    println!();

    // =========================================================================
    // 12. RPC LATENCY (Polygon)
    // =========================================================================
    println!("⑫ RPC LATENCY (Polygon)");
    println!("─────────────────────────────────────────────────────────────────────");

    let rpc_url =
        std::env::var("POLYGON_RPC_URL").unwrap_or_else(|_| "https://polygon-rpc.com".to_string());

    println!(
        "  RPC URL: {}",
        if rpc_url.contains("alchemy") {
            "Alchemy (private)"
        } else {
            &rpc_url
        }
    );

    // eth_blockNumber
    let mut block_number_stats = LatencyStats::new();
    for _ in 0..5 {
        let start = Instant::now();
        let _ = client
            .post(&rpc_url)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "eth_blockNumber",
                "params": [],
                "id": 1
            }))
            .send()
            .await;
        block_number_stats.record(start.elapsed());
    }
    print_stats("eth_blockNumber", &block_number_stats);

    // eth_chainId
    let mut chain_id_stats = LatencyStats::new();
    for _ in 0..5 {
        let start = Instant::now();
        let _ = client
            .post(&rpc_url)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "eth_chainId",
                "params": [],
                "id": 1
            }))
            .send()
            .await;
        chain_id_stats.record(start.elapsed());
    }
    print_stats("eth_chainId", &chain_id_stats);

    // eth_gasPrice
    let mut gas_price_stats = LatencyStats::new();
    for _ in 0..5 {
        let start = Instant::now();
        let _ = client
            .post(&rpc_url)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "eth_gasPrice",
                "params": [],
                "id": 1
            }))
            .send()
            .await;
        gas_price_stats.record(start.elapsed());
    }
    print_stats("eth_gasPrice", &gas_price_stats);

    // eth_getBalance (sample address)
    let mut balance_stats = LatencyStats::new();
    for _ in 0..5 {
        let start = Instant::now();
        let _ = client
            .post(&rpc_url)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "eth_getBalance",
                "params": ["0x0000000000000000000000000000000000000000", "latest"],
                "id": 1
            }))
            .send()
            .await;
        balance_stats.record(start.elapsed());
    }
    print_stats("eth_getBalance", &balance_stats);

    println!();

    // =========================================================================
    // 13. CRYPTO SIGNING (ECDSA)
    // =========================================================================
    println!("⑬ CRYPTO SIGNING (ECDSA secp256k1)");
    println!("─────────────────────────────────────────────────────────────────────");

    use k256::ecdsa::{signature::Signer, SigningKey};
    use rand::rngs::OsRng;

    // Generate a random signing key
    let signing_key = SigningKey::random(&mut OsRng);

    // Create a sample message (like an order)
    let sample_order = serde_json::json!({
        "salt": "123456789",
        "maker": "0x1234567890abcdef1234567890abcdef12345678",
        "signer": "0x1234567890abcdef1234567890abcdef12345678",
        "taker": "0x0000000000000000000000000000000000000000",
        "tokenId": "12345678901234567890",
        "makerAmount": "10000000",
        "takerAmount": "5000000",
        "expiration": "0",
        "nonce": "0",
        "feeRateBps": "0",
        "side": "BUY",
        "signatureType": 0
    });

    // Hash the message (simulating EIP-712 style hashing)
    let mut hash_stats = LatencyStats::new();
    for _ in 0..1000 {
        let start = Instant::now();
        let mut hasher = Sha256::new();
        hasher.update(sample_order.to_string().as_bytes());
        let _hash = hasher.finalize();
        hash_stats.record(start.elapsed());
    }
    print_stats("SHA256 hash (1000x)", &hash_stats);

    // ECDSA signing
    let mut sign_stats = LatencyStats::new();
    let message_hash: [u8; 32] = Sha256::digest(sample_order.to_string().as_bytes()).into();

    for _ in 0..100 {
        let start = Instant::now();
        let _signature: k256::ecdsa::Signature = signing_key.sign(&message_hash);
        sign_stats.record(start.elapsed());
    }
    print_stats("ECDSA sign (100x)", &sign_stats);

    // Full order signing simulation (hash + sign)
    let mut full_sign_stats = LatencyStats::new();
    for _ in 0..100 {
        let start = Instant::now();

        // 1. Serialize order
        let order_json = sample_order.to_string();

        // 2. Hash
        let hash: [u8; 32] = Sha256::digest(order_json.as_bytes()).into();

        // 3. Sign
        let _signature: k256::ecdsa::Signature = signing_key.sign(&hash);

        full_sign_stats.record(start.elapsed());
    }
    print_stats("Full order sign (100x)", &full_sign_stats);

    // Simulate signing K orders (K=3)
    let mut sign_k3_stats = LatencyStats::new();
    for _ in 0..50 {
        let start = Instant::now();

        for _ in 0..3 {
            let order_json = sample_order.to_string();
            let hash: [u8; 32] = Sha256::digest(order_json.as_bytes()).into();
            let _signature: k256::ecdsa::Signature = signing_key.sign(&hash);
        }

        sign_k3_stats.record(start.elapsed());
    }
    print_stats("Sign K=3 orders (50x)", &sign_k3_stats);

    // Simulate signing K orders (K=5)
    let mut sign_k5_stats = LatencyStats::new();
    for _ in 0..50 {
        let start = Instant::now();

        for _ in 0..5 {
            let order_json = sample_order.to_string();
            let hash: [u8; 32] = Sha256::digest(order_json.as_bytes()).into();
            let _signature: k256::ecdsa::Signature = signing_key.sign(&hash);
        }

        sign_k5_stats.record(start.elapsed());
    }
    print_stats("Sign K=5 orders (50x)", &sign_k5_stats);

    println!();

    // =========================================================================
    // 14. ESTIMATED TOTAL EXECUTION TIME
    // =========================================================================
    println!("⑭ ESTIMATED TOTAL EXECUTION TIME");
    println!("─────────────────────────────────────────────────────────────────────");

    let detection_time = e2e_stats.mean();
    let precheck_time = parallel_5_stats.mean();
    let sign_time = sign_k3_stats.mean();
    let post_order_estimate = Duration::from_millis(200); // Estimated
    let fill_wait_estimate = Duration::from_millis(1000); // Estimated
    let convert_tx_estimate = Duration::from_millis(3000); // Estimated (2 blocks)
    let sell_sign_time = sign_k3_stats.mean();
    let sell_post_estimate = Duration::from_millis(200);

    let total_estimate = detection_time
        + precheck_time
        + sign_time
        + post_order_estimate
        + fill_wait_estimate
        + convert_tx_estimate
        + sell_sign_time
        + sell_post_estimate;

    println!(
        "  Detection (scanner):        {}",
        format_duration(detection_time)
    );
    println!(
        "  Pre-check orderbooks:       {}",
        format_duration(precheck_time)
    );
    println!(
        "  Sign K=3 orders:            {}",
        format_duration(sign_time)
    );
    println!(
        "  POST orders (estimated):    {}",
        format_duration(post_order_estimate)
    );
    println!(
        "  Wait for fills (estimated): {}",
        format_duration(fill_wait_estimate)
    );
    println!(
        "  CONVERT tx (estimated):     {}",
        format_duration(convert_tx_estimate)
    );
    println!(
        "  Sign sell orders:           {}",
        format_duration(sell_sign_time)
    );
    println!(
        "  POST sell orders:           {}",
        format_duration(sell_post_estimate)
    );
    println!("  ─────────────────────────────────────────────");
    println!(
        "  TOTAL ESTIMATED:            {}",
        format_duration(total_estimate)
    );

    println!();

    // =========================================================================
    // FINAL SUMMARY
    // =========================================================================
    println!("╔══════════════════════════════════════════════════════════════════╗");
    println!("║                    COMPLETE LATENCY SUMMARY                      ║");
    println!("╠══════════════════════════════════════════════════════════════════╣");
    println!("║  SCANNER PROCESSING (CPU-BOUND)                                  ║");
    println!("╠══════════════════════════════════════════════════════════════════╣");
    println!(
        "║  Quick check (avg):             {:>10}                       ║",
        format_duration(quick_check_stats.mean())
    );
    println!(
        "║  Full VWAP analysis (avg):      {:>10}                       ║",
        format_duration(analysis_stats.mean())
    );
    println!(
        "║  End-to-end detection (avg):    {:>10}                       ║",
        format_duration(e2e_stats.mean())
    );
    println!(
        "║  Throughput:               {:>10} markets/sec                ║",
        format!("{:>5.0}", markets_per_sec)
    );
    println!("╠══════════════════════════════════════════════════════════════════╣");
    println!("║  CLOB API (NETWORK-BOUND)                                        ║");
    println!("╠══════════════════════════════════════════════════════════════════╣");
    println!(
        "║  GET /book (single):            {:>10}                       ║",
        format_duration(get_book_stats.mean())
    );
    println!(
        "║  POST /books (50 tokens):       {:>10}                       ║",
        format_duration(post_books_50_stats.mean())
    );
    println!(
        "║  5x Parallel GET /book:         {:>10}                       ║",
        format_duration(parallel_5_stats.mean())
    );
    println!(
        "║  GET /price:                    {:>10}                       ║",
        format_duration(get_price_stats.mean())
    );
    println!("╠══════════════════════════════════════════════════════════════════╣");
    println!("║  RPC (POLYGON)                                                   ║");
    println!("╠══════════════════════════════════════════════════════════════════╣");
    println!(
        "║  eth_blockNumber:               {:>10}                       ║",
        format_duration(block_number_stats.mean())
    );
    println!(
        "║  eth_gasPrice:                  {:>10}                       ║",
        format_duration(gas_price_stats.mean())
    );
    println!("╠══════════════════════════════════════════════════════════════════╣");
    println!("║  CRYPTO SIGNING                                                  ║");
    println!("╠══════════════════════════════════════════════════════════════════╣");
    println!(
        "║  ECDSA sign (single):           {:>10}                       ║",
        format_duration(sign_stats.mean())
    );
    println!(
        "║  Sign K=3 orders:               {:>10}                       ║",
        format_duration(sign_k3_stats.mean())
    );
    println!(
        "║  Sign K=5 orders:               {:>10}                       ║",
        format_duration(sign_k5_stats.mean())
    );
    println!("╠══════════════════════════════════════════════════════════════════╣");
    println!("║  TOTAL ESTIMATED EXECUTION                                       ║");
    println!("╠══════════════════════════════════════════════════════════════════╣");
    println!(
        "║  Full trade cycle:              {:>10}                       ║",
        format_duration(total_estimate)
    );
    println!("╚══════════════════════════════════════════════════════════════════╝");

    // =========================================================================
    // COMPARISON WITH PYTHON BENCHMARK
    // =========================================================================
    println!();
    println!("╔══════════════════════════════════════════════════════════════════╗");
    println!("║              RUST vs PYTHON BENCHMARK COMPARISON                 ║");
    println!("╠══════════════════════════════════════════════════════════════════╣");
    println!("║  OPERATION              │ RUST         │ PYTHON       │ WINNER  ║");
    println!("╠══════════════════════════════════════════════════════════════════╣");

    // Quick check comparison (Python: 9.1μs)
    let rust_quick = quick_check_stats.mean().as_nanos() as f64 / 1000.0;
    let python_quick = 9.1;
    let quick_winner = if rust_quick < python_quick {
        "RUST"
    } else {
        "Python"
    };
    println!(
        "║  Quick check            │ {:>8.1}μs   │ {:>8.1}μs   │ {:>6}  ║",
        rust_quick, python_quick, quick_winner
    );

    // Full analysis (Python: 1.4μs - suspicious)
    let rust_analysis = analysis_stats.mean().as_nanos() as f64 / 1000.0;
    let python_analysis = 1.4;
    println!(
        "║  Full VWAP analysis     │ {:>8.1}μs   │ {:>8.1}μs*  │ *short  ║",
        rust_analysis, python_analysis
    );

    // E2E detection (Python: 10.6μs - suspicious)
    let rust_e2e = e2e_stats.mean().as_nanos() as f64 / 1000.0;
    let python_e2e = 10.6;
    println!(
        "║  End-to-end detection   │ {:>8.1}μs   │ {:>8.1}μs*  │ *short  ║",
        rust_e2e, python_e2e
    );

    // Throughput (Python: 47,439/sec - suspicious)
    let python_throughput = 47439.0;
    println!(
        "║  Throughput             │ {:>7.0}/sec  │ {:>7.0}/sec* │ *short  ║",
        markets_per_sec, python_throughput
    );

    println!("╠══════════════════════════════════════════════════════════════════╣");

    // Parallel orderbooks (Python: 39.2ms for 5x)
    let rust_parallel = parallel_5_stats.mean().as_millis() as f64;
    let python_parallel = 39.2;
    let parallel_winner = if rust_parallel < python_parallel {
        "RUST"
    } else {
        "Python"
    };
    println!(
        "║  5x Parallel orderbooks │ {:>8.1}ms   │ {:>8.1}ms   │ {:>6}  ║",
        rust_parallel, python_parallel, parallel_winner
    );

    // Order signing (Python: 83.9ms)
    let rust_sign = sign_k3_stats.mean().as_micros() as f64 / 1000.0;
    let python_sign = 83.9;
    let sign_winner = if rust_sign < python_sign {
        "RUST"
    } else {
        "Python"
    };
    println!(
        "║  Order signing (K=3)    │ {:>8.2}ms   │ {:>8.1}ms   │ {:>6}  ║",
        rust_sign, python_sign, sign_winner
    );

    // RPC (Python: 42ms)
    let rust_rpc = block_number_stats.mean().as_millis() as f64;
    let python_rpc = 42.0;
    let rpc_winner = if rust_rpc < python_rpc {
        "RUST"
    } else {
        "~TIE"
    };
    println!(
        "║  RPC eth_blockNumber    │ {:>8.1}ms   │ {:>8.1}ms   │ {:>6}  ║",
        rust_rpc, python_rpc, rpc_winner
    );

    println!("╠══════════════════════════════════════════════════════════════════╣");
    println!("║  * Python results suspicious - likely short-circuiting           ║");
    println!("╚══════════════════════════════════════════════════════════════════╝");

    Ok(())
}
