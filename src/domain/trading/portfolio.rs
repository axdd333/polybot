use crate::domain::config::SweepStrategyConfig;
use crate::domain::market::types::{MarketId, OrderAction, OrderSurface, PositionState};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// Each stream starts at $1. When it doubles, it splits into two $1 streams.
const STREAM_UNIT: f64 = 1.0;

#[derive(Debug)]
pub struct Portfolio {
    positions: HashMap<MarketId, PositionState>,
    order_surfaces: HashMap<MarketId, OrderSurface>,
    pub cash: f64,
    pub realized_pnl: f64,
    pub kill_switch: bool,
    pub stream_splits: u32,
    split_credit: f64,
    realized_events: VecDeque<(Instant, f64)>,
}

impl Portfolio {
    pub fn new() -> Self {
        Self::with_starting_cash(SweepStrategyConfig::default().starting_cash)
    }

    pub fn with_starting_cash(starting_cash: f64) -> Self {
        Self {
            positions: HashMap::new(),
            order_surfaces: HashMap::new(),
            cash: starting_cash,
            realized_pnl: 0.0,
            kill_switch: false,
            stream_splits: 0,
            split_credit: 0.0,
            realized_events: VecDeque::new(),
        }
    }

    pub fn check_stream_split(&mut self, profit: f64) -> bool {
        if profit <= 0.0 {
            return false;
        }
        self.split_credit += profit;
        if self.split_credit >= STREAM_UNIT {
            self.stream_splits += (self.split_credit / STREAM_UNIT) as u32;
            self.split_credit %= STREAM_UNIT;
            return true;
        }
        false
    }

    pub fn inventory_for(&self, market_id: MarketId) -> f64 {
        self.positions.get(&market_id).map(|p| p.qty).unwrap_or(0.0)
    }

    pub fn position(&self, market_id: MarketId) -> Option<&PositionState> {
        self.positions.get(&market_id)
    }

    pub fn position_mut(&mut self, market_id: MarketId) -> &mut PositionState {
        self.positions.entry(market_id).or_default()
    }

    pub fn replace_surface(&mut self, market_id: MarketId, surface: OrderSurface) {
        self.order_surfaces.insert(market_id, surface);
    }

    pub fn surface(&self, market_id: MarketId) -> Option<&OrderSurface> {
        self.order_surfaces.get(&market_id)
    }

    pub fn mark_price(&mut self, market_id: MarketId, mark: f64) {
        if let Some(position) = self.positions.get_mut(&market_id) {
            position.unrealized = (mark - position.avg_px) * position.qty;
            position.max_favorable_excursion =
                position.max_favorable_excursion.max(position.unrealized);
            position.max_adverse_excursion =
                position.max_adverse_excursion.min(position.unrealized);
        }
    }

    pub fn drop_market(&mut self, market_id: MarketId) {
        self.order_surfaces.remove(&market_id);
        self.positions.remove(&market_id);
    }

    pub fn apply_fill(
        &mut self,
        market_id: MarketId,
        action: OrderAction,
        price: f64,
        qty: f64,
        ts: Instant,
    ) -> f64 {
        match action {
            OrderAction::Buy => self.buy_fill(market_id, price, qty),
            OrderAction::Sell => self.sell_fill(market_id, price, qty, ts),
        }
    }

    fn buy_fill(&mut self, market_id: MarketId, price: f64, qty: f64) -> f64 {
        if price <= 0.0 || qty <= 0.0 {
            return 0.0;
        }

        let affordable_qty = (self.cash / price).min(qty).max(0.0);
        if affordable_qty <= 0.0 {
            return 0.0;
        }

        self.cash -= affordable_qty * price;
        let position = self.positions.entry(market_id).or_default();
        let new_qty = position.qty + affordable_qty;
        position.avg_px = if new_qty > 0.0 {
            ((position.avg_px * position.qty) + (price * affordable_qty)) / new_qty
        } else {
            price
        };
        position.qty = new_qty;
        affordable_qty
    }

    fn sell_fill(&mut self, market_id: MarketId, price: f64, qty: f64, ts: Instant) -> f64 {
        let (sell_qty, pnl, remove_position) = {
            let Some(position) = self.positions.get_mut(&market_id) else {
                return 0.0;
            };
            let sell_qty = qty.min(position.qty).max(0.0);
            if sell_qty <= 0.0 {
                return 0.0;
            }

            let pnl = (price - position.avg_px) * sell_qty;
            position.realized += pnl;
            position.qty -= sell_qty;
            let remove_position = position.qty <= 1e-9;
            (sell_qty, pnl, remove_position)
        };

        self.cash += sell_qty * price;
        self.realized_pnl += pnl;
        self.realized_events.push_back((ts, pnl));
        self.trim_realized_events(ts);
        if remove_position {
            self.positions.remove(&market_id);
        }
        self.check_stream_split(pnl);
        sell_qty
    }

    pub fn total_unrealized(&self) -> f64 {
        self.positions.values().map(|p| p.unrealized).sum()
    }

    pub fn realized_over_window(&self, now: Instant, window: Duration) -> f64 {
        self.realized_events
            .iter()
            .filter(|(ts, _)| now.duration_since(*ts) <= window)
            .map(|(_, pnl)| *pnl)
            .sum()
    }

    fn trim_realized_events(&mut self, now: Instant) {
        let keep_for = Duration::from_secs(15 * 60);
        while self
            .realized_events
            .front()
            .map(|(ts, _)| now.duration_since(*ts) > keep_for)
            .unwrap_or(false)
        {
            self.realized_events.pop_front();
        }
    }
}

impl Default for Portfolio {
    fn default() -> Self {
        Self::new()
    }
}
