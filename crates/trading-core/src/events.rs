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
    pub venue_market_key: String,
    pub title: String,
    pub symbol: String,
    pub window_label: String,
    pub end_label: String,
    pub side: Side,
    pub time_to_expiry_secs: u64,
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
                ..
            } => Self::TradePrint {
                market_id: *market_id,
                price: *price,
                size: *size,
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
            | Self::MarketDiscovered { ts_millis, .. }
            | Self::MarketExpired { ts_millis, .. }
            | Self::TimerTick { ts_millis, .. } => *ts_millis,
        }
    }

    pub fn into_runtime(self, base_instant: Instant, first_ts_millis: u64) -> NormalizedEvent {
        let offset = Duration::from_millis(self.ts_millis().saturating_sub(first_ts_millis));
        let ts = base_instant + offset;
        match self {
            Self::UnderlyingTick { symbol, px, .. } => NormalizedEvent::UnderlyingTick {
                symbol,
                px,
                ts,
            },
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
                ..
            } => NormalizedEvent::TradePrint {
                market_id,
                price,
                size,
                ts,
            },
            Self::MarketDiscovered { market, .. } => NormalizedEvent::MarketDiscovered { market, ts },
            Self::MarketExpired { market_id, .. } => NormalizedEvent::MarketExpired { market_id, ts },
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
