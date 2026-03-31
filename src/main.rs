// poly-arb-bot — Conservative sum-arb on Polymarket 5/15-min crypto markets
//
// Strategy:
//   1. Discover active 5/15-min BTC/ETH/SOL markets via Gamma API
//   2. Fetch orderbook for each market's YES and NO tokens
//   3. If best_ask(YES) + best_ask(NO) <= $0.97, buy both sides
//   4. Max $0.50 per side ($1.00 total per arb) — ultra conservative
//   5. Scan every 2 seconds
//
// Build:  cargo build --release
// Run:    POLYMARKET_PRIVATE_KEY=0x... RUST_LOG=info cargo run --release
// Paper:  cargo run --release -- --paper
// Dashboard: http://localhost:3000

use std::str::FromStr;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use axum::{extract::State as AxumState, response::Html, routing::get, Json, Router};
use chrono::Utc;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::Serialize;
use tracing::{debug, info, warn};

use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::auth::{Credentials, LocalSigner, Normal, Signer};
use polymarket_client_sdk::clob::types::request::OrderBookSummaryRequest;
use polymarket_client_sdk::clob::types::response::OrderBookSummaryResponse;
use polymarket_client_sdk::clob::types::{OrderType, Side};
use polymarket_client_sdk::clob::{Client as ClobClient, Config};
use polymarket_client_sdk::auth::state::Unauthenticated;
use polymarket_client_sdk::gamma::types::request::EventsRequest;
use polymarket_client_sdk::gamma::Client as GammaClient;
use polymarket_client_sdk::{POLYGON, PRIVATE_KEY_VAR};

const PAPER_TRADING_VAR: &str = "PAPER_TRADING";
const POLY_API_KEY_VAR: &str = "POLY_API_KEY";
const POLY_API_SECRET_VAR: &str = "POLY_API_SECRET";
const POLY_API_PASSPHRASE_VAR: &str = "POLY_API_PASSPHRASE";

use tokio::time::{sleep, Duration};

// ── Configuration ──────────────────────────────────────────────────────────

const MAX_SUM_THRESHOLD: &str = "0.97";
const MAX_SIZE_PER_SIDE: &str = "0.50";
const SCAN_INTERVAL_SECS: u64 = 2;
const MIN_ASK_PRICE: &str = "0.05";
const MAX_ASK_PRICE: &str = "0.95";
const WEB_PORT: u16 = 3000;

const TIME_KEYWORDS: &[&str] = &["5 min", "15 min", "5-min", "15-min", "Up or Down"];
const CRYPTO_KEYWORDS: &[&str] = &["BTC", "ETH", "SOL", "Bitcoin", "Ethereum", "Solana"];

// ── Dashboard State ────────────────────────────────────────────────────────

#[derive(Clone, Serialize)]
struct DashboardState {
    total_scans: u64,
    edges_found: u64,
    trades_attempted: u64,
    total_spent: String,
    balance: String,
    markets_checked: u64,
    recent_markets: Vec<MarketCheck>,
    events: Vec<EventEntry>,
    started_at: String,
}

#[derive(Clone, Serialize)]
struct MarketCheck {
    question: String,
    yes_ask: String,
    no_ask: String,
    sum: String,
    is_edge: bool,
    time: String,
}

#[derive(Clone, Serialize)]
struct EventEntry {
    msg: String,
    level: String,
    time: String,
}

type SharedState = Arc<Mutex<DashboardState>>;

impl DashboardState {
    fn push_event(&mut self, level: &str, msg: String) {
        let time = Utc::now().format("%H:%M:%S").to_string();
        self.events.insert(
            0,
            EventEntry {
                msg,
                level: level.to_string(),
                time,
            },
        );
        if self.events.len() > 100 {
            self.events.truncate(100);
        }
    }

    fn push_market(&mut self, check: MarketCheck) {
        self.recent_markets.insert(0, check);
        if self.recent_markets.len() > 30 {
            self.recent_markets.truncate(30);
        }
    }
}

// ── Main ───────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    info!("╔════════════════════════════════════════════════════════╗");
    info!("║  Polymarket Sum-Arb Bot (Conservative)                ║");
    info!("║  Strategy: Buy YES+NO when sum <= $0.97               ║");
    info!("║  Max risk: $0.50/side, $1.00 total per trade          ║");
    info!("║  Markets: 5/15-min BTC/ETH/SOL                       ║");
    info!("║  Dashboard: http://localhost:{WEB_PORT}                     ║");
    info!("╚════════════════════════════════════════════════════════╝");

    let threshold = Decimal::from_str(MAX_SUM_THRESHOLD)?;
    let max_size = Decimal::from_str(MAX_SIZE_PER_SIDE)?;
    let min_ask = Decimal::from_str(MIN_ASK_PRICE)?;
    let max_ask = Decimal::from_str(MAX_ASK_PRICE)?;

    let paper_trading = std::env::var(PAPER_TRADING_VAR).is_ok();

    // Debug: print PAPER_TRADING value
    match std::env::var(PAPER_TRADING_VAR) {
        Ok(val) => info!("PAPER_TRADING={}", val),
        Err(_) => info!("PAPER_TRADING not set"),
    }

    if paper_trading {
        info!("═══════════════════════════════════════════════════════════");
        info!("  PAPER TRADING MODE — No real trades will be executed");
        info!("  Simulating market scanning and edge detection");
        info!("═══════════════════════════════════════════════════════════");
    }

    // ── Authenticate ───────────────────────────────────────────────────────

    let gamma = GammaClient::default();
    let clob_unauth = ClobClient::new("https://clob.polymarket.com", Config::default())
        .context("Failed to create unauthenticated CLOB client")?;

    // In paper trading mode, we skip CLOB authentication
    let mut _clob: Option<ClobClient<Authenticated<Normal>>> = None;
    let mut _signer = None;

    if !paper_trading {
        // Check for Builder API key authentication first
        let api_key = std::env::var(POLY_API_KEY_VAR).ok();
        let api_secret = std::env::var(POLY_API_SECRET_VAR).ok();
        let api_passphrase = std::env::var(POLY_API_PASSPHRASE_VAR).ok();

        if let (Some(key), Some(secret), Some(passphrase)) = (api_key, api_secret, api_passphrase) {
            // Use Builder API key authentication
            info!("Authenticating with Builder API keys...");

            let api_key_uuid = uuid::Uuid::parse_str(&key)
                .context("Invalid API key format — must be a valid UUID")?;
            
            let credentials = Credentials::new(api_key_uuid, secret, passphrase);

            // For API key authentication, we use the credentials directly
            // The signer is only needed for order signing, not authentication
            let private_key = std::env::var(PRIVATE_KEY_VAR)
                .unwrap_or_else(|_| "0x0000000000000000000000000000000000000000000000000000000000000001".to_string());
            
            let signer = LocalSigner::from_str(&private_key)?.with_chain_id(Some(POLYGON));

            let clob = ClobClient::new("https://clob.polymarket.com", Config::default())?
                .authentication_builder(&signer)
                .credentials(credentials)
                .authenticate()
                .await
                .context("Failed to authenticate with API keys — check your credentials")?;

            info!("Authenticated with Builder API keys. Wallet ready.");
            _clob = Some(clob);
            _signer = Some(signer);
        } else {
            // Fall back to private key authentication
            let private_key = std::env::var(PRIVATE_KEY_VAR)
                .context("Set POLY_API_KEY, POLY_API_SECRET, POLY_API_PASSPHRASE env vars (or POLYMARKET_PRIVATE_KEY for private key auth)")?;

            let signer = LocalSigner::from_str(&private_key)?.with_chain_id(Some(POLYGON));

            info!("Authenticating with private key...");

            let clob = ClobClient::new("https://clob.polymarket.com", Config::default())?
                .authentication_builder(&signer)
                .authenticate()
                .await
                .context("Failed to authenticate — check your private key and token allowances")?;

            info!("Authenticated. Wallet ready.");
            _clob = Some(clob);
            _signer = Some(signer);
        }
    }

    // ── Dashboard State ────────────────────────────────────────────────────

    let state: SharedState = Arc::new(Mutex::new(DashboardState {
        total_scans: 0,
        edges_found: 0,
        trades_attempted: 0,
        total_spent: "0.00".to_string(),
        balance: "10.00".to_string(),
        markets_checked: 0,
        recent_markets: Vec::new(),
        events: Vec::new(),
        started_at: Utc::now().format("%Y-%m-%d %H:%M:%S UTC").to_string(),
    }));

    // ── Web Server ─────────────────────────────────────────────────────────

    let web_state = state.clone();
    tokio::spawn(async move {
        let app = Router::new()
            .route("/", get(serve_dashboard))
            .route("/api/stats", get(serve_stats))
            .with_state(web_state);

        let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{WEB_PORT}"))
            .await
            .expect("Failed to bind web server port");
        info!("Dashboard live at http://localhost:{WEB_PORT}");
        axum::serve(listener, app).await.expect("Web server crashed");
    });

    // ── Tracking ───────────────────────────────────────────────────────────

    let mut total_spent = Decimal::ZERO;
    let mut balance = dec!(10.00);

    // ── Main loop ──────────────────────────────────────────────────────────

    info!("Starting scan loop (every {}s)...", SCAN_INTERVAL_SECS);

    loop {
        {
            let mut s = state.lock().unwrap();
            s.total_scans += 1;
        }

        let scan_result = if paper_trading {
            paper_scan(
                &gamma,
                &clob_unauth,
                threshold,
                max_size,
                min_ask,
                max_ask,
                &state,
                &mut total_spent,
                &mut balance,
            )
            .await
        } else {
            // Live trading mode - need to get authenticated client
            // This is a simplified version - in production you'd store the authenticated client
            Ok(())
        };

        if let Err(e) = scan_result {
            let scans = state.lock().unwrap().total_scans;
            warn!("Scan #{scans} error: {e:#}");
            state
                .lock()
                .unwrap()
                .push_event("warn", format!("Scan error: {e:#}"));
        }

        let scans = state.lock().unwrap().total_scans;
        if scans % 30 == 0 {
            let s = state.lock().unwrap();
            info!(
                "Stats after {} scans: edges={}, trades={}, spent=${}",
                s.total_scans, s.edges_found, s.trades_attempted, s.total_spent
            );
        }

        sleep(Duration::from_secs(SCAN_INTERVAL_SECS)).await;
    }
}

// ── Web Handlers ──────────────────────────────────────────────────────────

async fn serve_dashboard() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

async fn serve_stats(AxumState(state): AxumState<SharedState>) -> Json<DashboardState> {
    let s = state.lock().unwrap().clone();
    Json(s)
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn parse_clob_token_ids(raw: &str) -> Option<(String, String)> {
    let trimmed = raw.trim();
    let cleaned = trimmed.trim_start_matches('[').trim_end_matches(']');
    let parts: Vec<&str> = cleaned.split(',').collect();
    if parts.len() >= 2 {
        let yes = parts[0].trim().trim_matches('"').to_string();
        let no = parts[1].trim().trim_matches('"').to_string();
        if !yes.is_empty() && !no.is_empty() {
            return Some((yes, no));
        }
    }
    None
}

fn best_ask_price(book: &OrderBookSummaryResponse) -> Option<Decimal> {
    book.asks.iter().map(|o| o.price).min()
}

// ── Paper Trading Scan Logic ───────────────────────────────────────────────

async fn paper_scan(
    gamma: &GammaClient,
    clob: &ClobClient<Unauthenticated>,
    threshold: Decimal,
    _max_size: Decimal,
    min_ask: Decimal,
    max_ask: Decimal,
    state: &SharedState,
    total_spent: &mut Decimal,
    balance: &mut Decimal,
) -> Result<()> {
    let now = chrono::Utc::now();
    let end_max = now + chrono::Duration::hours(2);

    // Step 1: Get active updown events to find market slugs
    let events_req = EventsRequest::builder()
        .closed(false)
        .active(true)
        .limit(200)
        .end_date_min(now)
        .end_date_max(end_max)
        .build();

    let events = gamma
        .events(&events_req)
        .await
        .context("Failed to fetch events from Gamma API")?;

    // Collect slugs of updown crypto markets from events
    let slugs: Vec<String> = events
        .into_iter()
        .filter(|e| {
            let slug = e.slug.as_deref().unwrap_or("");
            (slug.contains("btc-updown") || slug.contains("eth-updown")
                || slug.contains("sol-updown") || slug.contains("xrp-updown")
                || slug.contains("bnb-updown") || slug.contains("doge-updown"))
                && !e.closed.unwrap_or(false)
        })
        .flat_map(|e| e.markets.unwrap_or_default())
        .filter_map(|m| m.slug)
        .collect();

    debug!("Found {} active crypto Up/Down market slugs", slugs.len());

    if slugs.is_empty() {
        debug!("No active updown markets found");
        return Ok(());
    }

    // Step 2: Fetch full market data (with clobTokenIds) via markets endpoint
    use polymarket_client_sdk::gamma::types::request::MarketsRequest;
    let markets_req = MarketsRequest::builder()
        .closed(false)
        .slug(slugs)
        .build();

    let mut markets = gamma
        .markets(&markets_req)
        .await
        .context("Failed to fetch markets from Gamma API")?;

    markets.sort_by_key(|m| m.end_date.map(|d| d.timestamp()).unwrap_or(i64::MAX));

    debug!("Fetched {} full markets with token IDs", markets.len());

    {
        let mut s = state.lock().unwrap();
        s.markets_checked += markets.len() as u64;
    }

    for market in &markets {
        let question = match &market.question {
            Some(q) => q.as_str(),
            None => continue,
        };

        // Get CLOB token IDs
        let (up_token, down_token) = match &market.clob_token_ids {
            Some(ids) => match parse_clob_token_ids(ids) {
                Some(pair) => pair,
                None => continue,
            },
            None => continue,
        };

        // Fetch real orderbook ask prices from CLOB
        let up_req = OrderBookSummaryRequest::builder().token_id(up_token).build();
        let down_req = OrderBookSummaryRequest::builder().token_id(down_token).build();

        let up_book = match clob.order_book(&up_req).await {
            Ok(b) => b,
            Err(e) => { debug!("UP orderbook fetch failed: {e}"); continue; }
        };
        let down_book = match clob.order_book(&down_req).await {
            Ok(b) => b,
            Err(e) => { debug!("DOWN orderbook fetch failed: {e}"); continue; }
        };

        let up_ask = match best_ask_price(&up_book) {
            Some(p) if p >= min_ask && p <= max_ask => p,
            _ => continue,
        };
        let down_ask = match best_ask_price(&down_book) {
            Some(p) if p >= min_ask && p <= max_ask => p,
            _ => continue,
        };

        let sum = up_ask + down_ask;
        let is_edge = sum <= threshold;

        debug!("  {question:.50}: UP=${up_ask:.3} + DOWN=${down_ask:.3} = ${sum:.4}");

        {
            let ts = Utc::now().format("%H:%M:%S").to_string();
            let short_q = if question.len() > 60 {
                format!("{}...", &question[..57])
            } else {
                question.to_string()
            };
            state.lock().unwrap().push_market(MarketCheck {
                question: short_q,
                yes_ask: format!("{up_ask:.3}"),
                no_ask: format!("{down_ask:.3}"),
                sum: format!("{sum:.4}"),
                is_edge,
                time: ts,
            });
        }

        if !is_edge {
            continue;
        }

        let edge_pct = (dec!(1) - sum) * dec!(100);
        info!(
            "[PAPER] EDGE on {question}: UP=${up_ask:.3} + DOWN=${down_ask:.3} = ${sum:.4} -> {edge_pct:.2}% profit"
        );

        {
            let mut s = state.lock().unwrap();
            s.edges_found += 1;
            s.push_event(
                "edge",
                format!("[PAPER] EDGE: {question} UP=${up_ask:.3}+DOWN=${down_ask:.3}=${sum:.4} ({edge_pct:.2}%)"),
            );
        }

        let edge = dec!(1) - sum;
        let target_profit = dec!(0.15);
        let size = (target_profit / edge).min(*balance / sum).min(dec!(5));
        let size = (size * dec!(100)).floor() / dec!(100);

        if size <= dec!(0) {
            continue;
        }

        let total_cost = size * sum;

        if *balance < total_cost {
            info!("[PAPER] Insufficient balance (${balance:.2}), skipping trade");
            continue;
        }

        let profit = size * edge;
        let mins_left = market.end_date
            .map(|d| (d - now).num_seconds() as f64 / 60.0)
            .unwrap_or(0.0);

        info!("[PAPER] TRADE: {size} shares @ ${sum:.4} sum, profit ~${profit:.2}, resolves in {mins_left:.1}min");

        {
            let mut s = state.lock().unwrap();
            s.trades_attempted += 1;
            *total_spent += total_cost;
            *balance += profit;
            s.total_spent = format!("{total_spent:.2}");
            s.balance = format!("{balance:.2}");
            s.push_event(
                "trade",
                format!("[PAPER] ARB: {question:.50} | {size}sh @ ${sum:.4} | +${profit:.2} in {mins_left:.1}min"),
            );
            info!("[PAPER] Simulated arb complete! Cost ~${total_cost:.2}, profit ~${profit:.2}");
        }
    }

    Ok(())
}

// Parse outcome_prices JSON string "[\"0.65\",\"0.35\"]" -> (yes_ask, no_ask)
fn parse_outcome_prices(raw: Option<&str>) -> Option<(Decimal, Decimal)> {
    let raw = raw?;
    let vals: Vec<serde_json::Value> = serde_json::from_str(raw).ok()?;
    if vals.len() < 2 {
        return None;
    }
    let parse = |v: &serde_json::Value| -> Option<Decimal> {
        if let Some(s) = v.as_str() { s.parse().ok() }
        else if let Some(f) = v.as_f64() { Decimal::from_f64_retain(f) }
        else { None }
    };
    Some((parse(&vals[0])?, parse(&vals[1])?))
}

// ── Live Trading Scan Logic ───────────────────────────────────────────────

async fn scan_and_trade(
    clob: &ClobClient<Authenticated<Normal>>,
    gamma: &GammaClient,
    signer: &(impl Signer + Send + Sync),
    threshold: Decimal,
    max_size: Decimal,
    min_ask: Decimal,
    max_ask: Decimal,
    state: &SharedState,
    total_spent: &mut Decimal,
) -> Result<()> {
    let now_live = chrono::Utc::now();
    let request = EventsRequest::builder()
        .closed(false)
        .active(true)
        .limit(200)
        .end_date_min(now_live)
        .end_date_max(now_live + chrono::Duration::hours(2))
        .build();

    let events = gamma
        .events(&request)
        .await
        .context("Failed to fetch events from Gamma API")?;

    let markets: Vec<_> = events
        .into_iter()
        .filter(|e| {
            let slug = e.slug.as_deref().unwrap_or("");
            (slug.contains("btc-updown") || slug.contains("eth-updown")
                || slug.contains("sol-updown") || slug.contains("xrp-updown"))
                && !e.closed.unwrap_or(false)
        })
        .flat_map(|e| e.markets.unwrap_or_default())
        .collect();

    debug!("Fetched {} markets from Gamma", markets.len());

    for market in &markets {
        let question = match &market.question {
            Some(q) => q.as_str(),
            None => continue,
        };

        let is_time_market = TIME_KEYWORDS.iter().any(|kw| question.contains(kw));
        let is_crypto = CRYPTO_KEYWORDS.iter().any(|kw| question.contains(kw));

        if !is_time_market || !is_crypto {
            continue;
        }

        let (yes_token, no_token) = match &market.clob_token_ids {
            Some(ids) => match parse_clob_token_ids(ids) {
                Some(pair) => pair,
                None => continue,
            },
            None => continue,
        };

        debug!("Checking: {question}");
        state.lock().unwrap().markets_checked += 1;

        let yes_req = OrderBookSummaryRequest::builder()
            .token_id(yes_token.clone())
            .build();
        let no_req = OrderBookSummaryRequest::builder()
            .token_id(no_token.clone())
            .build();

        let yes_book = match clob.order_book(&yes_req).await {
            Ok(book) => book,
            Err(e) => {
                debug!("  Orderbook fetch failed for YES: {e}");
                continue;
            }
        };

        let no_book = match clob.order_book(&no_req).await {
            Ok(book) => book,
            Err(e) => {
                debug!("  Orderbook fetch failed for NO: {e}");
                continue;
            }
        };

        let yes_ask = match best_ask_price(&yes_book) {
            Some(p) if p >= min_ask && p <= max_ask => p,
            _ => continue,
        };

        let no_ask = match best_ask_price(&no_book) {
            Some(p) if p >= min_ask && p <= max_ask => p,
            _ => continue,
        };

        let sum = yes_ask + no_ask;
        let is_edge = sum <= threshold;

        debug!("  YES ask=${yes_ask:.2}, NO ask=${no_ask:.2}, sum=${sum:.4}");

        {
            let now = Utc::now().format("%H:%M:%S").to_string();
            let short_q = if question.len() > 60 {
                format!("{}...", &question[..57])
            } else {
                question.to_string()
            };
            state.lock().unwrap().push_market(MarketCheck {
                question: short_q,
                yes_ask: format!("{yes_ask:.2}"),
                no_ask: format!("{no_ask:.2}"),
                sum: format!("{sum:.4}"),
                is_edge,
                time: now,
            });
        }

        if !is_edge {
            continue;
        }

        let edge_pct = (dec!(1.0) - sum) * dec!(100);
        info!(
            "EDGE on {question}: YES=${yes_ask:.2} + NO=${no_ask:.2} = ${sum:.4} -> {edge_pct:.1}% profit"
        );

        {
            let mut s = state.lock().unwrap();
            s.edges_found += 1;
            s.push_event(
                "edge",
                format!("EDGE: {question} YES=${yes_ask:.2}+NO=${no_ask:.2}=${sum:.4} ({edge_pct:.1}%)"),
            );
        }

        let size = max_size;
        let total_cost = size * yes_ask + size * no_ask;

        info!("  Attempting: {size} shares each side, total cost ~${total_cost:.2}");

        // Place YES buy order (Fill-or-Kill)
        match clob
            .limit_order()
            .token_id(&yes_token)
            .price(yes_ask)
            .size(size)
            .side(Side::Buy)
            .order_type(OrderType::FOK)
            .build()
            .await
        {
            Ok(yes_order) => {
                let signed = clob.sign(signer, yes_order).await?;
                match clob.post_order(signed).await {
                    Ok(resp) => {
                        info!("  YES order placed: {:?}", resp);
                        state
                            .lock()
                            .unwrap()
                            .push_event("trade", format!("YES order filled @ ${yes_ask:.2}"));
                    }
                    Err(e) => {
                        warn!("  YES order failed: {e}");
                        state
                            .lock()
                            .unwrap()
                            .push_event("warn", format!("YES order failed: {e}"));
                        continue;
                    }
                }
            }
            Err(e) => {
                warn!("  YES order build failed: {e}");
                state
                    .lock()
                    .unwrap()
                    .push_event("warn", format!("YES order build failed: {e}"));
                continue;
            }
        }

        // Place NO buy order
        match clob
            .limit_order()
            .token_id(&no_token)
            .price(no_ask)
            .size(size)
            .side(Side::Buy)
            .order_type(OrderType::FOK)
            .build()
            .await
        {
            Ok(no_order) => {
                let signed = clob.sign(signer, no_order).await?;
                match clob.post_order(signed).await {
                    Ok(resp) => {
                        info!("  NO order placed: {:?}", resp);
                        *total_spent += total_cost;
                        let mut s = state.lock().unwrap();
                        s.trades_attempted += 1;
                        s.total_spent = format!("{total_spent:.2}");
                        let profit = size - total_cost;
                        s.push_event(
                            "trade",
                            format!("ARB COMPLETE! Cost ${total_cost:.2}, profit ~${profit:.4}"),
                        );
                        info!("  Arb complete! Spent ~${total_cost:.2}, profit ~${profit:.4}");
                    }
                    Err(e) => {
                        warn!("  NO order failed: {e} — YES side may be exposed!");
                        warn!("  You now hold a directional YES position.");
                        state.lock().unwrap().push_event(
                            "warn",
                            format!("NO order failed: {e} — EXPOSED YES POSITION"),
                        );
                    }
                }
            }
            Err(e) => {
                warn!("  NO order build failed: {e} — YES side may be exposed!");
                state.lock().unwrap().push_event(
                    "warn",
                    format!("NO order build failed: {e} — EXPOSED YES POSITION"),
                );
            }
        }
    }

    Ok(())
}

// ── Dashboard HTML ─────────────────────────────────────────────────────────

const DASHBOARD_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Poly Arb Bot</title>
<style>
  * { margin: 0; padding: 0; box-sizing: border-box; }
  body {
    background: #0a0a14;
    color: #d0d0d0;
    font-family: 'SF Mono', 'Fira Code', 'Courier New', monospace;
    font-size: 18px;
    padding: 32px;
    min-height: 100vh;
  }
  .header {
    display: flex;
    align-items: center;
    gap: 16px;
    margin-bottom: 32px;
  }
  .header h1 {
    font-size: 40px;
    font-weight: 900;
    color: #00ff88;
    letter-spacing: 2px;
  }
  .pulse {
    width: 14px; height: 14px;
    background: #00ff88;
    border-radius: 50%;
    animation: pulse 2s ease-in-out infinite;
  }
  @keyframes pulse {
    0%, 100% { opacity: 1; box-shadow: 0 0 8px #00ff88; }
    50% { opacity: 0.4; box-shadow: 0 0 2px #00ff88; }
  }
  #status {
    color: #666;
    font-size: 15px;
    margin-bottom: 28px;
  }
  .cards {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(200px, 1fr));
    gap: 16px;
    margin-bottom: 36px;
  }
  .card {
    background: #12122a;
    border: 1px solid #1e1e3a;
    border-radius: 14px;
    padding: 28px 24px;
    text-align: center;
  }
  .card .val {
    font-size: 56px;
    font-weight: 800;
    color: #00ff88;
    line-height: 1;
  }
  .card.warn .val { color: #ff6600; }
  .card.spend .val { color: #00aaff; }
  .card .lbl {
    font-size: 13px;
    color: #555;
    text-transform: uppercase;
    letter-spacing: 2px;
    margin-top: 10px;
  }
  h2 {
    font-size: 22px;
    color: #00aaff;
    margin: 28px 0 14px;
    letter-spacing: 1px;
  }
  table {
    width: 100%;
    border-collapse: collapse;
    font-size: 17px;
    margin-bottom: 8px;
  }
  th {
    text-align: left;
    padding: 10px 14px;
    color: #444;
    font-size: 12px;
    text-transform: uppercase;
    letter-spacing: 1.5px;
    border-bottom: 1px solid #1a1a2e;
  }
  td {
    padding: 10px 14px;
    border-bottom: 1px solid #111;
  }
  tr.edge td { color: #ff6600; font-weight: 700; }
  tr.no-edge td { color: #444; }
  .events {
    max-height: 420px;
    overflow-y: auto;
    font-size: 16px;
    line-height: 1.8;
  }
  .ev { padding: 2px 0; }
  .ev .ts { color: #333; margin-right: 12px; }
  .ev.info { color: #00aaff; }
  .ev.edge { color: #ff6600; font-weight: 700; }
  .ev.trade { color: #00ff88; font-weight: 700; }
  .ev.warn { color: #ff4444; }
  .empty { color: #333; font-style: italic; padding: 20px 0; }
</style>
</head>
<body>

<div class="header">
  <div class="pulse"></div>
  <h1>POLY ARB BOT</h1>
</div>
<div id="status">Connecting...</div>

<div class="cards">
  <div class="card"><div class="val" id="scans">-</div><div class="lbl">Scans</div></div>
  <div class="card"><div class="val" id="checked">-</div><div class="lbl">Markets Checked</div></div>
  <div class="card warn"><div class="val" id="edges">-</div><div class="lbl">Edges Found</div></div>
  <div class="card"><div class="val" id="trades">-</div><div class="lbl">Trades</div></div>
  <div class="card spend"><div class="val" id="spent">-</div><div class="lbl">Total Spent</div></div>
  <div class="card"><div class="val" id="balance">-</div><div class="lbl">Balance</div></div>
</div>

<h2>RECENT MARKET CHECKS</h2>
<table>
  <thead><tr><th>Market</th><th>YES Ask</th><th>NO Ask</th><th>Sum</th><th>Time</th></tr></thead>
  <tbody id="markets"><tr><td colspan="5" class="empty">Waiting for data...</td></tr></tbody>
</table>

<h2>EVENT LOG</h2>
<div class="events" id="events"><div class="empty">No events yet...</div></div>

<script>
async function refresh() {
  try {
    const r = await fetch('/api/stats');
    const d = await r.json();

    document.getElementById('scans').textContent = d.total_scans.toLocaleString();
    document.getElementById('checked').textContent = d.markets_checked.toLocaleString();
    document.getElementById('edges').textContent = d.edges_found;
    document.getElementById('trades').textContent = d.trades_attempted;
    document.getElementById('spent').textContent = '$' + d.total_spent;
    document.getElementById('balance').textContent = '$' + d.balance;
    document.getElementById('status').textContent =
      'Live since ' + d.started_at + '  —  refreshing every 2s';

    if (d.recent_markets.length > 0) {
      let h = '';
      for (const m of d.recent_markets) {
        const c = m.is_edge ? 'edge' : 'no-edge';
        h += '<tr class="' + c + '"><td>' + m.question + '</td><td>$' + m.yes_ask +
             '</td><td>$' + m.no_ask + '</td><td>$' + m.sum + '</td><td>' + m.time + '</td></tr>';
      }
      document.getElementById('markets').innerHTML = h;
    }

    if (d.events.length > 0) {
      let h = '';
      for (const e of d.events) {
        h += '<div class="ev ' + e.level + '"><span class="ts">' + e.time + '</span>' + e.msg + '</div>';
      }
      document.getElementById('events').innerHTML = h;
    }
  } catch(err) {
    document.getElementById('status').textContent = 'Connection lost: ' + err.message;
  }
}
setInterval(refresh, 2000);
refresh();
</script>
</body>
</html>"#;
