// Polymarket BTC 5m/15m "Up or Down" bot — TUI edition
//
// Price feed : Chainlink BTC/USD aggregator on Polygon Mainnet
//              (same oracle Polymarket uses for resolution)
// Strategy   : compare current Chainlink price to previous tick
//              → bullish (+) → buy "Up" token
//              → bearish (−) → buy "Down" token
// Edge filter: skip if the favoured token's mid-price ≥ EDGE_THRESHOLD
//
// Controls   : [q] quit   [r] force refresh now
//
// To go live:
//   1. set PAPER_MODE = false
//   2. export POLYMARKET_PRIVATE_KEY=0x…
//   3. fill in the "live order path" TODO below

use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::Utc;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, List, ListItem, Paragraph, Wrap},
    Frame, Terminal,
};
use reqwest::Client as HttpClient;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::Deserialize;
use tokio::sync::{mpsc, Mutex};

use polymarket_client_sdk::clob::types::request::{MidpointRequest, OrderBookSummaryRequest};
use polymarket_client_sdk::clob::Client as ClobClient;
use polymarket_client_sdk::gamma::types::request::EventsRequest;
use polymarket_client_sdk::gamma::Client as GammaClient;

// ─── Config ──────────────────────────────────────────────────────────────────

/// Flip to `false` only when your wallet is funded and you have tested in paper mode.
const PAPER_MODE: bool = true;

/// Only buy a token priced BELOW this. 0.58 = market is uncertain (near 50/50).
/// Lower = better risk/reward. At 0.55 entry: upside $0.45, downside $0.55.
/// At 0.92 entry (old): upside $0.08, downside $0.92 — terrible R/R.
const EDGE_THRESHOLD: Decimal = dec!(0.58);

/// Starting paper bankroll in USDC.
const STARTING_BANKROLL: Decimal = dec!(10);

/// Fraction of current bankroll to bet per trade.
const BET_FRACTION: Decimal = dec!(0.15);

/// Don't place a trade if bankroll drops below this.
const MIN_BET: Decimal = dec!(0.10);

/// How often to poll for new market windows (seconds).
const POLL_SECS: u64 = 5;

/// How often the dedicated exit task fires (milliseconds).
const EXIT_INTERVAL_MS: u64 = 500;

/// Minimum order-book imbalance to allow a trade.
const OB_IMBALANCE_MIN: f64 = 0.20;


/// Force-exit any open position this many seconds before market expiry.
/// Exits BEFORE the binary result locks in — we never want to hold to resolution.
const FORCE_EXIT_SECS: i64 = 30;

/// Don't enter a position if the favoured token is priced below this.
const MIN_ENTRY_PRICE: Decimal = dec!(0.15);

/// Take profit: exit when mid moves this many cents in our favour.
const TAKE_PROFIT: Decimal = dec!(0.06);

/// Stop loss: exit when mid moves this many cents against us.
/// Only activated in the final SL_WINDOW_SECS — before that we hold through noise.
const STOP_LOSS: Decimal = dec!(0.05);

/// Minimum BTC move WITHIN the current 5m window to justify a trade.
/// Tick-to-tick noise is ±$5; a real in-window move is $20+.
const MIN_WINDOW_DELTA: f64 = 20.0;

/// Only activate stop-loss this many seconds before market close.
/// Gives the trade time to recover from noise before cutting it.
const SL_WINDOW_SECS: i64 = 120;

/// Chainlink BTC/USD aggregator on Polygon Mainnet — same oracle Polymarket uses.
const CHAINLINK_BTC_USD: &str = "0xc907E116054Ad103354f2D350FD2514433D57F6f";
/// Public Polygon RPC — no API key needed.
/// drpc.org is the primary; 1rpc.io/matic is the fallback (both verified working).
const POLYGON_RPC: &str = "https://polygon.drpc.org";

// ─── App state ───────────────────────────────────────────────────────────────

struct MarketRow {
    question: String,
    up_mid: Option<Decimal>,
    down_mid: Option<Decimal>,
    /// "UP" | "DOWN" | "" — which side the signal favours
    favoured: &'static str,
    /// whether a (paper) trade was placed this tick
    traded: bool,
    /// order-book imbalance for the favoured token: bid_vol / (bid_vol + ask_vol)
    /// None = data unavailable (treated as neutral 0.5)
    fav_imbalance: Option<f64>,
}

#[derive(Clone)]
struct OpenPosition {
    entered_at: String,
    slug: String,
    entry_price: Decimal,
    /// shares = bet_size / entry_price
    shares: Decimal,
    end_time: chrono::DateTime<chrono::Utc>,
    /// U256 token ID as decimal string — used to query last-trade-price for resolution
    fav_token_id: String,
}

#[derive(Clone)]
enum TradeResult {
    Open,
    Won,
    Lost,
}

#[derive(Clone)]
struct TradeEntry {
    time: String,
    side: &'static str,
    market: String,
    mid: Decimal,
    shares: Decimal,
    result: TradeResult,
    slug: String,
}

struct AppState {
    btc_price: Option<f64>,
    prev_btc_price: Option<f64>,
    /// BTC price at the start of each 5m window, keyed by event slug.
    /// Used to compute true in-window momentum rather than noisy tick-to-tick delta.
    window_btc_opens: std::collections::HashMap<String, f64>,
    markets: Vec<MarketRow>,
    trades: Vec<TradeEntry>,
    open_positions: Vec<OpenPosition>,
    bankroll: Decimal,
    total_pnl: Decimal,
    wins: u32,
    losses: u32,
    status: String,
    next_tick_secs: u64,
    last_update: String,
    fetching: bool,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            btc_price: None,
            prev_btc_price: None,
            window_btc_opens: std::collections::HashMap::new(),
            markets: Vec::new(),
            trades: Vec::new(),
            open_positions: Vec::new(),
            bankroll: STARTING_BANKROLL,
            total_pnl: Decimal::ZERO,
            wins: 0,
            losses: 0,
            status: "Initialising…".to_string(),
            next_tick_secs: POLL_SECS,
            last_update: "—".to_string(),
            fetching: false,
        }
    }
}

// ─── Chainlink price ─────────────────────────────────────────────────────────

/// Read `latestRoundData()` from the Chainlink BTC/USD aggregator via Polygon RPC.
/// Returns the BTC price in USD.
async fn chainlink_btc_usd(http: &HttpClient) -> Result<f64> {
    #[derive(Deserialize)]
    struct RpcResp {
        result: String,
    }

    let resp: RpcResp = http
        .post(POLYGON_RPC)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_call",
            "params": [
                { "to": CHAINLINK_BTC_USD, "data": "0xfeaf968c" },
                "latest"
            ],
            "id": 1
        }))
        .send()
        .await?
        .json()
        .await?;

    // ABI response layout (5 × 32 bytes):
    //   bytes  0-31  → roundId (uint80)
    //   bytes 32-63  → answer  (int256)  ← price × 1e8
    //   bytes 64-95  → startedAt
    //   bytes 96-127 → updatedAt
    //   bytes 128-159 → answeredInRound
    let hex = resp.result.trim_start_matches("0x");
    if hex.len() < 128 {
        anyhow::bail!("Short Chainlink result ({}): {hex}", hex.len());
    }
    let answer_hex = &hex[64..128];
    // BTC price never negative; u128 comfortably holds any real-world value
    let raw = u128::from_str_radix(answer_hex, 16)
        .map_err(|e| anyhow::anyhow!("Chainlink parse: {e}"))?;

    Ok(raw as f64 / 1e8)
}

// ─── Resolution checker ──────────────────────────────────────────────────────

/// For each open position whose end_time has passed, check resolution via
/// CLOB last-trade-price (Gamma API outcomePrices never updates for fast markets).
/// Winning token last-trades near 1.0, loser near 0.0.
async fn check_resolutions(http: &HttpClient, state: &Arc<Mutex<AppState>>) {
    let now = chrono::Utc::now();
    let positions: Vec<OpenPosition> = {
        let s = state.lock().await;
        s.open_positions.iter()
            .filter(|p| p.end_time <= now)
            .cloned()
            .collect()
    };

    for pos in positions {
        // Use CLOB last-trade-price — this correctly reflects on-chain resolution
        let url = format!(
            "https://clob.polymarket.com/last-trade-price?token_id={}",
            pos.fav_token_id
        );
        #[derive(serde::Deserialize)]
        struct LastPrice { price: String }
        let last: LastPrice = match http.get(&url).send().await
            .and_then(|r| r.error_for_status())
        {
            Ok(resp) => match resp.json().await { Ok(p) => p, Err(_) => continue },
            Err(_) => continue,
        };
        let price: f64 = match last.price.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        // Only settle when clearly resolved (< 0.05 = lost, > 0.95 = won)
        if price >= 0.05 && price <= 0.95 { continue; }

        let won = price > 0.95;

        let payout = if won { pos.shares } else { Decimal::ZERO };
        let pnl    = payout - pos.entry_price * pos.shares; // net vs cost basis

        {
            let mut s = state.lock().await;
            s.bankroll += payout;
            s.total_pnl += pnl;
            if won { s.wins += 1; } else { s.losses += 1; }
            // Update the matching trade entry result
            for t in s.trades.iter_mut() {
                if t.slug == pos.slug && t.time == pos.entered_at {
                    t.result = if won { TradeResult::Won } else { TradeResult::Lost };
                    break;
                }
            }
            s.open_positions.retain(|p| !(p.slug == pos.slug && p.entered_at == pos.entered_at));
        }
    }
}

// ─── Mid-market exit (take-profit / stop-loss) ───────────────────────────────

/// Every tick, check each open position's current mid-price.
/// If it moved ≥ TAKE_PROFIT in our favour → sell now, bank the gain.
/// If it moved ≥ STOP_LOSS against us     → sell now, cut the loss.
/// This is what makes the bot "fast money" instead of hold-to-resolution.
async fn check_exits(http: &HttpClient, state: &Arc<Mutex<AppState>>) {
    let positions: Vec<OpenPosition> = {
        let s = state.lock().await;
        s.open_positions.clone()
    };

    for pos in &positions {
        #[derive(serde::Deserialize)]
        struct MidResp { mid: String }
        let url = format!(
            "https://clob.polymarket.com/midpoint?token_id={}",
            pos.fav_token_id
        );
        let mid: Decimal = match http.get(&url).send().await {
            Ok(resp) => match resp.json::<MidResp>().await {
                Ok(r) => match r.mid.parse::<Decimal>() {
                    Ok(m) => m,
                    Err(e) => {
                        state.lock().await.status = format!("exit parse err: {e}");
                        continue;
                    }
                },
                Err(e) => {
                    state.lock().await.status = format!("exit json err: {e}");
                    continue;
                }
            },
            Err(e) => {
                state.lock().await.status = format!("exit http err: {e}");
                continue;
            }
        };

        let move_ = mid - pos.entry_price;
        let secs_left = (pos.end_time - chrono::Utc::now()).num_seconds();
        let force_exit = secs_left <= FORCE_EXIT_SECS;
        // SL only activates in the last SL_WINDOW_SECS — before that hold through noise.
        // A position down 2¢ at minute 1 may be up 4¢ a few seconds later.
        let sl_active = secs_left <= SL_WINDOW_SECS;

        // Hold if: TP not hit AND (SL not active OR SL not triggered) AND not expiring
        if !force_exit && move_ < TAKE_PROFIT && (!sl_active || move_ > -STOP_LOSS) {
            continue;
        }

        // Exit at current mid-price (paper: credit bankroll at mid * shares)
        let payout = mid * pos.shares;
        let cost   = pos.entry_price * pos.shares;
        let pnl    = payout - cost;
        let won    = pnl >= Decimal::ZERO;

        let mut s = state.lock().await;
        s.bankroll   += payout;
        s.total_pnl  += pnl;
        if won { s.wins += 1; } else { s.losses += 1; }
        for t in s.trades.iter_mut() {
            if t.slug == pos.slug && t.time == pos.entered_at {
                t.result = if won { TradeResult::Won } else { TradeResult::Lost };
                break;
            }
        }
        s.open_positions.retain(|p| {
            !(p.slug == pos.slug && p.entered_at == pos.entered_at)
        });
        let reason = if force_exit { "EXPIRY" } else if move_ >= TAKE_PROFIT { "TP" } else { "SL(2m)" };
        s.status = format!(
            "[{}] entry={:.3} → {:.3}  {}",
            reason,
            pos.entry_price,
            mid,
            if won { format!("+${:.3}", pnl) } else { format!("-${:.3}", pnl.abs()) },
        );
    }
}

// ─── Bot tick (runs every POLL_SECS) ─────────────────────────────────────────

async fn run_tick(http: &HttpClient, gamma: &GammaClient, clob: &ClobClient, state: &Arc<Mutex<AppState>>) {
    // Mark as fetching
    state.lock().await.fetching = true;

    // 1. Chainlink BTC price
    let btc_now = match chainlink_btc_usd(http).await {
        Ok(p) => p,
        Err(e) => {
            let mut s = state.lock().await;
            s.status = format!("Chainlink error: {e}");
            s.fetching = false;
            return;
        }
    };

    let delta = {
        let s = state.lock().await;
        let prev = s.btc_price.unwrap_or(btc_now);
        let d = btc_now - prev;
        drop(s);
        d
    };

    {
        let mut s = state.lock().await;
        s.prev_btc_price = s.btc_price;
        s.btc_price = Some(btc_now);
        s.last_update = Utc::now().format("%H:%M:%S").to_string();
    }

    // 2. Discover active BTC 5m/15m markets via the events endpoint.
    //    These markets live under slugs like btc-updown-5m-<unix> and btc-updown-15m-<unix>.
    //    They are created ~24h before their resolution window, so we MUST filter by
    //    end_date_min=now (only future markets) and order by endDate ASC (soonest first).
    //    Ordering by startDate misses them because they were created yesterday.
    let now = chrono::Utc::now();
    let raw_events = match gamma
        .events(
            &EventsRequest::builder()
                .closed(false)
                .active(true)
                .order(vec!["endDate".to_string()])
                .ascending(true)
                .end_date_min(now)
                .limit(500)
                .build(),
        )
        .await
    {
        Ok(e) => e,
        Err(e) => {
            let mut s = state.lock().await;
            s.status = format!("Gamma API: {e}");
            s.fetching = false;
            return;
        }
    };

    let mut market_rows: Vec<MarketRow> = Vec::new();
    let mut new_trades: Vec<TradeEntry> = Vec::new();
    let mut new_positions: Vec<OpenPosition> = Vec::new();
    let mut bankroll_debit = Decimal::ZERO;

    // Snapshot bankroll + open market slugs + window BTC opens.
    // Block by SLUG (not token ID) so we never enter both sides of the same market.
    let (current_bankroll, open_slugs, mut window_btc_opens) = {
        let s = state.lock().await;
        let slugs: std::collections::HashSet<String> =
            s.open_positions.iter().map(|p| p.slug.clone()).collect();
        (s.bankroll, slugs, s.window_btc_opens.clone())
    };
    let bet_size = (current_bankroll * BET_FRACTION).max(Decimal::ZERO);

    // The slug encodes the window-start Unix timestamp: btc-updown-5m-<unix>.
    // We ONLY trade when current time is INSIDE the resolution window:
    //   window_start <= now < window_end - FORCE_EXIT_SECS
    // This is the core of the strategy — enter during the actual 5m move,
    // force-exit before the binary result locks in.
    let now_unix = now.timestamp();

    let mut btc_markets: Vec<_> = raw_events
        .iter()
        .filter(|e| {
            let slug = e.slug.as_deref().unwrap_or("");
            slug.starts_with("btc-updown-5m-") || slug.starts_with("btc-updown-15m-")
        })
        .filter_map(|e| {
            let slug = e.slug.as_deref().unwrap_or("");
            let market = e.markets.as_ref()?.first()?;
            // Parse window start from slug suffix
            let window_start: i64 = slug.rsplit('-').next()?.parse().ok()?;
            Some((window_start, market))
        })
        .filter(|(window_start, m)| {
            if let Some(end) = m.end_date {
                let secs_left = (end - now).num_seconds();
                let secs_in_window = now_unix - window_start;
                // Must be inside the window AND have enough time to exit cleanly
                secs_in_window >= 0 && secs_left >= FORCE_EXIT_SECS
            } else {
                false
            }
        })
        .map(|(_, m)| m)
        .collect();

    // Sort by end_time ascending → pick the soonest-expiring active window
    btc_markets.sort_by_key(|m| m.end_date.unwrap_or(now));

    // Only trade the single best market this tick (soonest to resolve)
    let btc_markets = &btc_markets[..btc_markets.len().min(1)];

    for market in btc_markets
        .iter()
    {
        let q = market.question.as_deref().unwrap_or("unknown").to_string();

        // token_ids[0] = Up/Yes, [1] = Down/No (Polymarket convention)
        let token_ids = match &market.clob_token_ids {
            Some(ids) if ids.len() == 2 => ids,
            _ => continue,
        };

        let slug = market.slug.clone().unwrap_or_default();
        let already_open = open_slugs.contains(&slug);
        let end_time = market.end_date.unwrap_or_else(chrono::Utc::now);

        // --- In-window delta ---
        // Record BTC price the first time we see this slug within its window.
        // window_delta = total BTC move since THIS window opened.
        // Real signal: tick-to-tick is noise, in-window sustained trend is edge.
        let window_open = *window_btc_opens.entry(slug.clone()).or_insert(btc_now);
        let window_delta = btc_now - window_open;
        let bullish = window_delta >= 0.0;

        // Fetch mid-prices + order book for the window-directional token in parallel
        let fav_token = if bullish { token_ids[0] } else { token_ids[1] };
        let up_req   = MidpointRequest::builder().token_id(token_ids[0]).build();
        let down_req = MidpointRequest::builder().token_id(token_ids[1]).build();
        let ob_req   = OrderBookSummaryRequest::builder().token_id(fav_token).build();
        let (up_res, down_res, ob_res) = tokio::join!(
            clob.midpoint(&up_req),
            clob.midpoint(&down_req),
            clob.order_book(&ob_req),
        );

        let up_mid   = up_res.ok().map(|r| r.mid);
        let down_mid = down_res.ok().map(|r| r.mid);

        // Order-book imbalance: sum top-5 bid vs ask sizes
        let fav_imbalance: Option<f64> = ob_res.ok().map(|book| {
            let bid_vol: f64 = book.bids.iter().take(5)
                .map(|b| f64::try_from(b.size).unwrap_or(0.0))
                .sum();
            let ask_vol: f64 = book.asks.iter().take(5)
                .map(|a| f64::try_from(a.size).unwrap_or(0.0))
                .sum();
            bid_vol / (bid_vol + ask_vol + 1e-9)
        });

        let (fav_mid, favoured): (Option<Decimal>, &'static str) = if bullish {
            (up_mid, "UP")
        } else {
            (down_mid, "DOWN")
        };

        // Trade when:
        //   1. mid-price is below EDGE_THRESHOLD
        //   2. order-book is not actively fighting our signal (imbalance ≥ OB_IMBALANCE_MIN)
        //   3. in-window BTC delta is large enough to be a real trend, not noise
        let ob_ok = fav_imbalance.map_or(true, |imb| imb >= OB_IMBALANCE_MIN);
        let delta_ok = window_delta.abs() >= MIN_WINDOW_DELTA;

        let traded = if let Some(mid) = fav_mid {
            if mid >= MIN_ENTRY_PRICE && mid < EDGE_THRESHOLD && ob_ok && delta_ok && bet_size >= MIN_BET && !already_open {
                // shares = outcome tokens received if we win (bet_size / price)
                let shares = if mid > Decimal::ZERO { bet_size / mid } else { Decimal::ZERO };
                let now_str = Utc::now().format("%H:%M:%S").to_string();

                new_positions.push(OpenPosition {
                    entered_at: now_str.clone(),
                    slug: slug.clone(),
                    entry_price: mid,
                    shares,
                    end_time,
                    fav_token_id: fav_token.to_string(),
                });
                new_trades.push(TradeEntry {
                    time: now_str,
                    side: favoured,
                    market: q.clone(),
                    mid,
                    shares,
                    result: TradeResult::Open,
                    slug: slug.clone(),
                });
                bankroll_debit += bet_size;
                true
            } else {
                false
            }
        } else {
            false
        };

        market_rows.push(MarketRow { question: q, up_mid, down_mid, favoured, traded, fav_imbalance });
    }

    // Update state
    {
        let mut s = state.lock().await;
        s.markets = market_rows;
        // Persist updated window open prices for future ticks
        s.window_btc_opens = window_btc_opens;
        // Debit bankroll for new entries
        s.bankroll -= bankroll_debit;
        // Track open positions
        s.open_positions.extend(new_positions);
        // Prepend new trades (newest first)
        new_trades.extend(std::mem::take(&mut s.trades));
        new_trades.truncate(30);
        s.trades = new_trades;
        s.status = format!(
            "BTC ${:.2}  Δtick={:+.2}",
            btc_now,
            delta
        );
        s.fetching = false;
    }
}

// ─── TUI rendering ───────────────────────────────────────────────────────────

fn render(f: &mut Frame, s: &AppState) {
    let area = f.area();
    let chunks = Layout::vertical([
        Constraint::Length(3),   // header
        Constraint::Min(8),      // markets + signal
        Constraint::Length(12),  // trades
        Constraint::Length(3),   // footer
    ])
    .split(area);

    // ── Header ───────────────────────────────────────────────────────────────
    let mode_str = if PAPER_MODE { "PAPER MODE" } else { "⚠ LIVE MODE ⚠" };
    let mode_color = if PAPER_MODE { Color::Yellow } else { Color::Red };
    let btc_str = s.btc_price.map_or("loading…".into(), |p| format!("${:.2}", p));
    let fetch_indicator = if s.fetching { " ⟳" } else { "" };

    let pnl_color = if s.total_pnl >= Decimal::ZERO { Color::Green } else { Color::Red };
    let pnl_str   = format!("{:+.2}", s.total_pnl);
    let win_rate  = if s.wins + s.losses > 0 {
        format!("{}W/{}L ({:.0}%)", s.wins, s.losses,
            s.wins as f64 / (s.wins + s.losses) as f64 * 100.0)
    } else {
        "0W/0L".to_string()
    };

    let header_line = Line::from(vec![
        Span::raw("  Polymarket BTC 5m/15m  "),
        Span::styled(mode_str, Style::default().fg(mode_color).add_modifier(Modifier::BOLD)),
        Span::raw(format!("   BTC: {}   updated: {}{}   ", btc_str, s.last_update, fetch_indicator)),
        Span::raw(format!("Bankroll: ${:.2}  P&L: ", s.bankroll)),
        Span::styled(format!("${}", pnl_str), Style::default().fg(pnl_color).add_modifier(Modifier::BOLD)),
        Span::raw(format!("  {}  open: {}", win_rate, s.open_positions.len())),
    ]);
    f.render_widget(
        Paragraph::new(header_line).block(Block::bordered()),
        chunks[0],
    );

    // ── Body: markets (left) + signal (right) ────────────────────────────────
    let body = Layout::horizontal([Constraint::Percentage(75), Constraint::Percentage(25)])
        .split(chunks[1]);

    // Markets list
    let market_items: Vec<ListItem> = if s.markets.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            if s.fetching {
                " Fetching markets…".to_string()
            } else {
                format!(" Waiting for next 5m window to open…  ({})", s.last_update)
            },
            Style::default().fg(Color::DarkGray),
        )))]
    } else {
        s.markets
            .iter()
            .flat_map(|m| {
                let title = ListItem::new(Line::from(Span::styled(
                    format!(" {}", m.question),
                    Style::default().add_modifier(Modifier::BOLD),
                )));

                fn mid_str(v: Option<Decimal>) -> String {
                    v.map_or("—".into(), |p| format!("{:.3}", p))
                }

                let up_fav   = m.favoured == "UP";
                let down_fav = m.favoured == "DOWN";

                // Show imbalance next to the favoured side
                let imb_str = m.fav_imbalance.map_or("".into(), |v| format!("  ob={:.0}%", v * 100.0));
                let ob_warn = m.fav_imbalance.map_or(false, |v| v < OB_IMBALANCE_MIN);

                let up_line = ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("   Up   {}", mid_str(m.up_mid)),
                        if up_fav { Style::default().fg(Color::Green) } else { Style::default() },
                    ),
                    if up_fav {
                        if m.traded {
                            Span::styled("  ✓ traded", Style::default().fg(Color::Cyan))
                        } else if ob_warn {
                            Span::styled(format!("  ← signal{} OB!", imb_str), Style::default().fg(Color::Yellow))
                        } else {
                            Span::styled(format!("  ← signal{}", imb_str), Style::default().fg(Color::DarkGray))
                        }
                    } else {
                        Span::raw("")
                    },
                ]));

                let dn_line = ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("   Down {}", mid_str(m.down_mid)),
                        if down_fav { Style::default().fg(Color::Red) } else { Style::default() },
                    ),
                    if down_fav {
                        if m.traded {
                            Span::styled("  ✓ traded", Style::default().fg(Color::Cyan))
                        } else if ob_warn {
                            Span::styled(format!("  ← signal{} OB!", imb_str), Style::default().fg(Color::Yellow))
                        } else {
                            Span::styled(format!("  ← signal{}", imb_str), Style::default().fg(Color::DarkGray))
                        }
                    } else {
                        Span::raw("")
                    },
                ]));

                vec![title, up_line, dn_line, ListItem::new("")]
            })
            .collect()
    };

    f.render_widget(
        List::new(market_items).block(Block::bordered().title(" Markets ")),
        body[0],
    );

    // Signal panel
    let signal_lines = build_signal(s);
    f.render_widget(
        Paragraph::new(signal_lines)
            .block(Block::bordered().title(" Signal "))
            .wrap(Wrap { trim: false }),
        body[1],
    );

    // ── Recent trades ─────────────────────────────────────────────────────────
    let trade_items: Vec<ListItem> = if s.trades.is_empty() {
        vec![ListItem::new(Span::styled(
            " No paper trades yet",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        s.trades
            .iter()
            .take(9)
            .map(|t| {
                let side_color = if t.side == "UP" { Color::Green } else { Color::Red };
                let (result_str, result_color) = match t.result {
                    TradeResult::Open   => ("OPEN  ", Color::DarkGray),
                    TradeResult::Won    => ("WON ✓ ", Color::Cyan),
                    TradeResult::Lost   => ("LOST ✗", Color::Red),
                };
                let cost = t.mid * t.shares;
                let payout_str = match t.result {
                    TradeResult::Won  => format!(" +${:.2}", t.shares - cost),
                    TradeResult::Lost => format!(" -${:.2}", cost),
                    _ => String::new(),
                };
                ListItem::new(Line::from(vec![
                    Span::raw(format!(" {} ", t.time)),
                    Span::styled(format!("{:4}", t.side), Style::default().fg(side_color)),
                    Span::raw(format!(" {:.3} ", t.mid)),
                    Span::styled(result_str, Style::default().fg(result_color)),
                    Span::styled(format!("{:7}", payout_str), Style::default().fg(result_color)),
                    Span::raw(format!("  {}", t.market)),
                ]))
            })
            .collect()
    };

    f.render_widget(
        List::new(trade_items).block(Block::bordered().title(" Recent Paper Trades ")),
        chunks[2],
    );

    // ── Footer ────────────────────────────────────────────────────────────────
    let footer = format!(
        "  Next tick in {:2}s   [q] quit   [r] refresh now   Oracle: Chainlink BTC/USD · Polygon  bet={:.0}% of bankroll",
        s.next_tick_secs,
        BET_FRACTION * dec!(100),
    );
    f.render_widget(
        Paragraph::new(footer).block(Block::bordered()),
        chunks[3],
    );
}

fn build_signal(s: &AppState) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = vec![Line::from("")];

    let (btc, prev) = match (s.btc_price, s.prev_btc_price) {
        (Some(b), Some(p)) => (b, p),
        (Some(b), None) => {
            lines.push(Line::from(Span::raw(format!("  Waiting for second price sample…"))));
            lines.push(Line::from(Span::raw(format!("  BTC: ${:.2}", b))));
            return lines;
        }
        _ => {
            lines.push(Line::from(Span::styled(
                "  Fetching Chainlink price…",
                Style::default().fg(Color::DarkGray),
            )));
            return lines;
        }
    };

    let delta = btc - prev;
    let pct = if prev > 0.0 { delta / prev * 100.0 } else { 0.0 };
    let bull = delta >= 0.0;

    let (arrow, label, color) = if bull {
        ("▲", "BULLISH", Color::Green)
    } else {
        ("▼", "BEARISH", Color::Red)
    };

    lines.push(Line::from(Span::styled(
        format!("  {} {}  {:+.2} ({:+.4}%)", arrow, label, delta, pct),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("  Signal active  (Δ={:+.2})", delta),
        Style::default().fg(Color::Green),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::raw(format!("  Edge threshold : {} (mid-price)", EDGE_THRESHOLD))));
    lines.push(Line::from(Span::raw(format!("  OB filter      : ≥{:.0}% bid vol", OB_IMBALANCE_MIN * 100.0))));
    lines.push(Line::from(Span::raw(format!("  Bet size       : {:.0}% of bankroll", BET_FRACTION * dec!(100)))));
    lines.push(Line::from(Span::raw(format!("  Take profit    : +{} cents", TAKE_PROFIT))));
    lines.push(Line::from(Span::raw(format!("  Stop loss      : -{} cents", STOP_LOSS))));
    lines.push(Line::from(""));

    // Per-market edge + OB check
    for m in &s.markets {
        let fav_mid = if bull { m.up_mid } else { m.down_mid };
        if let Some(mid) = fav_mid {
            let price_ok = mid < EDGE_THRESHOLD;
            let ob_ok = m.fav_imbalance.map_or(true, |v| v >= OB_IMBALANCE_MIN);
            let tradeable = price_ok && ob_ok;

            let short_q = if m.question.len() > 18 {
                format!("{}…", &m.question[..18])
            } else {
                m.question.clone()
            };
            let imb_str = m.fav_imbalance.map_or("ob=?".into(), |v| format!("ob={:.0}%", v * 100.0));
            let reason = if !price_ok {
                format!("mid={:.3} ≥ {}", mid, EDGE_THRESHOLD)
            } else if !ob_ok {
                format!("mid={:.3}  {} blocked", mid, imb_str)
            } else {
                format!("mid={:.3}  {}", mid, imb_str)
            };

            lines.push(Line::from(Span::styled(
                format!("  {}  {}  {}", if tradeable { "✓" } else { "✗" }, short_q, reason),
                if tradeable {
                    Style::default().fg(Color::Cyan)
                } else if !price_ok {
                    Style::default().fg(Color::DarkGray)
                } else {
                    Style::default().fg(Color::Yellow) // OB blocked
                },
            )));
        }
    }

    if !s.status.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("  {}", s.status),
            Style::default().fg(Color::DarkGray),
        )));
    }

    lines
}

// ─── Main ────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    // Set up TUI — requires a real interactive terminal.
    // If stdin is not a TTY (e.g., piped/background process) we bail with a clear message.
    if !crossterm::tty::IsTty::is_tty(&io::stdin()) {
        eprintln!("Run this bot directly in a terminal: cargo run");
        std::process::exit(1);
    }

    // Restore terminal on panic so escape codes don't leak into the shell.
    std::panic::set_hook(Box::new(|info| {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            LeaveAlternateScreen,
            DisableMouseCapture,
            crossterm::cursor::Show,
        );
        eprintln!("panic: {info}");
    }));

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let state = Arc::new(Mutex::new(AppState::default()));
    let http = HttpClient::new();
    let gamma = GammaClient::default();
    let clob = ClobClient::default();

    // Channel: 'q' key → quit
    let (quit_tx, mut quit_rx) = mpsc::channel::<()>(1);
    // Channel: 'r' key → force refresh
    let (refresh_tx, mut refresh_rx) = mpsc::channel::<()>(1);

    // Blocking thread to read keyboard events without blocking the async runtime
    {
        let qt = quit_tx.clone();
        let rt = refresh_tx.clone();
        std::thread::spawn(move || loop {
            if let Ok(Event::Key(key)) = event::read() {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => {
                        let _ = qt.blocking_send(());
                        break;
                    }
                    KeyCode::Char('r') | KeyCode::Char('R') => {
                        let _ = rt.blocking_send(());
                    }
                    _ => {}
                }
            }
        });
    }

    // Spawn dedicated exit checker — fires every EXIT_INTERVAL_MS independent of market discovery.
    // This is what makes stop-loss and take-profit actually work in near-real-time.
    {
        let http_ex = http.clone();
        let state_ex = state.clone();
        tokio::spawn(async move {
            let mut timer = tokio::time::interval(
                Duration::from_millis(EXIT_INTERVAL_MS)
            );
            loop {
                timer.tick().await;
                check_exits(&http_ex, &state_ex).await;
            }
        });
    }

    // Run initial tick before the loop
    check_resolutions(&http, &state).await;
    run_tick(&http, &gamma, &clob, &state).await;

    let mut poll_timer = tokio::time::interval(Duration::from_secs(POLL_SECS));
    poll_timer.tick().await; // consume the immediate first tick (already ran above)

    let mut render_timer = tokio::time::interval(Duration::from_millis(200));
    let mut tick_start = Instant::now();

    'main: loop {
        // Update countdown
        {
            let mut s = state.lock().await;
            let elapsed = tick_start.elapsed().as_secs();
            s.next_tick_secs = POLL_SECS.saturating_sub(elapsed);
        }

        // Draw
        {
            let s = state.lock().await;
            terminal.draw(|f| render(f, &s))?;
        }

        tokio::select! {
            biased;

            _ = quit_rx.recv() => break 'main,

            _ = refresh_rx.recv() => {
                check_resolutions(&http, &state).await;
                run_tick(&http, &gamma, &clob, &state).await;
                tick_start = Instant::now();
                poll_timer.reset();
            }

            _ = poll_timer.tick() => {
                check_resolutions(&http, &state).await;
                run_tick(&http, &gamma, &clob, &state).await;
                tick_start = Instant::now();
            }

            _ = render_timer.tick() => {
                // just re-render for the countdown update — nothing else to do
            }
        }
    }

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    let total_trades = state.lock().await.trades.len();
    println!("Bot stopped. Paper trades this session: {total_trades}");

    Ok(())
}
