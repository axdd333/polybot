use crate::domain::market::quote;
use crate::domain::market::types::{
    CrossWindowState, MarketState, OrderBook, QueueTracker, UnderlyingState,
};
use rust_decimal::Decimal;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

const UNDERLYING_HISTORY_CAP: usize = 128;
pub(crate) const TRADE_HISTORY_CAP: usize = 128;

// ---------------------------------------------------------------------------
// Types previously private inside state.rs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) struct UnderlyingPoint {
    pub ts: Instant,
    pub px: f64,
}

#[derive(Debug, Clone)]
pub(crate) struct TradeObservation {
    pub ts: Instant,
    pub price: f64,
    pub size: f64,
}

#[derive(Debug, Clone)]
pub(crate) struct MarketRuntime {
    pub title: String,
    pub symbol: String,
    pub window_label: String,
    pub end_label: String,
    pub queue: QueueTracker,
    pub cross_window: CrossWindowState,
    pub trade_tape: VecDeque<TradeObservation>,
    pub fair_history: VecDeque<(Instant, f64)>,
    pub edge_history: VecDeque<(Instant, f64)>,
    pub flow_history: VecDeque<(Instant, f64)>,
    pub micro_history: VecDeque<(Instant, f64)>,
    pub last_bid_depth: f64,
    pub last_ask_depth: f64,
    pub last_book_ts: Option<Instant>,
    pub wall_persistence_score: f64,
    pub last_plan_note: Option<String>,
    pub last_plan_note_at: Option<Instant>,
}

impl Default for MarketRuntime {
    fn default() -> Self {
        Self {
            title: "Unknown Market".to_string(),
            symbol: "UNKNOWN".to_string(),
            window_label: "?".to_string(),
            end_label: "--:--".to_string(),
            queue: QueueTracker::default(),
            cross_window: CrossWindowState::default(),
            trade_tape: VecDeque::new(),
            fair_history: VecDeque::new(),
            edge_history: VecDeque::new(),
            flow_history: VecDeque::new(),
            micro_history: VecDeque::new(),
            last_bid_depth: 0.0,
            last_ask_depth: 0.0,
            last_book_ts: None,
            wall_persistence_score: 0.0,
            last_plan_note: None,
            last_plan_note_at: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Book runtime update (cancel rates, wall persistence, depth tracking)
// ---------------------------------------------------------------------------

pub(crate) fn update_book_runtime(
    market: &mut MarketState,
    runtime: &mut MarketRuntime,
    ts: Instant,
) {
    let bid_depth: f64 = market
        .book
        .bids
        .iter()
        .take(5)
        .map(|l| quote::decimal_to_f64(l.size))
        .sum();
    let ask_depth: f64 = market
        .book
        .asks
        .iter()
        .take(5)
        .map(|l| quote::decimal_to_f64(l.size))
        .sum();

    let elapsed_ms = runtime
        .last_book_ts
        .map(|last| ts.duration_since(last).as_millis() as f64)
        .unwrap_or(1.0)
        .max(1.0);

    let bid_cancel = (runtime.last_bid_depth - bid_depth).max(0.0) / elapsed_ms;
    let ask_cancel = (runtime.last_ask_depth - ask_depth).max(0.0) / elapsed_ms;

    runtime.queue.cancel_hazard = bid_cancel.max(ask_cancel);
    runtime.queue.fill_hazard = (bid_depth + ask_depth) / 2.0;
    runtime.wall_persistence_score =
        ((runtime.wall_persistence_score * 0.8) + bid_depth.min(ask_depth) * 0.2).min(5.0);
    runtime.last_bid_depth = bid_depth;
    runtime.last_ask_depth = ask_depth;
    runtime.last_book_ts = Some(ts);

    market.cancel_rate_bid = bid_cancel;
    market.cancel_rate_ask = ask_cancel;
}

// ---------------------------------------------------------------------------
// Underlying price tape
// ---------------------------------------------------------------------------

pub(crate) fn update_underlying_state(
    tape: &mut VecDeque<UnderlyingPoint>,
    px: f64,
    ts: Instant,
) -> UnderlyingState {
    tape.push_back(UnderlyingPoint { ts, px });
    trim_underlying_tape(tape, ts);

    let ret_250ms = horizon_return(tape, ts, Duration::from_millis(250));
    let ret_1s = horizon_return(tape, ts, Duration::from_secs(1));
    let ret_5s = horizon_return(tape, ts, Duration::from_secs(5));
    let vol_5s = realized_vol(tape, ts, Duration::from_secs(5));
    let vol_15s = realized_vol(tape, ts, Duration::from_secs(15));

    UnderlyingState {
        spot: Decimal::from_f64_retain(px).unwrap_or(Decimal::ZERO),
        ret_250ms,
        ret_1s,
        ret_5s,
        accel: ret_250ms - ret_1s,
        vol_5s,
        vol_15s,
    }
}

fn trim_underlying_tape(tape: &mut VecDeque<UnderlyingPoint>, now: Instant) {
    while tape.len() > UNDERLYING_HISTORY_CAP {
        tape.pop_front();
    }
    while tape
        .front()
        .map(|p| now.duration_since(p.ts) > Duration::from_secs(20))
        .unwrap_or(false)
    {
        tape.pop_front();
    }
}

fn horizon_return(tape: &VecDeque<UnderlyingPoint>, now: Instant, horizon: Duration) -> f64 {
    let Some(last) = tape.back() else {
        return 0.0;
    };
    let Some(base) = tape
        .iter()
        .rev()
        .find(|p| now.duration_since(p.ts) >= horizon)
    else {
        return 0.0;
    };
    if base.px <= 0.0 {
        0.0
    } else {
        (last.px - base.px) / base.px
    }
}

fn realized_vol(tape: &VecDeque<UnderlyingPoint>, now: Instant, horizon: Duration) -> f64 {
    let samples: Vec<f64> = tape
        .iter()
        .filter(|p| now.duration_since(p.ts) <= horizon)
        .map(|p| p.px)
        .collect();
    if samples.len() < 2 {
        return 0.0;
    }

    let returns: Vec<f64> = samples
        .windows(2)
        .filter_map(|w| {
            if w[0] <= 0.0 {
                None
            } else {
                Some((w[1] - w[0]) / w[0])
            }
        })
        .collect();

    if returns.is_empty() {
        return 0.0;
    }

    let mean = returns.iter().sum::<f64>() / returns.len() as f64;
    let variance = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / returns.len() as f64;
    variance.sqrt()
}

// ---------------------------------------------------------------------------
// Trade tape
// ---------------------------------------------------------------------------

pub(crate) fn trim_trade_tape(tape: &mut VecDeque<TradeObservation>, now: Instant) {
    while tape.len() > TRADE_HISTORY_CAP {
        tape.pop_front();
    }
    while tape
        .front()
        .map(|t| now.duration_since(t.ts) > Duration::from_secs(10))
        .unwrap_or(false)
    {
        tape.pop_front();
    }
}

pub(crate) fn trade_metrics(tape: &VecDeque<TradeObservation>, now: Instant) -> (f64, f64) {
    let trades: Vec<&TradeObservation> = tape
        .iter()
        .filter(|t| now.duration_since(t.ts) <= Duration::from_secs(2))
        .collect();
    if trades.is_empty() {
        return (0.0, 0.0);
    }

    let total_size = trades.iter().map(|t| t.size).sum::<f64>();
    let intensity = total_size / 2.0;

    let mean_price = trades.iter().map(|t| t.price).sum::<f64>() / trades.len() as f64;
    let burstiness = trades
        .iter()
        .map(|t| (t.price - mean_price).abs())
        .sum::<f64>()
        / trades.len() as f64;

    (intensity, burstiness)
}

// ---------------------------------------------------------------------------
// History series (for time-series charts in the TUI)
// ---------------------------------------------------------------------------

pub fn push_history(history: &mut VecDeque<(Instant, f64)>, ts: Instant, value: f64) {
    history.push_back((ts, value));
    while history.len() > 120 {
        history.pop_front();
    }
}

pub fn to_series(history: &VecDeque<(Instant, f64)>) -> Vec<(f64, f64)> {
    let Some((last_ts, _)) = history.back() else {
        return Vec::new();
    };
    history
        .iter()
        .map(|(ts, value)| {
            let age = last_ts.duration_since(*ts).as_secs_f64();
            (-age, *value)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Liquidation mark (portfolio pricing)
// ---------------------------------------------------------------------------

pub fn midpoint(book: &OrderBook) -> f64 {
    let bid = crate::domain::market::book::best_bid(book);
    let ask = crate::domain::market::book::best_ask(book);
    if bid > 0.0 && ask > 0.0 {
        (bid + ask) / 2.0
    } else {
        0.0
    }
}

pub fn liquidation_mark(book: &OrderBook, fallback_fair: f64) -> f64 {
    let bid = crate::domain::market::book::best_bid(book);
    if bid > 0.0 {
        return bid;
    }
    let ask = crate::domain::market::book::best_ask(book);
    if ask > 0.0 {
        return ask;
    }
    midpoint(book).max(fallback_fair).max(0.0)
}
