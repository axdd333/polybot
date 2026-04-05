use crate::market::types::{MarketId, OrderAction, OrderSurface, PositionState};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// Each stream starts at $1. When it doubles, it splits into two $1 streams.
const STREAM_UNIT: f64 = 1.0;

#[derive(Debug, Clone)]
struct ClosedTrade {
    ts: Instant,
    pnl: f64,
    hold_secs: f64,
}

#[derive(Debug, Clone, Default)]
pub struct ClosedTradeStats {
    pub count: usize,
    pub wins: usize,
    pub losses: usize,
    pub best_pnl: f64,
    pub worst_pnl: f64,
    pub gross_profit: f64,
    pub gross_loss: f64,
}

impl ClosedTradeStats {
    pub fn win_rate(&self) -> f64 {
        rate(self.wins, self.count)
    }

    pub fn profit_factor(&self) -> f64 {
        if self.gross_loss > 0.0 {
            self.gross_profit / self.gross_loss
        } else if self.gross_profit > 0.0 {
            f64::INFINITY
        } else {
            0.0
        }
    }
}

#[derive(Debug)]
pub struct Portfolio {
    positions: HashMap<MarketId, PositionState>,
    order_surfaces: HashMap<MarketId, OrderSurface>,
    pub starting_cash: f64,
    pub cash: f64,
    pub realized_pnl: f64,
    pub kill_switch: bool,
    pub stream_splits: u32,
    split_credit: f64,
    closed_stats: ClosedTradeStats,
    realized_events: VecDeque<(Instant, f64)>,
    closed_trades: VecDeque<ClosedTrade>,
}

impl Portfolio {
    pub fn new() -> Self {
        Self::with_starting_cash(100.0)
    }

    pub fn with_starting_cash(starting_cash: f64) -> Self {
        Self {
            positions: HashMap::new(),
            order_surfaces: HashMap::new(),
            starting_cash,
            cash: starting_cash,
            realized_pnl: 0.0,
            kill_switch: false,
            stream_splits: 0,
            split_credit: 0.0,
            closed_stats: ClosedTradeStats::default(),
            realized_events: VecDeque::new(),
            closed_trades: VecDeque::new(),
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

    pub fn observe_fair(&mut self, market_id: MarketId, fair: f64, ts: Instant) {
        let Some(position) = self.positions.get_mut(&market_id) else {
            return;
        };
        if fair > position.best_fair + 1e-9 {
            position.best_fair = fair;
            position.last_fair_improve_at = Some(ts);
        }
        position.last_fair = fair;
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
        fee: f64,
        ts: Instant,
    ) -> f64 {
        match action {
            OrderAction::Buy => self.buy_fill(market_id, price, qty, fee, ts),
            OrderAction::Sell => self.sell_fill(market_id, price, qty, fee, ts),
        }
    }

    fn buy_fill(
        &mut self,
        market_id: MarketId,
        price: f64,
        qty: f64,
        fee: f64,
        ts: Instant,
    ) -> f64 {
        if price <= 0.0 || qty <= 0.0 {
            return 0.0;
        }

        let unit_cost = (price + fee.max(0.0) / qty).max(price);
        let affordable_qty = (self.cash / unit_cost).min(qty).max(0.0);
        if affordable_qty <= 0.0 {
            return 0.0;
        }

        let paid_fee = fee.max(0.0) * (affordable_qty / qty);
        self.cash -= (affordable_qty * price) + paid_fee;
        let position = self.positions.entry(market_id).or_default();
        let new_qty = position.qty + affordable_qty;
        position.avg_px = if new_qty > 0.0 {
            let old_cost = position.avg_px * position.qty;
            (old_cost + (price * affordable_qty) + paid_fee) / new_qty
        } else {
            price
        };
        if position.qty <= 0.0 {
            position.opened_at = Some(ts);
            position.best_fair = price;
            position.last_fair = price;
            position.last_fair_improve_at = Some(ts);
        }
        position.qty = new_qty;
        affordable_qty
    }

    fn sell_fill(
        &mut self,
        market_id: MarketId,
        price: f64,
        qty: f64,
        fee: f64,
        ts: Instant,
    ) -> f64 {
        let (sell_qty, pnl, remove_position) = {
            let Some(position) = self.positions.get_mut(&market_id) else {
                return 0.0;
            };
            let sell_qty = qty.min(position.qty).max(0.0);
            if sell_qty <= 0.0 {
                return 0.0;
            }

            let paid_fee = fee.max(0.0) * (sell_qty / qty);
            let pnl = ((price - position.avg_px) * sell_qty) - paid_fee;
            position.realized += pnl;
            position.qty -= sell_qty;
            let remove_position = position.qty <= 1e-9;
            (sell_qty, pnl, remove_position)
        };

        let paid_fee = fee.max(0.0) * (sell_qty / qty);
        self.cash += (sell_qty * price) - paid_fee;
        self.realized_pnl += pnl;
        self.realized_events.push_back((ts, pnl));
        if remove_position {
            let hold_secs = self
                .positions
                .get(&market_id)
                .and_then(|p| p.opened_at)
                .map(|opened| ts.duration_since(opened).as_secs_f64())
                .unwrap_or(0.0);
            self.closed_trades
                .push_back(ClosedTrade { ts, pnl, hold_secs });
            bump_trade_stats(&mut self.closed_stats, pnl);
        }
        self.trim_realized_events(ts);
        self.trim_closed_trades(ts);
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

    pub fn drawdown_frac(&self, equity: f64) -> f64 {
        if self.starting_cash <= 0.0 {
            return 0.0;
        }
        ((self.starting_cash - equity) / self.starting_cash).max(0.0)
    }

    pub fn realized_per_min(&self, now: Instant, window: Duration) -> f64 {
        let mins = (window.as_secs_f64() / 60.0).max(1.0 / 60.0);
        self.realized_over_window(now, window) / mins
    }

    pub fn closed_trades_per_min(&self, now: Instant, window: Duration) -> f64 {
        let mins = (window.as_secs_f64() / 60.0).max(1.0 / 60.0);
        let n = self
            .closed_trades
            .iter()
            .filter(|t| now.duration_since(t.ts) <= window)
            .count();
        n as f64 / mins
    }

    pub fn avg_hold_secs(&self, now: Instant, window: Duration) -> f64 {
        let mut n = 0.0;
        let mut sum = 0.0;
        for trade in &self.closed_trades {
            if now.duration_since(trade.ts) > window {
                continue;
            }
            n += 1.0;
            sum += trade.hold_secs;
        }
        if n > 0.0 {
            sum / n
        } else {
            0.0
        }
    }

    pub fn recent_closed_pnl(&self, now: Instant, window: Duration) -> f64 {
        self.closed_trades
            .iter()
            .filter(|t| now.duration_since(t.ts) <= window)
            .map(|t| t.pnl)
            .sum()
    }

    pub fn closed_trade_stats(&self) -> ClosedTradeStats {
        self.closed_stats.clone()
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

    fn trim_closed_trades(&mut self, now: Instant) {
        let keep_for = Duration::from_secs(15 * 60);
        while self
            .closed_trades
            .front()
            .map(|t| now.duration_since(t.ts) > keep_for)
            .unwrap_or(false)
        {
            self.closed_trades.pop_front();
        }
    }
}

impl Default for Portfolio {
    fn default() -> Self {
        Self::new()
    }
}

fn bump_trade_stats(stats: &mut ClosedTradeStats, pnl: f64) {
    stats.count += 1;
    stats.best_pnl = best_pnl(stats.count, stats.best_pnl, pnl);
    stats.worst_pnl = worst_pnl(stats.count, stats.worst_pnl, pnl);
    if pnl >= 0.0 {
        stats.wins += 1;
        stats.gross_profit += pnl;
        return;
    }
    stats.losses += 1;
    stats.gross_loss += -pnl;
}

fn best_pnl(count: usize, cur: f64, pnl: f64) -> f64 {
    if count == 1 {
        pnl
    } else {
        cur.max(pnl)
    }
}

fn worst_pnl(count: usize, cur: f64, pnl: f64) -> f64 {
    if count == 1 {
        pnl
    } else {
        cur.min(pnl)
    }
}

fn rate(num: usize, den: usize) -> f64 {
    if den > 0 {
        num as f64 / den as f64
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::types::MarketId;

    #[test]
    fn closed_trade_stats_accumulate() {
        let mut pf = Portfolio::with_starting_cash(100.0);
        let ts = Instant::now();
        pf.apply_fill(MarketId(7), OrderAction::Buy, 0.40, 10.0, 0.0, ts);
        pf.apply_fill(MarketId(7), OrderAction::Sell, 0.55, 10.0, 0.0, ts);
        pf.apply_fill(MarketId(8), OrderAction::Buy, 0.60, 5.0, 0.0, ts);
        pf.apply_fill(MarketId(8), OrderAction::Sell, 0.50, 5.0, 0.0, ts);
        let stats = pf.closed_trade_stats();
        assert_eq!(stats.count, 2);
        assert_eq!(stats.wins, 1);
        assert_eq!(stats.losses, 1);
        assert!(stats.best_pnl > 1.4);
        assert!(stats.worst_pnl < -0.4);
    }
}
