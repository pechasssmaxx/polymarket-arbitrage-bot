# Moonbag Scanner

Real-time scanner for Polymarket moonbag arbitrage opportunities.

## Quick Start

```bash
cd polymarket-moonbag-rs

# Dry run (no trades, just monitoring)
MOONBAG_MIN_PROFIT=0.00 \
MOONBAG_DRY_RUN=1 \
./target/release/moonbag-scanner.exe --markets moonbag/markets.json --log-level info
```

## CLI Arguments

| Argument | Default | Description |
|----------|---------|-------------|
| `--markets` | required | Path to markets.json file |
| `--dry-run` | `true` | No writes to queue |
| `--log-level` | `info` | Log level: debug, info, warn, error |
| `--scan-interval-ms` | `5000` | Periodic full scan interval (ms) |
| `--rest-only` | `false` | Skip WebSocket, use REST polling only |
| `--skip-bootstrap` | `false` | Skip initial orderbook bootstrap |

## Environment Variables

### Scanner Settings

| Variable | Default | Description |
|----------|---------|-------------|
| `MOONBAG_MIN_PROFIT` | `0.05` | Minimum profit threshold (USD) |
| `MOONBAG_TARGET_SIZE` | `10.0` | Target size per outcome (tokens) |
| `MOONBAG_MAX_INVESTMENT` | `300` | Maximum investment per opportunity (USD) |
| `MOONBAG_MIN_K` | `2` | Minimum K (selected outcomes) |
| `MOONBAG_MAX_K` | unlimited | Maximum K (selected outcomes) |
| `MOONBAG_MAX_REMAINING_OUTCOMES` | unlimited | Max remaining outcomes filter |
| `MOONBAG_DRY_RUN` | `1` | Dry run mode (1=on, 0=off) |

### Telegram Deduplication (Optional)

| Variable | Default | Description |
|----------|---------|-------------|
| `MOONBAG_TELEGRAM_DEDUP_COOLDOWN_SECS` | `60` | Cooldown between duplicate alerts |
| `MOONBAG_TELEGRAM_DEDUP_MIN_PROFIT_DELTA` | `0.01` | Min profit change to resend alert |

### Telegram Notifications (Optional)

| Variable | Description |
|----------|-------------|
| `TELEGRAM_BOT_TOKEN` | Bot token from @BotFather |
| `TELEGRAM_CHAT_ID` | Chat ID for notifications |

**Note:** If not set, Telegram is disabled (no error).

### Gas Settings

| Variable | Default | Description |
|----------|---------|-------------|
| `MOONBAG_GAS_PRICE_GWEI` | `35` | Gas price in gwei |
| `MOONBAG_POL_USD` | `0.50` | POL/MATIC price in USD |

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                     moonbag-scanner                         │
├─────────────────────────────────────────────────────────────┤
│  1. Load markets from markets.json                          │
│  2. Bootstrap orderbooks via REST POST /books               │
│  3. Bootstrap prices via REST GET /price                    │
│  4. Connect to WebSocket for real-time updates              │
│  5. Process updates, trigger opportunity scans              │
│  6. Log/notify found opportunities                          │
└─────────────────────────────────────────────────────────────┘
```

## Opportunity Detection

The scanner finds K-of-M moonbag opportunities:
- Buy NO tokens on K outcomes where price < (K-1)/K breakeven
- CONVERT to get (K-1) USDC back + YES moonbags on remaining outcomes
- Profit = USDC return + sellable YES - NO cost - gas

### Quick Check Filter

Before full VWAP analysis, a fast filter checks:
- Sum of cheapest K NO prices < K (theoretical max)
- At least one outcome has YES bid > 0 (sellable)

## Output Examples

```
INFO OPPORTUNITY: market-slug | K=3/5 | profit=$2.50 | cost=$180.00 | moonbags=2
```

Detailed opportunity fields:
- `K/M`: Selected outcomes / Total outcomes
- `profit`: Guaranteed profit after gas
- `cost`: Total NO cost
- `moonbags`: Number of YES tokens kept (not sold)

## Modes

### WebSocket Mode (Default)
- Connects to `wss://ws-subscriptions-clob.polymarket.com/ws/market`
- Real-time orderbook updates
- Periodic full scans as backup (5s default)

### REST-Only Mode (`--rest-only`)
- Polls orderbooks via REST API
- Minimum 2s interval to avoid rate limits
- Use when WebSocket is unreliable

## Example Commands

```bash
# Minimal dry run
./target/release/moonbag-scanner.exe --markets moonbag/markets.json

# Production settings (still dry run)
MOONBAG_MIN_PROFIT=0.50 \
MOONBAG_MAX_INVESTMENT=500 \
MOONBAG_DRY_RUN=1 \
./target/release/moonbag-scanner.exe --markets moonbag/markets.json --log-level info

# REST-only mode with debug logging
./target/release/moonbag-scanner.exe \
  --markets moonbag/markets.json \
  --rest-only \
  --scan-interval-ms 10000 \
  --log-level debug

# With Telegram notifications
TELEGRAM_BOT_TOKEN=your_token \
TELEGRAM_CHAT_ID=your_chat_id \
MOONBAG_TELEGRAM_DEDUP_COOLDOWN_SECS=60 \
./target/release/moonbag-scanner.exe --markets moonbag/markets.json
```

## markets.json Format

```json
[
  {
    "neg_risk_market_id": "0x...",
    "title": "Market Title",
    "slug": "market-slug",
    "outcomes": [
      {
        "question": "Outcome question",
        "condition_id": "0x...",
        "yes_token_id": "12345...",
        "no_token_id": "67890...",
        "alphabetical_index": 0
      }
    ],
    "convertible_indices": [0, 1, 2]
  }
]
```

## Stats Output (every 30s)

```
INFO Stats book_updates=1234 opportunities=5 quick_pass=100 quick_fail=1500
```

- `book_updates`: WebSocket orderbook updates received
- `opportunities`: Total opportunities found
- `quick_pass`: Markets passing quick filter
- `quick_fail`: Markets failing quick filter (skipped)
