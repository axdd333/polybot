use super::{World, PNL_TARGET_5M, PNL_WINDOW_SECS};
use crate::domain::analyzer;
use crate::domain::market::{book, quote};
use crate::domain::trading::planner;
use crate::domain::trading::snapshot::{MarketSnapshot, WorldSnapshot};
use std::time::{Duration, Instant};

impl World {
    pub fn snapshot(&self) -> WorldSnapshot {
        let mut markets: Vec<MarketSnapshot> = self
            .markets
            .values()
            .map(|tracked| {
                let market = &tracked.state;
                let intents = self
                    .portfolio
                    .surface(market.market_id)
                    .cloned()
                    .unwrap_or_else(|| tracked.planned_surface.clone())
                    .intents;
                let fair_series = analyzer::to_series(&tracked.runtime.fair_history);
                let edge_series = analyzer::to_series(&tracked.runtime.edge_history);
                let flow_series = analyzer::to_series(&tracked.runtime.flow_history);
                let micro_series = analyzer::to_series(&tracked.runtime.micro_history);
                let best_bid = book::best_bid(&market.book);
                let best_ask = book::best_ask(&market.book);
                let bid_size = market
                    .book
                    .bids
                    .first()
                    .map(|level| quote::decimal_to_f64(level.size))
                    .unwrap_or(0.0);
                let ask_size = market
                    .book
                    .asks
                    .first()
                    .map(|level| quote::decimal_to_f64(level.size))
                    .unwrap_or(0.0);
                let (position_qty, avg_entry, unrealized_pnl) = self
                    .portfolio
                    .position(market.market_id)
                    .map(|p| (p.qty, p.avg_px, p.unrealized))
                    .unwrap_or((0.0, 0.0, 0.0));

                MarketSnapshot {
                    market_id: market.market_id,
                    title: tracked.runtime.title.clone(),
                    symbol: tracked.runtime.symbol.clone(),
                    window_label: tracked.runtime.window_label.clone(),
                    end_label: tracked.runtime.end_label.clone(),
                    side: market.side,
                    regime: market.regime,
                    score: market.model_score,
                    fair_value: market.fair_value,
                    edge_buy: market.edge_buy,
                    edge_sell: market.edge_sell,
                    best_bid,
                    best_ask,
                    bid_size,
                    ask_size,
                    spread_ticks: market.spread_ticks,
                    microprice: market.microprice,
                    trade_intensity: market.trade_intensity,
                    burstiness: market.burstiness,
                    torsion: market.cross_window_torsion,
                    void_score: market.liquidity_void_score,
                    wall_score: market.wall_persistence_score,
                    expiry_secs: market
                        .time_to_expiry
                        .saturating_sub(market.expiry_anchored_at.elapsed())
                        .as_secs(),
                    position_qty,
                    avg_entry,
                    unrealized_pnl,
                    features: tracked.features.clone(),
                    intents,
                    fair_series,
                    edge_series,
                    flow_series,
                    micro_series,
                }
            })
            .collect();

        markets.sort_by(|a, b| {
            planner::market_sort_key(b, self.strategy.max_entry_price)
                .partial_cmp(&planner::market_sort_key(a, self.strategy.max_entry_price))
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let journal_tail: Vec<String> = self.journal.iter().rev().take(10).cloned().collect();
        let equity = self.portfolio_equity();
        let unrealized_pnl = self.portfolio.total_unrealized();
        let snapshot_now = self
            .markets
            .values()
            .map(|tracked| tracked.state.book.last_update)
            .max()
            .unwrap_or_else(Instant::now);
        let realized_pnl_5m = self
            .portfolio
            .realized_over_window(snapshot_now, Duration::from_secs(PNL_WINDOW_SECS));
        let pnl_shortfall_5m = (PNL_TARGET_5M - realized_pnl_5m).max(0.0);
        let open_positions = markets
            .iter()
            .filter(|market| market.position_qty > 0.0)
            .count();
        let eligible_markets = markets
            .iter()
            .filter(|market| {
                market.position_qty <= 0.0
                    && quote::valid_live_quote(market.best_bid, market.best_ask)
                    && market.best_ask <= self.strategy.max_entry_price
                    && self
                        .evaluate_entry_gate(market.market_id, market.best_bid, market.best_ask)
                        .is_none()
            })
            .count();

        let btc_spot = self
            .underlying_tapes
            .get("BTC")
            .and_then(|tape| tape.back().map(|point| point.px))
            .unwrap_or(0.0);

        let signal_strength = self
            .markets
            .keys()
            .filter_map(|&market_id| self.momentum_signal(market_id))
            .map(|(_, signal)| signal)
            .fold(0.0_f64, f64::max);

        WorldSnapshot {
            markets,
            journal_tail,
            cash: self.portfolio.cash,
            equity,
            realized_pnl: self.portfolio.realized_pnl,
            unrealized_pnl,
            open_positions,
            kill_switch: self.portfolio.kill_switch,
            stream_splits: self.portfolio.stream_splits,
            btc_spot,
            signal_strength,
            realized_pnl_5m,
            pnl_shortfall_5m,
            eligible_markets,
            ticket_dollars: self.strategy.ticket_dollars,
            entry_threshold: self.strategy.max_entry_price,
            exit_threshold: self.strategy.take_profit_price,
            max_spread: self.strategy.max_spread,
            paper_real_mode: self.strategy.paper_real_mode,
        }
    }
}
