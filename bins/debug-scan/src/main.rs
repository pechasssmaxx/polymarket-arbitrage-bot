//! Debug scanner - check why no opportunities found

use moonbag_analyzer::{quick_check, MoonbagAnalyzer};
use moonbag_core::models::StoredMarket;
use rust_decimal::Decimal;
use std::collections::HashSet;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load markets
    let content = tokio::fs::read_to_string("moonbag/markets.json").await?;
    let markets: Vec<StoredMarket> = serde_json::from_str(&content)?;

    println!("Loaded {} markets", markets.len());

    // Check each market with fake "ideal" orderbooks
    let analyzer = MoonbagAnalyzer {
        min_profit: Decimal::ZERO,
        min_moonbags: 0,
        min_k: 2,
        max_k: None,
        ..Default::default()
    };

    let mut opportunities_count = 0;
    let mut quick_pass_count = 0;

    for stored in &markets {
        let mut market = stored.to_market_state();

        // Simulate orderbook with fake depth at theoretical prices
        // If sum(NO_ask) < K-1, there's an opportunity
        for outcome in market.outcomes.values_mut() {
            // Add fake liquidity: NO ask at 0.40, YES bid at 0.01
            outcome.no_asks = vec![(Decimal::new(40, 2), Decimal::new(1000, 0))];
            outcome.yes_bids = vec![(Decimal::new(1, 2), Decimal::new(1000, 0))];
        }

        // Check convertible_indices
        let convertible_set: Option<HashSet<u8>> = if market.convertible_indices.is_empty() {
            None
        } else {
            Some(market.convertible_indices.iter().copied().collect())
        };

        let eligible_count = if let Some(ref set) = convertible_set {
            market
                .outcomes
                .values()
                .filter(|o| set.contains(&o.convert_index))
                .count()
        } else {
            market.outcomes.len()
        };

        // Quick check with fake data
        if quick_check(
            &market,
            Decimal::new(10, 0),
            Decimal::ZERO, // min_profit_per_share
            2,
            None,
            Decimal::new(5, 3),
        ) {
            quick_pass_count += 1;

            // Full analysis
            if let Some(opp) = analyzer.find_opportunity(&market, Decimal::new(10, 0)) {
                opportunities_count += 1;
                if opportunities_count <= 5 {
                    println!("\n=== OPPORTUNITY: {} ===", opp.market_slug);
                    println!(
                        "  K={}/{}, profit=${:.4}",
                        opp.k, opp.m, opp.guaranteed_profit
                    );
                    println!("  NO cost: ${:.4}", opp.no_cost);
                    println!("  USDC return: ${:.4}", opp.usdc_return);
                    println!("  YES revenue: ${:.4}", opp.yes_revenue);
                    println!("  Gas cost: ${:.4}", opp.gas_cost);
                    println!("  Moonbags: {}", opp.moonbag_count);
                    println!("  convertible_indices: {:?}", market.convertible_indices);
                    println!("  eligible outcomes: {}", eligible_count);
                }
            }
        }
    }

    println!("\n=== SUMMARY (with fake 0.40 NO asks) ===");
    println!("Quick check pass: {}", quick_pass_count);
    println!("Opportunities found: {}", opportunities_count);

    // Now test with REAL scenario - need to bootstrap real orderbooks
    println!("\n\n=== NOW TESTING WITH REAL ORDERBOOK DATA ===");

    // Bootstrap real orderbooks
    use reqwest::Client;
    use serde::{Deserialize, Serialize};

    #[derive(Serialize)]
    struct BookRequest {
        token_id: String,
    }

    #[derive(Deserialize, Debug)]
    #[allow(dead_code)]
    struct BookResponse {
        asset_id: Option<String>,
        bids: Option<Vec<BookLevel>>,
        asks: Option<Vec<BookLevel>>,
    }

    #[derive(Deserialize, Debug)]
    struct BookLevel {
        price: Option<String>,
        size: Option<String>,
    }

    let client = Client::new();

    // Pick first 3 markets with >5 outcomes
    let test_markets: Vec<_> = markets
        .iter()
        .filter(|m| m.outcomes.len() >= 5)
        .take(3)
        .collect();

    for stored in test_markets {
        println!(
            "\n--- Testing: {} ({} outcomes) ---",
            stored.title,
            stored.outcomes.len()
        );

        let mut market = stored.to_market_state();

        // Fetch real NO orderbooks
        let no_tokens: Vec<String> = stored
            .outcomes
            .iter()
            .map(|o| o.no_token_id.clone())
            .collect();

        let body: Vec<BookRequest> = no_tokens
            .iter()
            .map(|t| BookRequest {
                token_id: t.clone(),
            })
            .collect();

        let resp = client
            .post("https://clob.polymarket.com/books")
            .json(&body)
            .send()
            .await?;

        let books: Vec<BookResponse> = resp.json().await?;

        // Update market with real data
        for (i, book) in books.iter().enumerate() {
            if i >= stored.outcomes.len() {
                break;
            }

            let outcome_key = &stored.outcomes[i].yes_token_id;
            if let Some(outcome) = market.outcomes.get_mut(outcome_key) {
                // Parse asks
                let asks: Vec<(Decimal, Decimal)> = book
                    .asks
                    .as_ref()
                    .map(|levels| {
                        levels
                            .iter()
                            .filter_map(|l| {
                                let price = l.price.as_ref()?.parse::<Decimal>().ok()?;
                                let size = l.size.as_ref()?.parse::<Decimal>().ok()?;
                                Some((price, size))
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                outcome.no_asks = asks;

                // Also set some fake YES bids for testing
                outcome.yes_bids = vec![(Decimal::new(1, 2), Decimal::new(100, 0))];
            }
        }

        // Print orderbook summary
        let mut no_prices: Vec<Decimal> = Vec::new();
        for (_key, outcome) in &market.outcomes {
            let best_no = outcome
                .no_asks
                .first()
                .map(|l| l.0)
                .unwrap_or(Decimal::new(999, 0));
            let depth: Decimal = outcome.no_asks.iter().map(|l| l.1).sum();
            if best_no < Decimal::ONE {
                no_prices.push(best_no);
            }
            if outcome.no_asks.is_empty() {
                println!(
                    "  {} - NO ASKS (no liquidity)",
                    &outcome.question[..30.min(outcome.question.len())]
                );
            } else {
                println!(
                    "  {} - NO ask: {:.3}, depth: {:.0}",
                    &outcome.question[..30.min(outcome.question.len())],
                    best_no,
                    depth
                );
            }
        }

        no_prices.sort();
        if no_prices.len() >= 2 {
            let k2_cost: Decimal = no_prices[..2].iter().sum();
            let k3_cost: Decimal = if no_prices.len() >= 3 {
                no_prices[..3].iter().sum()
            } else {
                Decimal::new(999, 0)
            };
            println!(
                "\n  K=2 sum of cheapest 2 NO: {:.4} (need < 1.0 for profit)",
                k2_cost
            );
            println!(
                "  K=3 sum of cheapest 3 NO: {:.4} (need < 2.0 for profit)",
                k3_cost
            );

            // Check quick_check
            let passes = quick_check(
                &market,
                Decimal::new(10, 0),
                Decimal::ZERO,
                2,
                None,
                Decimal::new(5, 3),
            );
            println!("  Quick check: {}", if passes { "PASS" } else { "FAIL" });

            // Try analyzer
            for size in [Decimal::new(1, 0), Decimal::new(5, 0), Decimal::new(10, 0)] {
                if let Some(opp) = analyzer.find_opportunity(&market, size) {
                    println!(
                        "  Found opp at size {}: profit=${:.4}",
                        size, opp.guaranteed_profit
                    );
                } else {
                    println!("  No opp at size {}", size);
                }
            }
        }
    }

    Ok(())
}
