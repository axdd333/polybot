use serde::{Deserialize, Serialize};
use rust_decimal::Decimal;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MarketId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Side {
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Regime {
    Continuation,
    Reversion,
    Chop,
    Burst,
    ExpiryPinch,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Venue {
    Polymarket,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AssetSymbol(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InstrumentId {
    pub venue: Venue,
    pub symbol: AssetSymbol,
    pub market: MarketId,
    pub side: Side,
}

impl InstrumentId {
    pub fn placeholder(market: MarketId) -> Self {
        Self {
            venue: Venue::Polymarket,
            symbol: AssetSymbol("UNKNOWN".to_string()),
            market,
            side: Side::Up,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct L2Level {
    pub price: Decimal,
    pub size: Decimal,
}

#[derive(Debug, Clone)]
pub struct OrderBook {
    pub bids: Vec<L2Level>,
    pub asks: Vec<L2Level>,
    pub last_update: Instant,
}

impl Default for OrderBook {
    fn default() -> Self {
        Self {
            bids: Vec::new(),
            asks: Vec::new(),
            last_update: Instant::now(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct UnderlyingState {
    pub spot: Decimal,
    pub ret_250ms: f64,
    pub ret_1s: f64,
    pub ret_5s: f64,
    pub accel: f64,
    pub vol_5s: f64,
    pub vol_15s: f64,
}

impl Default for UnderlyingState {
    fn default() -> Self {
        Self {
            spot: Decimal::ZERO,
            ret_250ms: 0.0,
            ret_1s: 0.0,
            ret_5s: 0.0,
            accel: 0.0,
            vol_5s: 0.0,
            vol_15s: 0.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MarketState {
    pub market_id: MarketId,
    pub side: Side,
    pub time_to_expiry: Duration,
    /// Wall-clock instant when time_to_expiry was last set from ExpiryUpdate.
    pub expiry_anchored_at: Instant,
    pub book: OrderBook,
    pub underlying: UnderlyingState,
    pub spread_ticks: f64,
    pub microprice: f64,
    pub imbalance_top: f64,
    pub imbalance_5lvl: f64,
    pub depth_slope_bid: f64,
    pub depth_slope_ask: f64,
    pub cancel_rate_bid: f64,
    pub cancel_rate_ask: f64,
    pub trade_intensity: f64,
    pub burstiness: f64,
    pub cross_window_torsion: f64,
    pub liquidity_void_score: f64,
    pub wall_persistence_score: f64,
    pub regime: Regime,
    pub model_score: f64,
    pub fair_value: f64,
    pub edge_buy: f64,
    pub edge_sell: f64,
}

impl MarketState {
    pub fn new(market_id: MarketId, side: Side, time_to_expiry: Duration) -> Self {
        Self {
            market_id,
            side,
            time_to_expiry,
            expiry_anchored_at: Instant::now(),
            book: OrderBook::default(),
            underlying: UnderlyingState::default(),
            spread_ticks: 0.0,
            microprice: 0.0,
            imbalance_top: 0.0,
            imbalance_5lvl: 0.0,
            depth_slope_bid: 0.0,
            depth_slope_ask: 0.0,
            cancel_rate_bid: 0.0,
            cancel_rate_ask: 0.0,
            trade_intensity: 0.0,
            burstiness: 0.0,
            cross_window_torsion: 0.0,
            liquidity_void_score: 1.0,
            wall_persistence_score: 0.0,
            regime: Regime::Chop,
            model_score: 0.0,
            fair_value: 0.5,
            edge_buy: 0.0,
            edge_sell: 0.0,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct QueueTracker {
    pub est_queue_ahead: f64,
    pub fill_hazard: f64,
    pub cancel_hazard: f64,
}

#[derive(Debug, Clone, Default)]
pub struct CrossWindowState {
    pub m5_score: f64,
    pub m15_score: f64,
    pub torsion: f64,
}

#[derive(Debug, Clone, Default)]
pub struct PositionState {
    pub qty: f64,
    pub avg_px: f64,
    pub unrealized: f64,
    pub realized: f64,
    pub max_adverse_excursion: f64,
    pub max_favorable_excursion: f64,
}

// ---------------------------------------------------------------------------
// Order types (previously in execution.rs)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderAction {
    Buy,
    Sell,
}

#[derive(Debug, Clone)]
pub struct OrderIntent {
    pub market_id: MarketId,
    pub side: Side,
    pub action: OrderAction,
    pub price: f64,
    pub qty: f64,
    pub ttl_ms: u64,
    pub aggressive: bool,
}

#[derive(Debug, Clone, Default)]
pub struct OrderSurface {
    pub intents: Vec<OrderIntent>,
}
