# Polymarket Sum-Arb Bot (Conservative)

A Rust bot that scans Polymarket's 5/15-minute crypto markets (BTC/ETH/SOL)
for sum-arbitrage opportunities — buying both YES and NO when their combined
ask price is below $0.97, locking in a guaranteed ~3%+ profit on resolution.

## ⚠️ IMPORTANT: Read This First

**This is a real trading bot that will spend real money.** Even though it's
capped at $0.50 per side ($1.00 total per trade), understand these risks:

1. **Fill risk**: If the YES order fills but the NO order doesn't (someone
   grabs the liquidity first), you're stuck with a directional bet. The bot
   warns you loudly when this happens.

2. **Fee erosion**: Polymarket charges taker fees on crypto markets (~1.5-2%).
   The 3% threshold is set above this, but fees vary by price level.

3. **Latency**: Professional arb bots run on VPS nodes in Amsterdam with
   sub-1ms latency. On a $5/mo VPS, you'll be slower and miss tight edges.

4. **Edge frequency**: 3%+ edges on 5/15-min markets are uncommon. The bot
   may run for hours without finding one. This is by design — we're only
   taking high-confidence trades.

**Realistic expectation**: This bot might make $0.10-$0.50/day on a good day,
or nothing for several days. It's a conservative lottery ticket, not a money
printer.

## Prerequisites

1. **Rust** (1.75+ recommended): https://rustup.rs
2. **Polymarket account** with funded wallet (USDC.e on Polygon)
3. **Authentication credentials** — either:
   - **Option A**: Private key exported from MetaMask (for self-custody wallets)
   - **Option B**: Builder API keys from Polymarket Settings > Relayer API Keys (for programmatic trading)
4. **Token allowances** set — the first time you use the SDK, you need to
   approve the exchange contracts. See the official SDK examples:
   https://github.com/Polymarket/rs-clob-client/blob/main/examples/approvals.rs

## Setup

### Paper Trading Mode (Safe - No real money)

```bash
# Run in paper trading mode to test without risking real funds
cd /Users/ahmedm/Desktop/poly-rust
PAPER_TRADING=1 RUST_LOG=info ./target/release/poly-arb-bot
```

### Live Trading Mode

**Option 1: Private Key Authentication**
```bash
export POLYMARKET_PRIVATE_KEY="0xYOUR_PRIVATE_KEY_HERE"
RUST_LOG=info ./target/release/poly-arb-bot
```

**Option 2: Builder API Key Authentication**
Your API credentials are stored in `.env`:
- `POLY_API_KEY`: 019d3e20-f1a8-7dc5-b444-05186bf6e3c4
- `POLY_API_SECRET`: (stored in .env)
- `POLY_API_PASSPHRASE`: (stored in .env)

To use API keys for live trading:
```bash
source .env
unset PAPER_TRADING
RUST_LOG=info ./target/release/poly-arb-bot
```

## Configuration

Edit the constants at the top of `src/main.rs`:

| Constant | Default | What it does |
|----------|---------|--------------|
| `MAX_SUM_THRESHOLD` | `0.97` | Only arb when YES+NO ask sum ≤ this |
| `MAX_SIZE_PER_SIDE` | `0.50` | Max USDC per side ($1.00 total per trade) |
| `SCAN_INTERVAL_SECS` | `2` | Seconds between scans |
| `MIN_ASK_PRICE` | `0.05` | Ignore asks below this (dust) |
| `MAX_ASK_PRICE` | `0.95` | Ignore asks above this (no edge) |

**To increase aggression** (more trades, thinner edge):
- Raise `MAX_SUM_THRESHOLD` to `0.98` (but you'll get closer to fee breakeven)
- Raise `MAX_SIZE_PER_SIDE` to `2.00`

**To decrease aggression** (fewer trades, safer):
- Lower `MAX_SUM_THRESHOLD` to `0.96`
- Lower `MAX_SIZE_PER_SIDE` to `0.25`

## VPS Deployment

For best results, run on a VPS close to Polymarket's infrastructure:

```bash
# Example: DigitalOcean $5/mo droplet (Amsterdam recommended)
# SSH in, install Rust, clone, build, run in tmux/screen

# Using tmux to keep it running after disconnect:
tmux new -s polybot
export POLYMARKET_PRIVATE_KEY="0x..."
cargo run --release
# Ctrl+B then D to detach
# tmux attach -t polybot to reattach
```

## How It Works

```
Every 2 seconds:
  │
  ├─ Fetch active markets from Gamma API
  ├─ Filter to 5/15-min BTC/ETH/SOL markets
  │
  For each matching market:
  │
  ├─ Fetch orderbook for YES token
  ├─ Fetch orderbook for NO token
  ├─ Calculate: sum = best_ask(YES) + best_ask(NO)
  │
  ├─ If sum > $0.97 → skip (no edge)
  ├─ If sum ≤ $0.97 → TRADE:
  │   ├─ Buy $0.50 of YES at ask (Fill-or-Kill)
  │   ├─ If YES fills → Buy $0.50 of NO at ask (FOK)
  │   └─ On resolution: one side pays $0.50, total cost was < $0.97
  │       → profit ≈ $0.03+ per $1.00 risked
  │
  └─ Log stats every 60 seconds
```

## Troubleshooting

| Error | Fix |
|-------|-----|
| "Failed to authenticate" | Check private key format (must start with 0x) |
| "insufficient allowance" | Run the approvals example from the SDK |
| "insufficient balance" | Fund your wallet with more USDC.e on Polygon |
| "order rejected" | Market may have closed or price moved — normal |
| YES fills but NO doesn't | You have a directional position. Sell on Polymarket UI or wait. |

## Architecture

```
poly-arb-bot/
├── Cargo.toml          # Dependencies (official polymarket-client-sdk)
├── src/
│   └── main.rs         # Everything in one file — scan, filter, trade
└── README.md
```

This intentionally lives in a single file. For a $10 lottery ticket, you don't
need a microservice architecture.

## License

Do whatever you want with this. It's your $10.
