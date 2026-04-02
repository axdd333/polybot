use anyhow::Result;
use chrono::{DateTime, Local, Utc};
use futures::{future, StreamExt};
use polymarket_client_sdk::clob::types::request::OrderBookSummaryRequest;
use polymarket_client_sdk::clob::types::response::OrderBookSummaryResponse;
use polymarket_client_sdk::clob::ws::types::response::{BookUpdate, LastTradePrice};
use polymarket_client_sdk::clob::ws::Client as ClobWsClient;
use polymarket_client_sdk::gamma::types::request::EventsRequest;
use polymarket_client_sdk::gamma::types::response::Event as GammaEvent;
use polymarket_client_sdk::gamma::Client as GammaClient;
use polymarket_client_sdk::rtds::Client as RtdsClient;
use polymarket_client_sdk::types::{Decimal, U256};
use reqwest::Client as HttpClient;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::env;
use std::hash::{Hash, Hasher};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch, Mutex, RwLock};
use tokio::task::JoinHandle;

use crate::app::events::Event;
use crate::domain::market::quote;
use crate::domain::market::types::{L2Level, MarketId, Side};
use crate::domain::trading::world::World;

const UNIVERSE_REFRESH_SECS: u64 = 20;
const ERROR_NOTE_MAX_CHARS: usize = 96;
const MAX_RECONNECT_BACKOFF_SECS: u64 = 30;

#[derive(Clone, Debug)]
struct Asset {
    name: &'static str,
    oracle: &'static str,
    rtds_symbol: &'static str,
    slug_5m: &'static str,
    slug_15m: &'static str,
}

static ASSETS: &[Asset] = &[Asset {
    name: "BTC",
    oracle: "0xc907E116054Ad103354f2D350FD2514433D57F6f",
    rtds_symbol: "btcusdt",
    slug_5m: "bitcoin",
    slug_15m: "btc",
}];

#[derive(Clone, Debug)]
struct LiveMarket {
    title: String,
    symbol: &'static str,
    window_label: String,
    end_label: String,
    time_to_expiry: Duration,
    up_market_id: MarketId,
    down_market_id: MarketId,
    up_token_id: U256,
    down_token_id: U256,
}

#[derive(Default)]
struct Registry {
    tokens: HashMap<U256, MarketId>,
    asset_ids: Vec<U256>,
}

pub fn spawn_live_feeds(world: Arc<Mutex<World>>, tx: mpsc::Sender<Event>) -> Vec<JoinHandle<()>> {
    let registry = Arc::new(RwLock::new(Registry::default()));
    let (asset_tx, asset_rx) = watch::channel(Vec::<U256>::new());
    let mut handles = vec![tokio::spawn(universe_refresh(
        world.clone(),
        tx.clone(),
        registry.clone(),
        asset_tx,
    ))];

    if env_bool("CLOB_WS_ENABLED", true) {
        handles.push(tokio::spawn(clob_ws_loop(
            tx.clone(),
            registry.clone(),
            asset_rx,
        )));
    }

    if env_bool("RTDS_ENABLED", true) {
        handles.push(tokio::spawn(rtds_loop(tx.clone())));
    }

    if env_bool("CHAINLINK_FALLBACK_ENABLED", true) {
        handles.push(tokio::spawn(chainlink_fallback_loop(tx)));
    }

    handles
}

async fn universe_refresh(
    world: Arc<Mutex<World>>,
    tx: mpsc::Sender<Event>,
    registry: Arc<RwLock<Registry>>,
    asset_tx: watch::Sender<Vec<U256>>,
) {
    let gamma = GammaClient::default();
    let clob = polymarket_client_sdk::clob::Client::default();

    loop {
        match fetch_active_markets(&gamma).await {
            Ok(markets) => {
                let mut ids = Vec::new();
                let mut active_market_ids = HashSet::new();
                let mut expiry_updates = Vec::new();
                {
                    let mut world = world.lock().await;
                    let mut reg = registry.write().await;
                    reg.tokens.clear();

                    for market in markets {
                        active_market_ids.insert(market.up_market_id);
                        active_market_ids.insert(market.down_market_id);
                        world.seed_market(
                            market.up_market_id,
                            &market.title,
                            market.symbol,
                            market.window_label.clone(),
                            market.end_label.clone(),
                            Side::Up,
                            market.time_to_expiry,
                        );
                        world.seed_market(
                            market.down_market_id,
                            &market.title,
                            market.symbol,
                            market.window_label,
                            market.end_label.clone(),
                            Side::Down,
                            market.time_to_expiry,
                        );
                        reg.tokens.insert(market.up_token_id, market.up_market_id);
                        reg.tokens
                            .insert(market.down_token_id, market.down_market_id);
                        ids.push(market.up_token_id);
                        ids.push(market.down_token_id);
                        expiry_updates.push(Event::ExpiryUpdate {
                            market_id: market.up_market_id,
                            time_to_expiry: market.time_to_expiry,
                        });
                        expiry_updates.push(Event::ExpiryUpdate {
                            market_id: market.down_market_id,
                            time_to_expiry: market.time_to_expiry,
                        });
                    }

                    world.prune_markets(&active_market_ids);

                    let n = ids.len() / 2;
                    world.journal.push(format!(
                        "universe: {n} live markets discovered ({})",
                        chrono::Local::now().format("%H:%M:%S")
                    ));
                    reg.asset_ids = ids.clone();
                }

                for event in expiry_updates {
                    let _ = tx.send(event).await;
                }
                let _ = asset_tx.send(ids.clone());
                backfill_books(&clob, &tx, &registry, &ids).await;
            }
            Err(err) => {
                let msg = format!("universe error: {}", concise_error(&err.to_string()));
                if let Ok(mut w) = world.try_lock() {
                    w.journal.push(msg);
                }
            }
        }

        tokio::time::sleep(Duration::from_secs(UNIVERSE_REFRESH_SECS)).await;
    }
}

async fn clob_ws_loop(
    tx: mpsc::Sender<Event>,
    registry: Arc<RwLock<Registry>>,
    mut asset_rx: watch::Receiver<Vec<U256>>,
) {
    let mut failures = 0_u32;
    loop {
        let asset_ids = asset_rx.borrow().clone();
        if asset_ids.is_empty() {
            if asset_rx.changed().await.is_err() {
                break;
            }
            continue;
        }

        let client = ClobWsClient::default();
        let books = match client.subscribe_orderbook(asset_ids.clone()) {
            Ok(stream) => Box::pin(stream.fuse()),
            Err(_err) => {
                failures = failures.saturating_add(1);
                tokio::time::sleep(reconnect_delay(failures)).await;
                continue;
            }
        };
        let trades = match client.subscribe_last_trade_price(asset_ids.clone()) {
            Ok(stream) => Box::pin(stream.fuse()),
            Err(_err) => {
                failures = failures.saturating_add(1);
                tokio::time::sleep(reconnect_delay(failures)).await;
                continue;
            }
        };

        failures = 0;
        let mut books = books;
        let mut trades = trades;
        let mut ended_cleanly = false;

        loop {
            tokio::select! {
                changed = asset_rx.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    if *asset_rx.borrow() != asset_ids {
                        ended_cleanly = true;
                        break;
                    }
                }
                item = books.next() => {
                    match item {
                        Some(Ok(book)) => {
                            translate_book(book, &tx, &registry).await;
                        }
                        Some(Err(err)) => {
                            let _ = err;
                            break;
                        }
                        None => break,
                    }
                }
                item = trades.next() => {
                    match item {
                        Some(Ok(trade)) => {
                            translate_trade(trade, &tx, &registry).await;
                        }
                        Some(Err(err)) => {
                            let _ = err;
                            break;
                        }
                        None => break,
                    }
                }
            }
        }

        if !ended_cleanly {
            failures = failures.saturating_add(1);
            tokio::time::sleep(reconnect_delay(failures)).await;
        }
    }
}

async fn rtds_loop(tx: mpsc::Sender<Event>) {
    let client = RtdsClient::default();
    let symbols = ASSETS
        .iter()
        .map(|asset| asset.rtds_symbol.to_string())
        .collect::<Vec<_>>();
    let mut stream = match client.subscribe_crypto_prices(Some(symbols)) {
        Ok(stream) => Box::pin(stream.fuse()),
        Err(err) => {
            let _ = err;
            return;
        }
    };

    while let Some(item) = stream.next().await {
        match item {
            Ok(update) => {
                if let Some(asset) = ASSETS
                    .iter()
                    .find(|asset| asset.rtds_symbol == update.symbol)
                {
                    let _ = tx
                        .send(Event::UnderlyingTick {
                            symbol: asset.name.to_string(),
                            px: quote::decimal_to_f64(update.value),
                            ts: Instant::now(),
                        })
                        .await;
                }
            }
            Err(err) => {
                let _ = err;
                break;
            }
        }
    }
}

async fn chainlink_fallback_loop(tx: mpsc::Sender<Event>) {
    let http = HttpClient::new();
    loop {
        let tasks = ASSETS.iter().map(|asset| async {
            let price = chainlink_price(&http, asset.oracle).await;
            (asset.name, price)
        });
        for (symbol, result) in future::join_all(tasks).await {
            if let Ok(price) = result {
                let _ = tx
                    .send(Event::UnderlyingTick {
                        symbol: symbol.to_string(),
                        px: quote::decimal_to_f64(price),
                        ts: Instant::now(),
                    })
                    .await;
            }
        }
        tokio::time::sleep(Duration::from_secs(15)).await;
    }
}

async fn translate_book(
    book: BookUpdate,
    tx: &mpsc::Sender<Event>,
    registry: &Arc<RwLock<Registry>>,
) {
    let market_id = {
        let reg = registry.read().await;
        reg.tokens.get(&book.asset_id).copied()
    };
    let Some(market_id) = market_id else {
        return;
    };

    let bids = book
        .bids
        .into_iter()
        .map(level_from_book)
        .collect::<Vec<_>>();
    let asks = book
        .asks
        .into_iter()
        .map(level_from_book)
        .collect::<Vec<_>>();
    let _ = tx
        .send(Event::BookSnapshot {
            market_id,
            bids,
            asks,
            ts: Instant::now(),
        })
        .await;
}

async fn translate_trade(
    trade: LastTradePrice,
    tx: &mpsc::Sender<Event>,
    registry: &Arc<RwLock<Registry>>,
) {
    let market_id = {
        let reg = registry.read().await;
        reg.tokens.get(&trade.asset_id).copied()
    };
    let Some(market_id) = market_id else {
        return;
    };

    let _ = tx
        .send(Event::TradePrint {
            market_id,
            price: quote::decimal_to_f64(trade.price),
            size: trade.size.map(quote::decimal_to_f64).unwrap_or(0.0),
            ts: Instant::now(),
        })
        .await;
}

async fn backfill_books(
    clob: &polymarket_client_sdk::clob::Client,
    tx: &mpsc::Sender<Event>,
    registry: &Arc<RwLock<Registry>>,
    asset_ids: &[U256],
) {
    for token_id in asset_ids {
        let request = OrderBookSummaryRequest::builder()
            .token_id(*token_id)
            .build();
        if let Ok(book) = clob.order_book(&request).await {
            translate_backfill_book(book, tx, registry).await;
        }
    }
}

async fn translate_backfill_book(
    book: OrderBookSummaryResponse,
    tx: &mpsc::Sender<Event>,
    registry: &Arc<RwLock<Registry>>,
) {
    let market_id = {
        let reg = registry.read().await;
        reg.tokens.get(&book.asset_id).copied()
    };
    let Some(market_id) = market_id else {
        return;
    };

    let bids = book
        .bids
        .into_iter()
        .map(|level| L2Level {
            price: level.price,
            size: level.size,
        })
        .collect::<Vec<_>>();
    let asks = book
        .asks
        .into_iter()
        .map(|level| L2Level {
            price: level.price,
            size: level.size,
        })
        .collect::<Vec<_>>();

    let _ = tx
        .send(Event::BookSnapshot {
            market_id,
            bids,
            asks,
            ts: Instant::now(),
        })
        .await;
}

async fn fetch_active_markets(gamma: &GammaClient) -> Result<Vec<LiveMarket>> {
    let now = Utc::now();
    let request = EventsRequest::builder()
        .closed(false)
        .active(true)
        .order(vec!["endDate".to_string()])
        .ascending(true)
        .end_date_min(now)
        .limit(500)
        .build();
    let events = gamma.events(&request).await?;
    Ok(normalize_events(events, now))
}

fn normalize_events(events: Vec<GammaEvent>, now: DateTime<Utc>) -> Vec<LiveMarket> {
    let now_unix = now.timestamp();
    let mut markets = Vec::new();

    for event in events {
        let Some(slug) = event.slug.as_deref() else {
            continue;
        };
        let Some(asset) = ASSETS
            .iter()
            .find(|asset| slug.starts_with(asset.slug_5m) || slug.starts_with(asset.slug_15m))
        else {
            continue;
        };
        let Some(market) = event.markets.as_ref().and_then(|markets| markets.first()) else {
            continue;
        };
        let Some(end_time) = market.end_date else {
            continue;
        };
        let Some(token_ids) = &market.clob_token_ids else {
            continue;
        };
        if token_ids.len() != 2 {
            continue;
        }
        let Some((up_token_id, down_token_id)) = infer_token_mapping(
            market.question.as_deref(),
            market.outcomes.as_deref(),
            token_ids,
        ) else {
            continue;
        };
        let start_unix = slug
            .rsplit('-')
            .next()
            .and_then(|part| part.parse::<i64>().ok())
            .unwrap_or(now_unix);

        let secs_left = (end_time - now).num_seconds();
        // Accept currently live markets up to 16 minutes out.
        // Keeping the lower bound above zero lets the universe rotate continuously
        // without carrying already-resolved listings.
        if secs_left <= 0 || secs_left > 960 {
            continue;
        }

        let up_market_id = hashed_market_id(slug, "up");
        let down_market_id = hashed_market_id(slug, "down");
        let start_label = DateTime::from_timestamp(start_unix, 0)
            .map(|ts| ts.with_timezone(&Local).format("%H:%M").to_string())
            .unwrap_or_else(|| "--:--".to_string());
        let end_label = end_time.with_timezone(&Local).format("%H:%M").to_string();
        let title = event
            .title
            .clone()
            .unwrap_or_else(|| "Unknown Event".to_string());

        markets.push(LiveMarket {
            title,
            symbol: asset.name,
            window_label: format!("{start_label}-{end_label}"),
            end_label,
            time_to_expiry: Duration::from_secs(secs_left as u64),
            up_market_id,
            down_market_id,
            up_token_id,
            down_token_id,
        });
    }

    markets.sort_by_key(|market| market.time_to_expiry);
    markets.truncate(48);
    markets
}

fn hashed_market_id(slug: &str, side: &str) -> MarketId {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    slug.hash(&mut hasher);
    side.hash(&mut hasher);
    MarketId(hasher.finish())
}

fn infer_token_mapping(
    question: Option<&str>,
    outcomes: Option<&[String]>,
    token_ids: &[U256],
) -> Option<(U256, U256)> {
    if token_ids.len() != 2 {
        return None;
    }

    let outcomes = outcomes?;
    if outcomes.len() != 2 {
        return None;
    }

    let first = normalize_label(&outcomes[0])?;
    let second = normalize_label(&outcomes[1])?;

    match (first, second) {
        (OutcomeLabel::Up, OutcomeLabel::Down) => Some((token_ids[0], token_ids[1])),
        (OutcomeLabel::Down, OutcomeLabel::Up) => Some((token_ids[1], token_ids[0])),
        (OutcomeLabel::Yes, OutcomeLabel::No) => infer_yes_no_mapping(question, token_ids),
        (OutcomeLabel::No, OutcomeLabel::Yes) => {
            infer_yes_no_mapping(question, &[token_ids[1], token_ids[0]])
        }
        _ => None,
    }
}

fn infer_yes_no_mapping(question: Option<&str>, token_ids: &[U256]) -> Option<(U256, U256)> {
    if token_ids.len() != 2 {
        return None;
    }

    let question = question?.to_ascii_lowercase();
    let bullish = [" up", "higher", "above", "rise", "gain", "increase", "bull"]
        .iter()
        .any(|needle| question.contains(needle));
    let bearish = [
        " down", "lower", "below", "fall", "drop", "decrease", "bear",
    ]
    .iter()
    .any(|needle| question.contains(needle));

    if bullish == bearish {
        return None;
    }

    if bullish {
        Some((token_ids[0], token_ids[1]))
    } else {
        Some((token_ids[1], token_ids[0]))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutcomeLabel {
    Yes,
    No,
    Up,
    Down,
}

fn normalize_label(label: &str) -> Option<OutcomeLabel> {
    let value = label.trim().to_ascii_lowercase();
    match value.as_str() {
        "yes" => Some(OutcomeLabel::Yes),
        "no" => Some(OutcomeLabel::No),
        "up" | "higher" | "above" => Some(OutcomeLabel::Up),
        "down" | "lower" | "below" => Some(OutcomeLabel::Down),
        _ => None,
    }
}

fn level_from_book(
    level: polymarket_client_sdk::clob::ws::types::response::OrderBookLevel,
) -> L2Level {
    L2Level {
        price: level.price,
        size: level.size,
    }
}

fn reconnect_delay(failures: u32) -> Duration {
    let shift = failures.saturating_sub(1).min(5);
    Duration::from_secs(1_u64 << shift).min(Duration::from_secs(MAX_RECONNECT_BACKOFF_SECS))
}

async fn chainlink_price(http: &HttpClient, oracle: &str) -> Result<Decimal> {
    #[derive(Deserialize)]
    struct RpcResp {
        result: String,
    }

    let resp: RpcResp = http
        .post(polygon_rpc_url())
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_call",
            "params": [
                { "to": oracle, "data": "0xfeaf968c" },
                "latest"
            ],
            "id": 1
        }))
        .send()
        .await?
        .json()
        .await?;

    let hex = resp.result.trim_start_matches("0x");
    let answer_hex = &hex[64..128];
    let raw = u128::from_str_radix(answer_hex, 16)?;
    let price = Decimal::from_str(&format!("{raw}"))? / Decimal::from(100_000_000u64);
    Ok(price)
}

fn env_bool(key: &str, default: bool) -> bool {
    env::var(key)
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(default)
}

fn polygon_rpc_url() -> String {
    env::var("POLYGON_RPC_URL").unwrap_or_else(|_| "https://polygon.drpc.org".to_string())
}

fn concise_error(err: &str) -> String {
    let compact = err.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut concise = compact
        .chars()
        .take(ERROR_NOTE_MAX_CHARS)
        .collect::<String>();
    if compact.chars().count() > ERROR_NOTE_MAX_CHARS {
        concise.push_str("...");
    }
    concise
}

#[cfg(test)]
mod tests {
    use super::{concise_error, infer_token_mapping, U256};

    #[test]
    fn maps_yes_no_question_with_bullish_wording() {
        let outcomes = vec!["Yes".to_string(), "No".to_string()];
        let token_ids = vec![U256::from(11_u64), U256::from(22_u64)];

        let mapping = infer_token_mapping(
            Some("Will BTC be higher at 12:10?"),
            Some(&outcomes),
            &token_ids,
        );

        assert_eq!(mapping, Some((U256::from(11_u64), U256::from(22_u64))));
    }

    #[test]
    fn rejects_unknown_yes_no_question_direction() {
        let outcomes = vec!["Yes".to_string(), "No".to_string()];
        let token_ids = vec![U256::from(11_u64), U256::from(22_u64)];

        let mapping = infer_token_mapping(
            Some("Will the market resolve this window?"),
            Some(&outcomes),
            &token_ids,
        );

        assert_eq!(mapping, None);
    }

    #[test]
    fn concise_error_trims_transport_noise() {
        let err = "Internal: error sending request for url (https://gamma-api.polymarket.com/events?limit=500&order=endDate)";
        let concise = concise_error(err);
        assert!(concise.len() <= 99);
        assert!(concise.starts_with("Internal: error sending request"));
    }
}
