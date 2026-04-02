use crate::domain::market::types::{L2Level, MarketId};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub enum Event {
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
    ExpiryUpdate {
        market_id: MarketId,
        time_to_expiry: Duration,
    },
    TimerFast,
    TimerSlow,
}
