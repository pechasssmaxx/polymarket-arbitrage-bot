
# Polymarket Moonbag Scanner

An arbitrage bot for Polymarket multi-outcome (NegRisk) markets. It scans order books in real-time, identifies profitable opportunities, and sends alerts via Telegram.

---

## Overview

Polymarket features markets with multiple mutually exclusive outcomes. Examples include:
* **Politics**: "Who will win the presidential election?" (10+ candidates).
* **Crypto/Finance**: "What will Token X's FDV be?" (Options: <$100M, <$200M, <$500M, etc.).

Each outcome has **YES** and **NO** tokens. 
* **YES** = The outcome happens.
* **NO** = The outcome does not happen.

**The Arbitrage Opportunity:** These markets use a `CONVERT` mechanism via the `NegRiskAdapter`. If you hold **NO** tokens for every outcome in a set, you can convert them back into **USDC** plus receive **YES** tokens for free. This bot automates the search for price imbalances that make this conversion profitable.

---

## How it Works (The "Moonbag" Strategy)

Imagine a market: "FOGO FDV after launch" with 5 options:
- >$50M, >$100M, >$200M, >$500M, >$1B

The bot detects that **NO** tokens for 3 specific options are undervalued:
- **NO** on ">$500M" = $0.60
- **NO** on ">$1B" = $0.55
- **NO** on ">$200M" = $0.70

**Execution Steps:**
1. **Buy**: The bot buys NO tokens for these 3 outcomes ($K=3$).
2. **Convert**: Uses `NegRiskAdapter` to convert them $\rightarrow$ receives **(K-1) = 2 USDC** back for every share.
3. **Bonus**: You receive **YES** tokens for all 3 outcomes for free—these are your **"Moonbags"**.

**Math Example (per 10 shares):**
* **Cost**: $(0.60 \times 10) + (0.55 \times 10) + (0.70 \times 10) = 18.50 \text{ USDC}$
* **Returns**: $(3-1) \times 10 = 20.00 \text{ USDC}$
* **Guaranteed Profit**: $20.00 - 18.50 - \text{gas} \approx 1.30 \text{ USDC}$

*Additionally, you hold the YES tokens. If the FDV actually exceeds $500M, that YES token becomes worth $1. Since you got it for free via arbitrage, it's pure upside.*

---

## The Formula

$$\text{profit} = (K - 1) \times \text{amount} - \sum (\text{NO\_price}[i] \times \text{amount}) - \text{gas}$$

**Break-even point**: Average NO price must be $< \frac{K-1}{K}$

| K (Outcomes Purchased) | Max Avg. NO Price | Example |
| :--- | :--- | :--- |
| 2 | $0.50 (50¢) | Two NOs at ~$0.49 each |
| 3 | $0.667 (66.7¢) | Three NOs averaging ~$0.65 |
| 5 | $0.80 (80¢) | Five NOs averaging ~$0.78 |
| 10 | $0.90 (90¢) | Ten NOs averaging ~$0.89 |

*Higher $K$ makes finding a profitable combination easier but requires more capital.*

---

## Core Features

1. **Real-time Scanning**: Connects to Polymarket WebSockets for instant order book updates.
2. **Market Discovery**: Refreshes active markets via Gamma API every 5 minutes.
3. **Microsecond Filtering**: Efficiently discards non-profitable markets instantly.
4. **Slippage Awareness**: Calculates real execution prices based on order book depth, not just the best ask.
5. **Optimization Engine**: Iterates through outcome combinations to find the highest profit margin.
6. **Telegram Integration**: Sends detailed alerts including market names, outcomes, and projected profit.

*By default, the bot runs in `DRY_RUN=1` mode (Scan & Alert only).*

---

## Quick Start

### 1. Installation
Requires **Rust**. If you don't have it: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`

```bash
git clone [https://github.com/pechasssmaxx/polymarket-arbitrage-bot.git](https://github.com/pechasssmaxx/polymarket-arbitrage-bot.git)
cd polymarket-arbitrage-bot
cargo build --release

### 2. Configuration


cp .env.example .env

Minimal Setup (Monitoring Only):


# Polygon RPC Node (Alchemy or Infura)
POLYGON_RPC_URL=[https://polygon-mainnet.g.alchemy.com/v2/YOUR_KEY](https://polygon-mainnet.g.alchemy.com/v2/YOUR_KEY)

# Simulation Mode
MOONBAG_DRY_RUN=1

# Telegram Alerts
TELEGRAM_BOT_TOKEN=123456:ABC-DEF...
TELEGRAM_CHAT_ID=-100123456789

### 3. Running the Bot

# Start Scanner (logs to console)
RUST_LOG=info ./target/release/moonbag-scanner

# Run Scanner + Telegram Notifier (two processes)
./target/release/moonbag-scanner --no-telegram &
./target/release/moonbag-notifier &

# Health Check
curl http://localhost:8080/health

---

## CLI Flags

### Scanner Flags (`moonbag-scanner`)

| Flag | Description |
| --- | --- |
| --dry-run | Only detect opportunities, do not trade (Default: `true`). |
| --execute | Enables live trading upon detecting an opportunity. |
| --once | Testing: Finds one opportunity, executes/logs it, and exits. |
| --no-telegram | Disables Telegram alerts (useful if running a separate notifier process). |
| --rest-only | Disables WebSockets; uses REST polling (debugging). |
| --scan-interval-ms | Scan frequency in milliseconds (Default: `5000`). |

---

## Environment Variables

| Variable | Default | Description |
| --- | --- | --- |
| MOONBAG_MIN_PROFIT | 0.00 | Minimum USDC profit required to trigger an alert. |
| MOONBAG_MAX_INVESTMENT | 3000 | Maximum USDC to deploy per trade. |
| MOONBAG_MIN_K | 2 | Minimum number of outcomes to include in a set. |
| MOONBAG_TARGET_SIZE | 10.0 | Target position size in shares. |
| MOONBAG_MODE | profit | profit for max ROI, farm for max volume. |

---

## Project Structure

* moonbag-analyzer/: Core logic for filtering and opportunity detection.
* moonbag-scanner/: Hardware for WebSockets, Order Books, and Gamma API.
* moonbag-executor/: Execution logic for CLOB API and NegRisk conversions.
* moonbag-notifier/: Telegram alert service.
* scripts/: Python utilities for manual relaying and retries.

---

## FAQ

Q: Is this legal? A: Yes. This is standard market arbitrage. You are buying tokens at market price and utilizing a publicly available smart contract function.

Q: How much can I earn? A: It depends on market volatility. Opportunities appear more frequently during high-volume events. It is a tool for discovery, not a guaranteed "money printer."

Q: What are the risks? A: Gas costs on Polygon, execution slippage, and potential software bugs. Always start with DRY_RUN=1 to verify logic.

---

## License

MIT. Use at your own risk. This software is not financial advice.

