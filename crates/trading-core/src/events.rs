use crate::market::types::{InstrumentId, L2Level, MarketId, Side};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TimerCadence {
    Fast,
    Slow,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketDiscovered {
    pub instrument_id: InstrumentId,
    #[serde(default)]
    pub condition_id: String,
    #[serde(default)]
    pub token_id: String,
    pub title: String,
    pub symbol: String,
    pub window_label: String,
    pub end_label: String,
    pub side: Side,
    pub time_to_expiry_secs: u64,
    #[serde(default = "default_tick_size")]
    pub min_tick_size: f64,
    #[serde(default = "default_order_size")]
    pub min_order_size: f64,
    #[serde(default)]
    pub maker_fee_bps: f64,
    #[serde(default)]
    pub taker_fee_bps: f64,
    #[serde(default = "default_accepting_orders")]
    pub accepting_orders: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LiveOrderStatus {
    Pending,
    Open,
    PartiallyFilled,
    Filled,
    Cancelled,
    Rejected,
}

#[derive(Debug, Clone)]
pub enum NormalizedEvent {
    UnderlyingTick {
        symbol: String,
        px: f64,
        ts: Instant,
    },
    BookSnapshot {
        market_id: MarketId,
        bids: Vec<L2Level>,
        asks: Vec<L2Level>,
        ts: Instant,
    },
    BookDelta {
        market_id: MarketId,
        bids: Vec<L2Level>,
        asks: Vec<L2Level>,
        ts: Instant,
    },
    TradePrint {
        market_id: MarketId,
        price: f64,
        size: f64,
        fee_rate_bps: Option<f64>,
        ts: Instant,
    },
    TickSizeChange {
        market_id: MarketId,
        new_tick_size: f64,
        ts: Instant,
    },
    MarketDiscovered {
        market: MarketDiscovered,
        ts: Instant,
    },
    MarketExpired {
        market_id: MarketId,
        ts: Instant,
    },
    LiveOrderUpdate {
        market_id: MarketId,
        order_id: String,
        status: LiveOrderStatus,
        size_matched: f64,
        ts: Instant,
    },
    LiveTrade {
        market_id: MarketId,
        order_id: Option<String>,
        action: crate::market::types::OrderAction,
        price: f64,
        qty: f64,
        ts: Instant,
    },
    TimerTick {
        cadence: TimerCadence,
        ts: Instant,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecordedEvent {
    UnderlyingTick {
        symbol: String,
        px: f64,
        ts_millis: u64,
    },
    BookSnapshot {
        market_id: MarketId,
        bids: Vec<L2Level>,
        asks: Vec<L2Level>,
        ts_millis: u64,
    },
    BookDelta {
        market_id: MarketId,
        bids: Vec<L2Level>,
        asks: Vec<L2Level>,
        ts_millis: u64,
    },
    TradePrint {
        market_id: MarketId,
        price: f64,
        size: f64,
        #[serde(default)]
        fee_rate_bps: Option<f64>,
        ts_millis: u64,
    },
    TickSizeChange {
        market_id: MarketId,
        new_tick_size: f64,
        ts_millis: u64,
    },
    MarketDiscovered {
        market: MarketDiscovered,
        ts_millis: u64,
    },
    MarketExpired {
        market_id: MarketId,
        ts_millis: u64,
    },
    LiveOrderUpdate {
        market_id: MarketId,
        order_id: String,
        status: LiveOrderStatus,
        size_matched: f64,
        ts_millis: u64,
    },
    LiveTrade {
        market_id: MarketId,
        order_id: Option<String>,
        action: crate::market::types::OrderAction,
        price: f64,
        qty: f64,
        ts_millis: u64,
    },
    TimerTick {
        cadence: TimerCadence,
        ts_millis: u64,
    },
}

impl RecordedEvent {
    pub fn from_runtime(event: &NormalizedEvent) -> Self {
        let ts_millis = now_ts_millis();
        match event {
            NormalizedEvent::UnderlyingTick { symbol, px, .. } => Self::UnderlyingTick {
                symbol: symbol.clone(),
                px: *px,
                ts_millis,
            },
            NormalizedEvent::BookSnapshot {
                market_id,
                bids,
                asks,
                ..
            } => Self::BookSnapshot {
                market_id: *market_id,
                bids: bids.clone(),
                asks: asks.clone(),
                ts_millis,
            },
            NormalizedEvent::BookDelta {
                market_id,
                bids,
                asks,
                ..
            } => Self::BookDelta {
                market_id: *market_id,
                bids: bids.clone(),
                asks: asks.clone(),
                ts_millis,
            },
            NormalizedEvent::TradePrint {
                market_id,
                price,
                size,
                fee_rate_bps,
                ..
            } => Self::TradePrint {
                market_id: *market_id,
                price: *price,
                size: *size,
                fee_rate_bps: *fee_rate_bps,
                ts_millis,
            },
            NormalizedEvent::TickSizeChange {
                market_id,
                new_tick_size,
                ..
            } => Self::TickSizeChange {
                market_id: *market_id,
                new_tick_size: *new_tick_size,
                ts_millis,
            },
            NormalizedEvent::MarketDiscovered { market, .. } => Self::MarketDiscovered {
                market: market.clone(),
                ts_millis,
            },
            NormalizedEvent::MarketExpired { market_id, .. } => Self::MarketExpired {
                market_id: *market_id,
                ts_millis,
            },
            NormalizedEvent::LiveOrderUpdate {
                market_id,
                order_id,
                status,
                size_matched,
                ..
            } => Self::LiveOrderUpdate {
                market_id: *market_id,
                order_id: order_id.clone(),
                status: *status,
                size_matched: *size_matched,
                ts_millis,
            },
            NormalizedEvent::LiveTrade {
                market_id,
                order_id,
                action,
                price,
                qty,
                ..
            } => Self::LiveTrade {
                market_id: *market_id,
                order_id: order_id.clone(),
                action: *action,
                price: *price,
                qty: *qty,
                ts_millis,
            },
            NormalizedEvent::TimerTick { cadence, .. } => Self::TimerTick {
                cadence: *cadence,
                ts_millis,
            },
        }
    }

    pub fn ts_millis(&self) -> u64 {
        match self {
            Self::UnderlyingTick { ts_millis, .. }
            | Self::BookSnapshot { ts_millis, .. }
            | Self::BookDelta { ts_millis, .. }
            | Self::TradePrint { ts_millis, .. }
            | Self::TickSizeChange { ts_millis, .. }
            | Self::MarketDiscovered { ts_millis, .. }
            | Self::MarketExpired { ts_millis, .. }
            | Self::LiveOrderUpdate { ts_millis, .. }
            | Self::LiveTrade { ts_millis, .. }
            | Self::TimerTick { ts_millis, .. } => *ts_millis,
        }
    }

    pub fn into_runtime(self, base_instant: Instant, first_ts_millis: u64) -> NormalizedEvent {
        let offset = Duration::from_millis(self.ts_millis().saturating_sub(first_ts_millis));
        let ts = base_instant + offset;
        match self {
            Self::UnderlyingTick { symbol, px, .. } => {
                NormalizedEvent::UnderlyingTick { symbol, px, ts }
            }
            Self::BookSnapshot {
                market_id,
                bids,
                asks,
                ..
            } => NormalizedEvent::BookSnapshot {
                market_id,
                bids,
                asks,
                ts,
            },
            Self::BookDelta {
                market_id,
                bids,
                asks,
                ..
            } => NormalizedEvent::BookDelta {
                market_id,
                bids,
                asks,
                ts,
            },
            Self::TradePrint {
                market_id,
                price,
                size,
                fee_rate_bps,
                ..
            } => NormalizedEvent::TradePrint {
                market_id,
                price,
                size,
                fee_rate_bps,
                ts,
            },
            Self::TickSizeChange {
                market_id,
                new_tick_size,
                ..
            } => NormalizedEvent::TickSizeChange {
                market_id,
                new_tick_size,
                ts,
            },
            Self::MarketDiscovered { market, .. } => {
                NormalizedEvent::MarketDiscovered { market, ts }
            }
            Self::MarketExpired { market_id, .. } => {
                NormalizedEvent::MarketExpired { market_id, ts }
            }
            Self::LiveOrderUpdate {
                market_id,
                order_id,
                status,
                size_matched,
                ..
            } => NormalizedEvent::LiveOrderUpdate {
                market_id,
                order_id,
                status,
                size_matched,
                ts,
            },
            Self::LiveTrade {
                market_id,
                order_id,
                action,
                price,
                qty,
                ..
            } => NormalizedEvent::LiveTrade {
                market_id,
                order_id,
                action,
                price,
                qty,
                ts,
            },
            Self::TimerTick { cadence, .. } => NormalizedEvent::TimerTick { cadence, ts },
        }
    }
}

fn now_ts_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn default_tick_size() -> f64 {
    0.01
}

fn default_order_size() -> f64 {
    1.0
}

fn default_accepting_orders() -> bool {
    true
}
