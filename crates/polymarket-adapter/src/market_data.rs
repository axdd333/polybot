use anyhow::{Context, Result};
use chrono::{DateTime, Local, Utc};
use futures::{future, StreamExt};
use polymarket_client_sdk::auth::{LocalSigner, Signer as _};
use polymarket_client_sdk::clob::types::request::OrderBookSummaryRequest;
use polymarket_client_sdk::clob::types::response::OrderBookSummaryResponse;
use polymarket_client_sdk::clob::types::Side as ClobSide;
use polymarket_client_sdk::clob::ws::types::response::{BookUpdate, LastTradePrice, OrderMessageType};
use polymarket_client_sdk::clob::ws::Client as ClobWsClient;
use polymarket_client_sdk::clob::Client as ClobClient;
use polymarket_client_sdk::gamma::types::request::EventsRequest;
use polymarket_client_sdk::gamma::types::response::Event as GammaEvent;
use polymarket_client_sdk::gamma::Client as GammaClient;
use polymarket_client_sdk::rtds::Client as RtdsClient;
use polymarket_client_sdk::types::{Address, B256, Decimal, U256};
use polymarket_client_sdk::POLYGON;
use reqwest::Client as HttpClient;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch, RwLock};
use tokio::task::JoinHandle;
use trading_core::config::{AdapterProfile, AssetConfig, LiveProfile, WalletSignatureType};
use trading_core::events::{LiveOrderStatus, MarketDiscovered, NormalizedEvent};
use trading_core::market::quote;
use trading_core::market::types::{AssetSymbol, InstrumentId, L2Level, MarketId, OrderAction, Side, Venue};

const ERROR_NOTE_MAX_CHARS: usize = 96;
const MAX_RECONNECT_BACKOFF_SECS: u64 = 30;

#[derive(Clone, Debug)]
struct LiveMarket {
    condition_id: String,
    title: String,
    symbol: String,
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
}

pub fn spawn_live_feeds(
    config: AdapterProfile,
    live: Option<LiveProfile>,
    tx: mpsc::Sender<NormalizedEvent>,
) -> Vec<JoinHandle<()>> {
    let registry = Arc::new(RwLock::new(Registry::default()));
    let (asset_tx, asset_rx) = watch::channel(Vec::<U256>::new());
    let (condition_tx, condition_rx) = watch::channel(Vec::<String>::new());
    let mut handles = vec![tokio::spawn(universe_refresh(
        config.clone(),
        tx.clone(),
        registry.clone(),
        asset_tx,
        condition_tx,
    ))];

    if config.clob_ws_enabled {
        handles.push(tokio::spawn(clob_ws_loop(
            tx.clone(),
            registry.clone(),
            asset_rx,
        )));
    }

    if config.rtds_enabled {
        handles.push(tokio::spawn(rtds_loop(config.clone(), tx.clone())));
    }

    if config.chainlink_fallback_enabled {
        handles.push(tokio::spawn(chainlink_fallback_loop(config.clone(), tx.clone())));
    }

    if let Some(live) = live.filter(|live| live.enabled) {
        handles.push(tokio::spawn(user_ws_loop(
            live,
            tx,
            registry,
            condition_rx,
        )));
    }

    handles
}

async fn universe_refresh(
    config: AdapterProfile,
    tx: mpsc::Sender<NormalizedEvent>,
    registry: Arc<RwLock<Registry>>,
    asset_tx: watch::Sender<Vec<U256>>,
    condition_tx: watch::Sender<Vec<String>>,
) {
    let gamma = GammaClient::default();
    let clob = polymarket_client_sdk::clob::Client::default();
    let mut active_market_ids = HashSet::new();

    loop {
        match fetch_active_markets(&gamma, &config).await {
            Ok(markets) => {
                let mut ids = Vec::new();
                let mut condition_ids = Vec::new();
                let mut fresh_active = HashSet::new();
                {
                    let mut reg = registry.write().await;
                    reg.tokens.clear();
                    for market in &markets {
                        fresh_active.insert(market.up_market_id);
                        fresh_active.insert(market.down_market_id);
                        reg.tokens.insert(market.up_token_id, market.up_market_id);
                        reg.tokens.insert(market.down_token_id, market.down_market_id);
                        condition_ids.push(market.condition_id.clone());
                        ids.push(market.up_token_id);
                        ids.push(market.down_token_id);
                    }
                }

                for market in markets {
                    let _ = tx
                        .send(NormalizedEvent::MarketDiscovered {
                            market: discovery_from_live_market(&market, Side::Up),
                            ts: Instant::now(),
                        })
                        .await;
                    let _ = tx
                        .send(NormalizedEvent::MarketDiscovered {
                            market: discovery_from_live_market(&market, Side::Down),
                            ts: Instant::now(),
                        })
                        .await;
                }

                for expired in active_market_ids.drain() {
                    if !fresh_active.contains(&expired) {
                        let _ = tx
                            .send(NormalizedEvent::MarketExpired {
                                market_id: expired,
                                ts: Instant::now(),
                            })
                            .await;
                    }
                }
                active_market_ids = fresh_active;

                let _ = asset_tx.send(ids.clone());
                let _ = condition_tx.send(condition_ids);
                backfill_books(&clob, &tx, &registry, &ids).await;
            }
            Err(err) => {
                let _ = concise_error(&err.to_string());
            }
        }

        tokio::time::sleep(Duration::from_secs(config.universe_refresh_secs)).await;
    }
}

async fn user_ws_loop(
    live: LiveProfile,
    tx: mpsc::Sender<NormalizedEvent>,
    registry: Arc<RwLock<Registry>>,
    mut condition_rx: watch::Receiver<Vec<String>>,
) {
    let (client, ws_client) = match authenticated_clients(&live).await {
        Ok(clients) => clients,
        Err(err) => {
            let _ = tx
                .send(NormalizedEvent::TimerTick {
                    cadence: trading_core::events::TimerCadence::Slow,
                    ts: Instant::now(),
                })
                .await;
            eprintln!("live auth failed: {err}");
            return;
        }
    };

    let _ = client;
    let mut failures = 0_u32;
    loop {
        let condition_ids = condition_rx.borrow().clone();
        if condition_ids.is_empty() {
            if condition_rx.changed().await.is_err() {
                break;
            }
            continue;
        }

        let markets: Vec<B256> = condition_ids
            .iter()
            .filter_map(|id| id.parse::<B256>().ok())
            .collect();
        if markets.is_empty() {
            if condition_rx.changed().await.is_err() {
                break;
            }
            continue;
        }

        let orders = match ws_client.subscribe_orders(markets.clone()) {
            Ok(stream) => Box::pin(stream.fuse()),
            Err(_) => {
                failures = failures.saturating_add(1);
                tokio::time::sleep(reconnect_delay(failures)).await;
                continue;
            }
        };
        let trades = match ws_client.subscribe_trades(markets) {
            Ok(stream) => Box::pin(stream.fuse()),
            Err(_) => {
                failures = failures.saturating_add(1);
                tokio::time::sleep(reconnect_delay(failures)).await;
                continue;
            }
        };

        failures = 0;
        let mut orders = orders;
        let mut trades = trades;
        let mut ended_cleanly = false;

        loop {
            tokio::select! {
                changed = condition_rx.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    if *condition_rx.borrow() != condition_ids {
                        ended_cleanly = true;
                        break;
                    }
                }
                item = orders.next() => {
                    match item {
                        Some(Ok(order)) => {
                            translate_user_order(order, &tx, &registry).await;
                        }
                        Some(Err(_)) | None => break,
                    }
                }
                item = trades.next() => {
                    match item {
                        Some(Ok(trade)) => {
                            translate_user_trade(trade, &tx, &registry).await;
                        }
                        Some(Err(_)) | None => break,
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

async fn clob_ws_loop(
    tx: mpsc::Sender<NormalizedEvent>,
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
            Err(_) => {
                failures = failures.saturating_add(1);
                tokio::time::sleep(reconnect_delay(failures)).await;
                continue;
            }
        };
        let trades = match client.subscribe_last_trade_price(asset_ids.clone()) {
            Ok(stream) => Box::pin(stream.fuse()),
            Err(_) => {
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
                        Some(Ok(book)) => translate_book(book, &tx, &registry).await,
                        Some(Err(_)) | None => break,
                    }
                }
                item = trades.next() => {
                    match item {
                        Some(Ok(trade)) => translate_trade(trade, &tx, &registry).await,
                        Some(Err(_)) | None => break,
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

async fn rtds_loop(config: AdapterProfile, tx: mpsc::Sender<NormalizedEvent>) {
    let client = RtdsClient::default();
    let symbols = config
        .assets
        .iter()
        .map(|asset| asset.rtds_symbol.clone())
        .collect::<Vec<_>>();
    let mut stream = match client.subscribe_crypto_prices(Some(symbols)) {
        Ok(stream) => Box::pin(stream.fuse()),
        Err(_) => return,
    };

    while let Some(item) = stream.next().await {
        match item {
            Ok(update) => {
                if let Some(asset) = config
                    .assets
                    .iter()
                    .find(|asset| asset.rtds_symbol == update.symbol)
                {
                    let _ = tx
                        .send(NormalizedEvent::UnderlyingTick {
                            symbol: asset.name.clone(),
                            px: quote::decimal_to_f64(update.value),
                            ts: Instant::now(),
                        })
                        .await;
                }
            }
            Err(_) => break,
        }
    }
}

async fn chainlink_fallback_loop(config: AdapterProfile, tx: mpsc::Sender<NormalizedEvent>) {
    let http = HttpClient::new();
    loop {
        let tasks = config.assets.iter().map(|asset| async {
            let price = chainlink_price(&http, &config.polygon_rpc_url, &asset.oracle).await;
            (asset.name.clone(), price)
        });
        for (symbol, result) in future::join_all(tasks).await {
            if let Ok(price) = result {
                let _ = tx
                    .send(NormalizedEvent::UnderlyingTick {
                        symbol,
                        px: quote::decimal_to_f64(price),
                        ts: Instant::now(),
                    })
                    .await;
            }
        }
        tokio::time::sleep(Duration::from_secs(15)).await;
    }
}

fn discovery_from_live_market(market: &LiveMarket, side: Side) -> MarketDiscovered {
    let market_id = match side {
        Side::Up => market.up_market_id,
        Side::Down => market.down_market_id,
    };
    let token_id = match side {
        Side::Up => market.up_token_id.to_string(),
        Side::Down => market.down_token_id.to_string(),
    };
    MarketDiscovered {
        instrument_id: InstrumentId {
            venue: Venue::Polymarket,
            symbol: AssetSymbol(market.symbol.clone()),
            market: market_id,
            side,
        },
        condition_id: market.condition_id.clone(),
        token_id,
        title: market.title.clone(),
        symbol: market.symbol.clone(),
        window_label: market.window_label.clone(),
        end_label: market.end_label.clone(),
        side,
        time_to_expiry_secs: market.time_to_expiry.as_secs(),
    }
}

async fn translate_user_order(
    order: polymarket_client_sdk::clob::ws::types::response::OrderMessage,
    tx: &mpsc::Sender<NormalizedEvent>,
    registry: &Arc<RwLock<Registry>>,
) {
    let market_id = {
        let reg = registry.read().await;
        reg.tokens.get(&order.asset_id).copied()
    };
    let Some(market_id) = market_id else {
        return;
    };
    let _ = tx
        .send(NormalizedEvent::LiveOrderUpdate {
            market_id,
            order_id: order.id,
            status: map_live_order_status(order.msg_type.as_ref()),
            size_matched: order
                .size_matched
                .map(quote::decimal_to_f64)
                .unwrap_or(0.0),
            ts: Instant::now(),
        })
        .await;
}

async fn translate_user_trade(
    trade: polymarket_client_sdk::clob::ws::types::response::TradeMessage,
    tx: &mpsc::Sender<NormalizedEvent>,
    registry: &Arc<RwLock<Registry>>,
) {
    let market_id = {
        let reg = registry.read().await;
        reg.tokens.get(&trade.asset_id).copied()
    };
    let Some(market_id) = market_id else {
        return;
    };
    let order_id = trade
        .taker_order_id
        .clone()
        .or_else(|| trade.maker_orders.first().map(|maker| maker.order_id.clone()));
    let action = match trade.side {
        ClobSide::Buy => OrderAction::Buy,
        ClobSide::Sell => OrderAction::Sell,
        ClobSide::Unknown => return,
        _ => return,
    };

    let _ = tx
        .send(NormalizedEvent::LiveTrade {
            market_id,
            order_id,
            action,
            price: quote::decimal_to_f64(trade.price),
            qty: quote::decimal_to_f64(trade.size),
            ts: Instant::now(),
        })
        .await;
}

async fn translate_book(
    book: BookUpdate,
    tx: &mpsc::Sender<NormalizedEvent>,
    registry: &Arc<RwLock<Registry>>,
) {
    let market_id = {
        let reg = registry.read().await;
        reg.tokens.get(&book.asset_id).copied()
    };
    let Some(market_id) = market_id else {
        return;
    };

    let bids = book.bids.into_iter().map(level_from_book).collect::<Vec<_>>();
    let asks = book.asks.into_iter().map(level_from_book).collect::<Vec<_>>();
    let _ = tx
        .send(NormalizedEvent::BookSnapshot {
            market_id,
            bids,
            asks,
            ts: Instant::now(),
        })
        .await;
}

async fn translate_trade(
    trade: LastTradePrice,
    tx: &mpsc::Sender<NormalizedEvent>,
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
        .send(NormalizedEvent::TradePrint {
            market_id,
            price: quote::decimal_to_f64(trade.price),
            size: trade.size.map(quote::decimal_to_f64).unwrap_or(0.0),
            ts: Instant::now(),
        })
        .await;
}

async fn backfill_books(
    clob: &polymarket_client_sdk::clob::Client,
    tx: &mpsc::Sender<NormalizedEvent>,
    registry: &Arc<RwLock<Registry>>,
    asset_ids: &[U256],
) {
    for token_id in asset_ids {
        let request = OrderBookSummaryRequest::builder().token_id(*token_id).build();
        if let Ok(book) = clob.order_book(&request).await {
            translate_backfill_book(book, tx, registry).await;
        }
    }
}

async fn translate_backfill_book(
    book: OrderBookSummaryResponse,
    tx: &mpsc::Sender<NormalizedEvent>,
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
        .send(NormalizedEvent::BookSnapshot {
            market_id,
            bids,
            asks,
            ts: Instant::now(),
        })
        .await;
}

async fn fetch_active_markets(gamma: &GammaClient, config: &AdapterProfile) -> Result<Vec<LiveMarket>> {
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
    Ok(normalize_events(events, now, &config.assets))
}

fn normalize_events(
    events: Vec<GammaEvent>,
    now: DateTime<Utc>,
    assets: &[AssetConfig],
) -> Vec<LiveMarket> {
    let now_unix = now.timestamp();
    let mut markets = Vec::new();

    for event in events {
        let Some(slug) = event.slug.as_deref() else {
            continue;
        };
        let Some(asset) = assets
            .iter()
            .find(|asset| asset.slug_prefixes.iter().any(|prefix| slug.starts_with(prefix)))
        else {
            continue;
        };
        let Some(market) = event.markets.as_ref().and_then(|markets| markets.first()) else {
            continue;
        };
        let Some(condition_id) = market.condition_id.map(|value| value.to_string()) else {
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
            condition_id,
            title,
            symbol: asset.name.clone(),
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

async fn authenticated_clients(
    live: &LiveProfile,
) -> Result<(
    ClobClient<polymarket_client_sdk::auth::state::Authenticated<polymarket_client_sdk::auth::Normal>>,
    ClobWsClient<polymarket_client_sdk::auth::state::Authenticated<polymarket_client_sdk::auth::Normal>>,
)> {
    let private_key = std::env::var(&live.private_key_env)
        .with_context(|| format!("missing env var {}", live.private_key_env))?;
    let signer = LocalSigner::from_str(&private_key)?.with_chain_id(Some(POLYGON));
    let mut auth = ClobClient::new(&live.clob_host, Default::default())?
        .authentication_builder(&signer);
    match live.signature_type {
        WalletSignatureType::Eoa => auth = auth.signature_type(polymarket_client_sdk::clob::types::SignatureType::Eoa),
        WalletSignatureType::Proxy => auth = auth.signature_type(polymarket_client_sdk::clob::types::SignatureType::Proxy),
        WalletSignatureType::GnosisSafe => auth = auth.signature_type(polymarket_client_sdk::clob::types::SignatureType::GnosisSafe),
    }
    if let Some(funder) = &live.funder {
        auth = auth.funder(funder.parse::<Address>()?);
    }
    let client = auth.authenticate().await?;
    let ws = ClobWsClient::new(&live.ws_host, Default::default())?
        .authenticate(client.credentials().clone(), client.address())?;
    Ok((client, ws))
}

fn map_live_order_status(msg_type: Option<&OrderMessageType>) -> LiveOrderStatus {
    match msg_type {
        Some(OrderMessageType::Placement) => LiveOrderStatus::Open,
        Some(OrderMessageType::Update) => LiveOrderStatus::PartiallyFilled,
        Some(OrderMessageType::Cancellation) => LiveOrderStatus::Cancelled,
        _ => LiveOrderStatus::Pending,
    }
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

async fn chainlink_price(http: &HttpClient, rpc_url: &str, oracle: &str) -> Result<Decimal> {
    #[derive(Deserialize)]
    struct RpcResp {
        result: String,
    }

    let resp: RpcResp = http
        .post(rpc_url)
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
