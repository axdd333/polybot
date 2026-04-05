use std::time::{Duration, Instant};
use trading_core::config::{AppProfile, ImpulseProfile, RunMode, SweepProfile};
use trading_core::engine::{EngineParts, TradingEngine};
use trading_core::executor::Executor;
use trading_core::state::TrackedMarket;
use trading_core::market::book;
use trading_core::market::quote;
use trading_core::market::types::{
    MarketId, MarketState, OrderAction, OrderIntent, OrderSurface, Side,
};
use trading_core::traits::{Strategy, StrategyContext};

use crate::planner::{
    directional_score, edge_summary, entry_has_exit_headroom,
    entry_has_fast_exit, entry_signal_ok, exit_plan, maker_fill_quality, regime_adjust,
};
pub use crate::risk::SweepRiskPolicy;

pub fn build_engine(
    profile: &AppProfile,
    mode: RunMode,
    executor: Box<dyn Executor>,
) -> TradingEngine {
    TradingEngine::new(EngineParts {
        mode,
        starting_cash: profile.sweep.starting_cash,
        model_weights: profile.model_weights.clone(),
        strategy: Box::new(SweepStrategy::new(profile.sweep.clone())),
        risk_policy: Box::new(SweepRiskPolicy::new(profile.risk.clone())),
        executor,
    })
}

#[derive(Debug, Clone)]
pub struct SweepStrategy {
    pub config: SweepProfile,
}

#[derive(Clone, Copy)]
struct EntryMode {
    aggressive: bool,
}

#[derive(Clone)]
struct ReversalEntry {
    score: f64,
}

#[derive(Clone, Copy)]
struct ImpulseView {
    vel: f64,
    edge: f64,
}

impl SweepStrategy {
    pub fn new(config: SweepProfile) -> Self {
        Self { config }
    }

    fn entry_gate(&self, ctx: &StrategyContext<'_>, market: &MarketState) -> Option<String> {
        let bid = book::best_bid(&market.book);
        let ask = book::best_ask(&market.book);
        let tune = regime_adjust(&self.config, market.regime);
        let edge = edge_summary(&self.config, market);
        let score = self.opportunity_score(market, edge.net_buy);
        let mode = self.entry_mode(score);
        if self.open_positions(ctx) >= self.config.max_open_positions {
            return Some("blocked: max open positions".to_string());
        }
        if tune.exit_only {
            return Some("blocked: regime is exit-only".to_string());
        }
        if self.expiry_exit_only(market) {
            return Some("blocked: expiry clamp".to_string());
        }
        if !quote::valid_live_quote(bid, ask) {
            return Some("blocked: invalid quote".to_string());
        }
        if bid <= 0.0 {
            return Some("blocked: no bid support".to_string());
        }
        if !self.depth_ok(ctx, market, mode) {
            return Some("blocked: thin top-of-book".to_string());
        }
        if self.has_negative_selection(market) {
            return Some("blocked: unstable microstructure".to_string());
        }
        if !self.spread_ok(bid, ask, ask - bid, &tune, edge.net_buy, mode) {
            return Some("blocked: spread regime gate".to_string());
        }
        if ask > self.config.max_entry_price || ask < self.config.min_entry_price {
            return Some("blocked: ask outside entry band".to_string());
        }
        if !self.tick_ok(ask, market.min_tick_size) {
            return Some("blocked: coarse tick economics".to_string());
        }
        if !self.exit_headroom_ok(market, mode) {
            return Some("blocked: fair below viable exit".to_string());
        }
        if market.min_order_size > 0.0 && self.entry_qty(ctx, market) + 1e-9 < market.min_order_size
        {
            return Some("blocked: ticket below min order size".to_string());
        }
        if market.edge_buy <= 0.0 {
            return Some("blocked: raw edge below regime floor".to_string());
        }
        if !self.net_edge_ok(market, edge.net_buy, &tune, mode) {
            return Some("blocked: net edge below cost floor".to_string());
        }
        if !self.fair_gap_ok(market, edge.fair_gap, mode) {
            return Some("blocked: fair gap below cost floor".to_string());
        }
        if !self.model_ok(market, mode) {
            return Some("blocked: model conviction weak".to_string());
        }
        if !self.opportunity_ok(score, mode) {
            return Some("blocked: opportunity score weak".to_string());
        }
        if !entry_signal_ok(
            bid,
            ask,
            market.microprice,
            market.edge_buy.max(edge.net_buy),
        ) {
            return Some("blocked: weak micro/edge signal".to_string());
        }
        if self.passive_entry(market, mode)
            && !maker_fill_quality(&self.config, market).maker_ok
        {
            return Some("blocked: weak maker queue".to_string());
        }
        self.pair_gate(ctx, ask, market)
    }

    fn flow_pressure(&self, ctx: &StrategyContext<'_>) -> bool {
        let win = Duration::from_secs(self.config.flow_window_secs);
        let pnl = ctx.portfolio.realized_per_min(ctx.now, win);
        let cyc = ctx.portfolio.closed_trades_per_min(ctx.now, win);
        pnl < self.config.flow_target_per_min || cyc < self.config.min_cycle_rate_per_min
    }

    fn flow_entry(&self, ctx: &StrategyContext<'_>, market: &MarketState) -> bool {
        let bid = book::best_bid(&market.book);
        let ask = book::best_ask(&market.book);
        let mid = (bid + ask) * 0.5;
        let edge = edge_summary(&self.config, market);
        if !self.flow_pressure(ctx) || !quote::valid_live_quote(bid, ask) {
            return false;
        }
        if self.open_positions(ctx) >= self.config.max_open_positions {
            return false;
        }
        if self.expiry_exit_only(market) || self.has_negative_selection(market) {
            return false;
        }
        if !self.depth_ok(ctx, market, EntryMode { aggressive: false }) {
            return false;
        }
        if !maker_fill_quality(&self.config, market).maker_ok {
            return false;
        }
        if (mid - 0.5).abs() > self.config.maker_band_dist {
            return false;
        }
        if market.edge_buy >= self.config.min_edge_to_buy * 2.5 {
            return false;
        }
        if edge.net_buy < self.config.flow_reversion_edge {
            return false;
        }
        if market.fair_value < bid + self.config.flow_reversion_edge {
            return false;
        }
        if market.trade_intensity < self.config.min_queue_trade_intensity {
            return false;
        }
        self.pair_gate(ctx, ask, market).is_none()
    }

    fn impulse_entry(
        &self,
        ctx: &StrategyContext<'_>,
        market: &MarketState,
    ) -> Option<(OrderSurface, Option<String>)> {
        if !self.impulse_ready(ctx, market) {
            return None;
        }
        let qty = self.impulse_qty(ctx, market);
        let ask = book::best_ask(&market.book);
        let note = format!("impulse buy qty {:.1} px {}", qty, quote::cents_or_dash(ask));
        Some((self.buy_surface(ctx.market_id, market.side, ask, qty), Some(note)))
    }

    fn flow_qty(&self, ctx: &StrategyContext<'_>, market: &MarketState) -> f64 {
        self.entry_qty(ctx, market) * self.config.flow_size_mult
    }

    fn flow_exit(
        &self,
        ctx: &StrategyContext<'_>,
        market: &MarketState,
    ) -> Option<(OrderSurface, Option<String>)> {
        let pos = ctx.portfolio.position(ctx.market_id)?;
        let bid = book::best_bid(&market.book);
        if bid <= 0.0 {
            return None;
        }
        let hold = pos
            .opened_at
            .map(|ts| ctx.now.duration_since(ts).as_secs())
            .unwrap_or(0);
        let green = bid >= pos.avg_px + self.config.flow_exit_edge;
        let timed = hold >= self.config.flow_max_hold_secs;
        if !self.flow_pressure(ctx) || (!green && !timed) {
            return None;
        }
        let note = format!(
            "exit flow qty {:.1} px {}",
            pos.qty.abs(),
            quote::cents_or_dash(bid)
        );
        let intent = OrderIntent {
            market_id: ctx.market_id,
            side: market.side,
            action: OrderAction::Sell,
            price: bid,
            qty: pos.qty.abs(),
            ttl_ms: 120,
            aggressive: true,
        };
        Some((
            OrderSurface {
                intents: vec![intent],
            },
            Some(note),
        ))
    }

    fn impulse_fade_exit(
        &self,
        ctx: &StrategyContext<'_>,
        market: &MarketState,
    ) -> Option<(OrderSurface, Option<String>)> {
        let pos = ctx.portfolio.position(ctx.market_id)?;
        let view = self.impulse_view(ctx, market)?;
        let hold = pos
            .opened_at
            .map(|ts| ctx.now.duration_since(ts).as_secs())
            .unwrap_or(0);
        if !self.fade_exit_ok(hold, market, view) {
            return None;
        }
        let bid = book::best_bid(&market.book);
        let note = format!("impulse fade exit qty {:.1} px {}", pos.qty, bid);
        Some((self.sell_surface(ctx.market_id, market.side, bid, pos.qty), Some(note)))
    }

    fn impulse_fade_entry(
        &self,
        ctx: &StrategyContext<'_>,
        market: &MarketState,
    ) -> Option<(OrderSurface, Option<String>)> {
        if !self.fade_entry_ok(ctx, market) {
            return None;
        }
        let qty = self.impulse_qty(ctx, market);
        let ask = book::best_ask(&market.book);
        let note = format!("impulse fade buy qty {:.1} px {}", qty, ask);
        Some((self.buy_surface(ctx.market_id, market.side, ask, qty), Some(note)))
    }

    fn has_negative_selection(&self, market: &MarketState) -> bool {
        market.liquidity_void_score > self.config.max_void_score
            || market.wall_persistence_score < self.config.min_wall_score
            || market.cancel_skew.abs() > self.config.max_cancel_skew
    }

    fn reversal_entry(
        &self,
        ctx: &StrategyContext<'_>,
        market: &MarketState,
    ) -> Result<ReversalEntry, String> {
        let runtime = self
            .market_runtime(ctx)
            .ok_or_else(|| "blocked: reversal runtime missing".to_string())?;
        if !runtime.runtime.opening.done {
            return Err("blocked: opening range not sealed".to_string());
        }
        let draw = self.reversal_drawdown(market.side, &runtime.runtime.opening);
        if draw < bps(self.config.reversal.min_drawdown_bps) {
            return Err("blocked: opening washout too small".to_string());
        }
        let rebound = self.reversal_rebound(market.side, &runtime.runtime.opening);
        let frac = rebound / draw.max(1e-6);
        if rebound < bps(self.config.reversal.min_rebound_bps) {
            return Err("blocked: bounce too weak".to_string());
        }
        if frac < self.config.reversal.min_rebound_frac {
            return Err("blocked: bounce lacks follow-through".to_string());
        }
        let bias = self.side_wick_bias(market.side, runtime.runtime.prev_wick_bias);
        if bias < self.config.reversal.min_wick_bias {
            return Err("blocked: prior wick bias disagrees".to_string());
        }
        let score = self.reversal_score(draw, rebound, bias);
        if score < self.config.reversal.min_signal_score {
            return Err("blocked: reversal score weak".to_string());
        }
        Ok(ReversalEntry { score })
    }

    fn market_runtime<'a>(
        &self,
        ctx: &'a StrategyContext<'_>,
    ) -> Option<&'a trading_core::state::TrackedMarket> {
        ctx.markets.get(&ctx.market_id)
    }

    fn reversal_drawdown(&self, side: Side, opening: &trading_core::analyzer::OpeningRange) -> f64 {
        let open = opening.open_px.unwrap_or(0.0);
        match side {
            Side::Up => (open - opening.low_px).max(0.0),
            Side::Down => (opening.high_px - open).max(0.0),
        }
    }

    fn reversal_rebound(&self, side: Side, opening: &trading_core::analyzer::OpeningRange) -> f64 {
        match side {
            Side::Up => (opening.last_px - opening.low_px).max(0.0),
            Side::Down => (opening.high_px - opening.last_px).max(0.0),
        }
    }

    fn side_wick_bias(&self, side: Side, bias: f64) -> f64 {
        match side {
            Side::Up => bias,
            Side::Down => -bias,
        }
    }

    fn reversal_score(&self, draw: f64, rebound: f64, bias: f64) -> f64 {
        let draw_unit = (draw / bps(self.config.reversal.min_drawdown_bps)).clamp(0.0, 2.0);
        let bounce = (rebound / draw.max(1e-6)).clamp(0.0, 1.5);
        let wick = (bias / self.config.reversal.min_wick_bias.max(1e-6)).clamp(0.0, 1.5);
        (draw_unit * 0.35) + (bounce * 0.35) + (wick * 0.30)
    }

    fn impulse_cfg(&self) -> &ImpulseProfile {
        &self.config.impulse
    }

    fn impulse_ready(&self, ctx: &StrategyContext<'_>, market: &MarketState) -> bool {
        let ask = book::best_ask(&market.book);
        let bid = book::best_bid(&market.book);
        self.impulse_view(ctx, market).is_some()
            && self.open_positions(ctx) < self.config.max_open_positions
            && !self.expiry_exit_only(market)
            && !self.has_negative_selection(market)
            && quote::valid_live_quote(bid, ask)
            && self.tick_ok(ask, market.min_tick_size)
            && self.depth_ok(ctx, market, EntryMode { aggressive: true })
            && self.pair_gate(ctx, ask, market).is_none()
    }

    fn impulse_view(
        &self,
        ctx: &StrategyContext<'_>,
        market: &MarketState,
    ) -> Option<ImpulseView> {
        let cfg = self.impulse_cfg();
        let tracked = self.market_runtime(ctx)?;
        let shock = self.shock_score(market);
        let vel = trading_core::analyzer::history_velocity(
            &tracked.runtime.price_history,
            ctx.now,
            Duration::from_millis(cfg.velocity_window_ms),
        );
        let edge = (book::best_bid(&market.book) - market.fair_value).max(0.0);
        if shock < cfg.min_ret_250ms || self.slow_shock(market) < cfg.min_ret_1s {
            return None;
        }
        if self.shock_accel(market) < cfg.min_accel || market.trade_intensity < cfg.min_trade_intensity {
            return None;
        }
        Some(ImpulseView { vel, edge })
    }

    fn shock_score(&self, market: &MarketState) -> f64 {
        let fast = directional_score(market.side, market.underlying.ret_250ms);
        let slow = directional_score(market.side, market.underlying.ret_1s);
        (fast * 0.65) + (slow * 0.35)
    }

    fn slow_shock(&self, market: &MarketState) -> f64 {
        directional_score(market.side, market.underlying.ret_1s)
    }

    fn shock_accel(&self, market: &MarketState) -> f64 {
        directional_score(market.side, market.underlying.accel)
    }

    fn impulse_qty(&self, ctx: &StrategyContext<'_>, market: &MarketState) -> f64 {
        let qty = self.entry_qty(ctx, market) * self.impulse_cfg().size_mult;
        qty.max(market.min_order_size).min(ctx.portfolio.cash / book::best_ask(&market.book))
    }

    fn fade_exit_ok(
        &self,
        hold: u64,
        market: &MarketState,
        view: ImpulseView,
    ) -> bool {
        hold <= self.impulse_cfg().max_hold_secs
            && view.edge >= self.impulse_cfg().min_overshoot_edge
            && view.vel >= self.impulse_cfg().min_velocity
            && market.imbalance_5lvl >= self.impulse_cfg().min_peak_imbalance
            && market.wall_persistence_score >= self.impulse_cfg().min_peak_wall_score
    }

    fn fade_entry_ok(&self, ctx: &StrategyContext<'_>, market: &MarketState) -> bool {
        let ask = book::best_ask(&market.book);
        let bid = book::best_bid(&market.book);
        let Some(mate) = self.pair_market(ctx) else {
            return false;
        };
        let Some(view) = self.pair_impulse_view(ctx, mate) else {
            return false;
        };
        quote::valid_live_quote(bid, ask)
            && view.edge >= self.impulse_cfg().fade_reentry_edge
            && edge_summary(&self.config, market).net_buy >= self.impulse_cfg().fade_reentry_edge
            && self.pair_gate(ctx, ask, market).is_none()
    }

    fn pair_market<'a>(&self, ctx: &'a StrategyContext<'_>) -> Option<&'a TrackedMarket> {
        let cur = ctx.markets.get(&ctx.market_id)?;
        self.pair_market_ids(ctx).into_iter().find_map(|id| {
            let other = ctx.markets.get(&id)?;
            (other.state.side != cur.state.side).then_some(other)
        })
    }

    fn pair_impulse_view(
        &self,
        ctx: &StrategyContext<'_>,
        mate: &TrackedMarket,
    ) -> Option<ImpulseView> {
        let sub = StrategyContext {
            market_id: mate.state.market_id,
            markets: ctx.markets,
            portfolio: ctx.portfolio,
            now: ctx.now,
        };
        self.impulse_view(&sub, &mate.state)
    }

    fn buy_surface(
        &self,
        market_id: MarketId,
        side: Side,
        price: f64,
        qty: f64,
    ) -> OrderSurface {
        OrderSurface {
            intents: vec![OrderIntent {
                market_id,
                side,
                action: OrderAction::Buy,
                price,
                qty,
                ttl_ms: self.impulse_cfg().entry_ttl_ms,
                aggressive: true,
            }],
        }
    }

    fn sell_surface(
        &self,
        market_id: MarketId,
        side: Side,
        price: f64,
        qty: f64,
    ) -> OrderSurface {
        OrderSurface {
            intents: vec![OrderIntent {
                market_id,
                side,
                action: OrderAction::Sell,
                price,
                qty,
                ttl_ms: self.impulse_cfg().fade_ttl_ms,
                aggressive: true,
            }],
        }
    }

    fn depth_ok(&self, ctx: &StrategyContext<'_>, market: &MarketState, mode: EntryMode) -> bool {
        let bid_sz = market
            .book
            .bids
            .first()
            .map(|lvl| quote::decimal_to_f64(lvl.size))
            .unwrap_or(0.0);
        let ask_sz = market
            .book
            .asks
            .first()
            .map(|lvl| quote::decimal_to_f64(lvl.size))
            .unwrap_or(0.0);
        let vis = book::imbalance(&market.book.bids.iter().take(5).cloned().collect::<Vec<_>>())
            + book::imbalance(&market.book.asks.iter().take(5).cloned().collect::<Vec<_>>());
        let need = self
            .entry_qty(ctx, market)
            .max(market.min_order_size)
            .max(1.0);
        let bid_need = self.config.min_bid_size.min(need * 1.5);
        let vis_need = if mode.aggressive {
            need * 2.0
        } else {
            self.config.min_visible_depth.min(need * 4.0)
        };
        let top_ok = if mode.aggressive {
            ask_sz >= (need * 0.75) && bid_sz >= (need * 0.25)
        } else {
            bid_sz >= bid_need
        };
        top_ok && vis >= vis_need
    }

    fn spread_ok(
        &self,
        bid: f64,
        ask: f64,
        spread: f64,
        tune: &trading_core::config::RegimeAdjust,
        net_buy: f64,
        mode: EntryMode,
    ) -> bool {
        let cap = self.config.max_spread * tune.spread_mult;
        if spread < 0.0 {
            return false;
        }
        let rel = if ask > 0.0 { spread / ask } else { 99.0 };
        spread <= cap
            || (mode.aggressive && rel <= 0.8)
            || (mode.aggressive && bid > 0.0 && spread <= net_buy * 1.5)
    }

    fn expiry_exit_only(&self, market: &MarketState) -> bool {
        market.time_to_expiry.as_secs() <= self.config.no_new_entry_expiry_secs
    }

    fn tick_ok(&self, ask: f64, tick: f64) -> bool {
        ask > 0.0 && tick / ask <= self.config.max_tick_frac
    }

    fn passive_entry(&self, market: &MarketState, mode: EntryMode) -> bool {
        !mode.aggressive
            && market.time_to_expiry.as_secs() > self.config.no_new_entry_expiry_secs + 30
    }

    fn entry_mode(&self, score: f64) -> EntryMode {
        EntryMode {
            aggressive: score >= self.config.aggressive_entry_score,
        }
    }

    fn opportunity_score(&self, market: &MarketState, net_buy: f64) -> f64 {
        let edge = (net_buy * 120.0).clamp(0.0, 1.2);
        let model = directional_score(market.side, market.model_score).clamp(0.0, 1.5);
        let micro = ((market.microprice - book::best_bid(&market.book)) * 100.0).clamp(0.0, 1.0);
        let flow = (market.trade_intensity / 6.0).clamp(0.0, 1.0);
        let void = (1.0 - market.liquidity_void_score).clamp(0.0, 1.0);
        (edge * 0.4) + (model * 0.25) + (micro * 0.15) + (flow * 0.1) + (void * 0.1)
    }

    fn exit_headroom_ok(&self, market: &MarketState, mode: EntryMode) -> bool {
        entry_has_exit_headroom(&self.config, market)
            || (mode.aggressive && entry_has_fast_exit(&self.config, market))
            || self.raw_exit_edge_ok(market)
    }

    fn opportunity_ok(&self, score: f64, mode: EntryMode) -> bool {
        let floor = if mode.aggressive {
            self.config.aggressive_entry_score
        } else {
            self.config.min_opportunity_score
        };
        score >= floor
    }

    fn net_edge_ok(
        &self,
        market: &MarketState,
        net_buy: f64,
        tune: &trading_core::config::RegimeAdjust,
        mode: EntryMode,
    ) -> bool {
        let ask = book::best_ask(&market.book);
        let need = self.entry_cost_floor(ask, self.config.min_net_edge_buy * tune.edge_mult);
        net_buy >= need || (mode.aggressive && market.edge_buy >= need)
    }

    fn fair_gap_ok(&self, market: &MarketState, fair_gap: f64, mode: EntryMode) -> bool {
        let ask = book::best_ask(&market.book);
        let need = self.entry_cost_floor(ask, self.config.min_fair_gap_after_cost);
        fair_gap >= need
            || (mode.aggressive && market.edge_buy >= self.config.min_fair_gap_after_cost * 2.0)
    }

    fn model_ok(&self, market: &MarketState, mode: EntryMode) -> bool {
        let score = directional_score(market.side, market.model_score);
        score >= self.config.min_model_score
            || (mode.aggressive && market.edge_buy >= self.config.min_edge_to_buy * 2.0)
    }

    fn entry_cost_floor(&self, ask: f64, base: f64) -> f64 {
        let room = (self.config.take_profit_price - ask).max(0.0) * 0.25;
        base.min(room)
    }

    fn raw_exit_edge_ok(&self, market: &MarketState) -> bool {
        market.edge_buy > 0.0
    }

    fn entry_qty(&self, ctx: &StrategyContext<'_>, market: &MarketState) -> f64 {
        let ask = book::best_ask(&market.book);
        if ask <= 0.0 {
            return 0.0;
        }
        let dollars = self.ticket_dollars_for(market);
        let cash = (ctx.portfolio.cash / ask).max(0.0);
        let qty = (dollars / ask).min(cash);
        if market.min_order_size > 0.0 && cash >= market.min_order_size {
            return qty.max(market.min_order_size);
        }
        qty
    }

    fn ticket_dollars_for(&self, market: &MarketState) -> f64 {
        let tune = regime_adjust(&self.config, market.regime);
        let edge = edge_summary(&self.config, market);
        let score = directional_score(market.side, market.model_score).max(0.0);
        let spread = (1.0 - (market.spread_ticks / 5.0)).clamp(0.25, 1.25);
        let void = (1.0 - market.liquidity_void_score).clamp(0.2, 1.2);
        let expiry = if market.time_to_expiry.as_secs() <= self.config.reduce_size_expiry_secs {
            0.4
        } else {
            1.0
        };
        let conf = (0.5 + score + edge.net_buy.max(0.0) * 40.0) * spread * void * expiry;
        self.config.ticket_dollars * tune.ticket_mult * conf.clamp(0.25, 2.0)
    }

    fn exit_surface(
        &self,
        ctx: StrategyContext<'_>,
        market: &MarketState,
    ) -> (OrderSurface, Option<String>) {
        let Some(pos) = ctx.portfolio.position(ctx.market_id) else {
            return (OrderSurface::default(), None);
        };
        let stale = self.stale_thesis(pos, ctx.now, market.fair_value);
        let expired = self.expired_hold(pos, ctx.now);
        let plan = exit_plan(
            &self.config,
            market,
            pos.avg_px,
            pos.unrealized,
            pos.max_favorable_excursion,
            stale,
            expired,
        );
        let intent = OrderIntent {
            market_id: ctx.market_id,
            side: market.side,
            action: OrderAction::Sell,
            price: plan.price,
            qty: pos.qty.abs(),
            ttl_ms: plan.ttl_ms,
            aggressive: plan.aggressive,
        };
        let note = format!(
            "exit {} qty {:.1} px {}",
            plan.reason,
            pos.qty.abs(),
            quote::cents_or_dash(plan.price)
        );
        (
            OrderSurface {
                intents: vec![intent],
            },
            Some(note),
        )
    }

    fn stale_thesis(
        &self,
        pos: &trading_core::market::types::PositionState,
        now: Instant,
        fair: f64,
    ) -> bool {
        if fair >= pos.best_fair - self.config.min_fair_improve {
            return false;
        }
        pos.last_fair_improve_at
            .map(|ts| {
                now.duration_since(ts) >= Duration::from_secs(self.config.max_stale_fair_secs)
            })
            .unwrap_or(false)
    }

    fn expired_hold(&self, pos: &trading_core::market::types::PositionState, now: Instant) -> bool {
        pos.opened_at
            .map(|ts| now.duration_since(ts) >= Duration::from_secs(self.config.max_hold_secs))
            .unwrap_or(false)
    }

    fn pair_gate(
        &self,
        ctx: &StrategyContext<'_>,
        ask: f64,
        market: &MarketState,
    ) -> Option<String> {
        let pair = self.pair_market_ids(ctx);
        if pair.len() <= 1 {
            return None;
        }
        if pair
            .iter()
            .copied()
            .any(|id| id != ctx.market_id && ctx.portfolio.inventory_for(id).abs() > 0.0)
        {
            return Some("blocked: paired side already open".to_string());
        }
        let sum: f64 = pair
            .iter()
            .filter_map(|id| ctx.markets.get(id).map(|m| book::best_ask(&m.state.book)))
            .filter(|px| *px > 0.0)
            .sum();
        if sum > self.config.max_pair_ask_sum {
            return Some("blocked: pair ask sum too high".to_string());
        }
        let best = pair
            .iter()
            .filter_map(|id| {
                ctx.markets
                    .get(id)
                    .map(|m| (*id, edge_summary(&self.config, &m.state).net_buy))
            })
            .max_by(|a, b| a.1.total_cmp(&b.1));
        match best {
            Some((id, _)) if id != ctx.market_id => {
                Some("blocked: weaker side in pair".to_string())
            }
            Some(_) if ask <= 0.0 || market.edge_buy <= 0.0 => {
                Some("blocked: pair edge invalid".to_string())
            }
            _ => None,
        }
    }

    fn open_positions(&self, ctx: &StrategyContext<'_>) -> usize {
        ctx.markets
            .keys()
            .filter(|id| ctx.portfolio.inventory_for(**id).abs() > 0.0)
            .count()
    }

    fn pair_market_ids(&self, ctx: &StrategyContext<'_>) -> Vec<MarketId> {
        let Some(cur) = ctx.markets.get(&ctx.market_id) else {
            return vec![ctx.market_id];
        };
        let mut ids: Vec<_> = ctx
            .markets
            .iter()
            .filter(|(_, other)| {
                other.runtime.symbol == cur.runtime.symbol
                    && other.runtime.window_label == cur.runtime.window_label
                    && other.runtime.title == cur.runtime.title
            })
            .map(|(id, _)| *id)
            .collect();
        ids.sort_by_key(|id| id.0);
        ids
    }
}

impl Strategy for SweepStrategy {
    fn plan(
        &self,
        ctx: StrategyContext<'_>,
        market: &MarketState,
    ) -> (OrderSurface, Option<String>) {
        if ctx.portfolio.inventory_for(ctx.market_id).abs() > 0.0 {
            if let Some(surface) = self.impulse_fade_exit(&ctx, market) {
                return surface;
            }
            if let Some(surface) = self.flow_exit(&ctx, market) {
                return surface;
            }
            return self.exit_surface(ctx, market);
        }
        if let Some(surface) = self.impulse_fade_entry(&ctx, market) {
            return surface;
        }
        if let Some(surface) = self.impulse_entry(&ctx, market) {
            return surface;
        }
        if self.flow_entry(&ctx, market) {
            let bid = book::best_bid(&market.book);
            let qty = self.flow_qty(&ctx, market);
            if qty > 0.0 {
                let note = format!("flow buy qty {:.1} px {}", qty, quote::cents_or_dash(bid));
                let intent = OrderIntent {
                    market_id: ctx.market_id,
                    side: market.side,
                    action: OrderAction::Buy,
                    price: bid,
                    qty,
                    ttl_ms: 1_200,
                    aggressive: false,
                };
                return (
                    OrderSurface {
                        intents: vec![intent],
                    },
                    Some(note),
                );
            }
        }
        if let Some(reason) = self.entry_gate(&ctx, market) {
            return (OrderSurface::default(), Some(reason));
        }
        let qty = self.entry_qty(&ctx, market);
        if qty <= 0.0 {
            return (
                OrderSurface::default(),
                Some("blocked: insufficient cash".to_string()),
            );
        }
        let ask = book::best_ask(&market.book);
        let bid = book::best_bid(&market.book);
        let dollars = self.ticket_dollars_for(market);
        let edge = edge_summary(&self.config, market);
        let mode = self.entry_mode(self.opportunity_score(market, edge.net_buy));
        let reversal = self.reversal_entry(&ctx, market).ok();
        let near_close = reversal.is_some() || !self.passive_entry(market, mode);
        let px = if near_close || bid <= 0.0 { ask } else { bid };
        let note = format!(
            "buy qty {:.1} px {} ticket ${:.2}{}",
            qty,
            quote::cents_or_dash(px),
            dollars,
            reversal
                .as_ref()
                .map(|rev| format!(" rev {:.2}", rev.score))
                .unwrap_or_default()
        );
        let intent = OrderIntent {
            market_id: ctx.market_id,
            side: market.side,
            action: OrderAction::Buy,
            price: px,
            qty,
            ttl_ms: reversal
                .as_ref()
                .map(|_| self.config.reversal.ttl_ms)
                .unwrap_or_else(|| if near_close { 120 } else { 2_500 }),
            aggressive: reversal.is_some() || near_close,
        };
        (
            OrderSurface {
                intents: vec![intent],
            },
            Some(note),
        )
    }

    fn sort_key(&self, market: &trading_core::snapshot::MarketSnapshot) -> f64 {
        crate::planner::market_sort_key(market, self.config.max_entry_price)
    }
}

fn bps(value: f64) -> f64 {
    value / 10_000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use std::collections::HashMap;
    use trading_core::analyzer::OpeningRange;
    use trading_core::market::types::{L2Level, MarketId, Side};
    use trading_core::portfolio::Portfolio;
    use trading_core::state::TrackedMarket;

    #[test]
    fn expiry_zone_blocks_new_entry() {
        let strategy = SweepStrategy::new(SweepProfile::default());
        let market_id = MarketId(7);
        let mut market = MarketState::new(market_id, Side::Up, Duration::from_secs(20));
        market.book.bids.push(L2Level {
            price: dec!(0.50),
            size: dec!(8),
        });
        market.book.asks.push(L2Level {
            price: dec!(0.51),
            size: dec!(8),
        });
        market.model_score = 1.2;
        market.fair_value = 0.56;
        market.edge_buy = 0.05;
        let tracked = TrackedMarket::placeholder(market_id);
        let mut markets = HashMap::new();
        markets.insert(market_id, tracked);
        let portfolio = Portfolio::with_starting_cash(10.0);
        let ctx = StrategyContext {
            market_id,
            markets: &markets,
            portfolio: &portfolio,
            now: Instant::now(),
        };
        let (_, note) = strategy.plan(ctx, &market);
        assert!(note.unwrap().contains("expiry"));
    }

    #[test]
    fn entry_scales_to_min_order_size() {
        let strategy = SweepStrategy::new(SweepProfile::default());
        let market_id = MarketId(9);
        let mut market = MarketState::new(market_id, Side::Up, Duration::from_secs(60));
        market.book.bids.push(L2Level {
            price: dec!(0.84),
            size: dec!(100),
        });
        market.book.asks.push(L2Level {
            price: dec!(0.85),
            size: dec!(100),
        });
        market.model_score = 1.2;
        market.fair_value = 0.95;
        market.edge_buy = 0.10;
        market.microprice = 0.849;
        market.trade_intensity = 3.0;
        market.wall_persistence_score = 0.5;
        market.liquidity_void_score = 0.1;
        market.cancel_skew = 0.0;
        market.imbalance_5lvl = 0.2;
        market.min_order_size = 5.0;
        let mut tracked = TrackedMarket::placeholder(market_id);
        tracked.state = market.clone();
        let mut markets = HashMap::new();
        markets.insert(market_id, tracked);
        let portfolio = Portfolio::with_starting_cash(10.0);
        let ctx = StrategyContext {
            market_id,
            markets: &markets,
            portfolio: &portfolio,
            now: Instant::now(),
        };
        let (surface, _) = strategy.plan(ctx, &market);
        assert_eq!(surface.intents.len(), 1);
        assert!(surface.intents[0].qty >= 5.0);
    }

    #[test]
    fn thin_book_is_blocked() {
        let strategy = SweepStrategy::new(SweepProfile::default());
        let market_id = MarketId(10);
        let mut market = MarketState::new(market_id, Side::Up, Duration::from_secs(60));
        market.book.bids.push(L2Level {
            price: dec!(0.40),
            size: dec!(0.2),
        });
        market.book.asks.push(L2Level {
            price: dec!(0.41),
            size: dec!(0.2),
        });
        market.model_score = 1.0;
        market.fair_value = 0.55;
        market.edge_buy = 0.14;
        market.microprice = 0.405;
        market.trade_intensity = 5.0;
        market.wall_persistence_score = 0.5;
        let mut tracked = TrackedMarket::placeholder(market_id);
        tracked.state = market.clone();
        let mut markets = HashMap::new();
        markets.insert(market_id, tracked);
        let portfolio = Portfolio::with_starting_cash(20.0);
        let ctx = StrategyContext {
            market_id,
            markets: &markets,
            portfolio: &portfolio,
            now: Instant::now(),
        };
        let (surface, note) = strategy.plan(ctx, &market);
        assert!(surface.intents.is_empty());
        assert!(note.unwrap().contains("thin top-of-book"));
    }

    #[test]
    fn flow_entry_posts_passive_bid() {
        let cfg = SweepProfile {
            min_opportunity_score: 1.2,
            ..SweepProfile::default()
        };
        let strategy = SweepStrategy::new(cfg);
        let market_id = MarketId(16);
        let mut market = MarketState::new(market_id, Side::Up, Duration::from_secs(120));
        market.book.bids.push(L2Level {
            price: dec!(0.49),
            size: dec!(120),
        });
        market.book.asks.push(L2Level {
            price: dec!(0.50),
            size: dec!(120),
        });
        market.fair_value = 0.504;
        market.edge_buy = 0.004;
        market.microprice = 0.495;
        market.trade_intensity = 3.0;
        market.wall_persistence_score = 0.6;
        market.liquidity_void_score = 0.1;
        market.cancel_skew = 0.0;
        let mut tracked = TrackedMarket::placeholder(market_id);
        tracked.state = market.clone();
        let mut markets = HashMap::new();
        markets.insert(market_id, tracked);
        let portfolio = Portfolio::with_starting_cash(20.0);
        let ctx = StrategyContext {
            market_id,
            markets: &markets,
            portfolio: &portfolio,
            now: Instant::now(),
        };
        let (surface, note) = strategy.plan(ctx, &market);
        assert_eq!(surface.intents.len(), 1);
        assert_eq!(surface.intents[0].price, 0.49);
        assert!(!surface.intents[0].aggressive);
        assert!(note.unwrap().contains("flow buy"));
    }

    #[test]
    fn coarse_tick_market_is_blocked() {
        let strategy = SweepStrategy::new(SweepProfile::default());
        let market_id = MarketId(11);
        let mut market = MarketState::new(market_id, Side::Up, Duration::from_secs(60));
        market.book.bids.push(L2Level {
            price: dec!(0.23),
            size: dec!(100),
        });
        market.book.asks.push(L2Level {
            price: dec!(0.24),
            size: dec!(100),
        });
        market.model_score = 0.8;
        market.fair_value = 0.30;
        market.edge_buy = 0.06;
        market.microprice = 0.235;
        market.trade_intensity = 4.0;
        market.wall_persistence_score = 0.5;
        market.liquidity_void_score = 0.1;
        market.cancel_skew = 0.0;
        market.imbalance_5lvl = 0.3;
        let mut tracked = TrackedMarket::placeholder(market_id);
        tracked.state = market.clone();
        let mut markets = HashMap::new();
        markets.insert(market_id, tracked);
        let portfolio = Portfolio::with_starting_cash(20.0);
        let ctx = StrategyContext {
            market_id,
            markets: &markets,
            portfolio: &portfolio,
            now: Instant::now(),
        };
        let (surface, note) = strategy.plan(ctx, &market);
        assert!(surface.intents.is_empty());
        assert!(note.unwrap().contains("coarse tick economics"));
    }

    #[test]
    fn fee_heavy_trade_is_blocked_by_net_edge() {
        let strategy = SweepStrategy::new(SweepProfile::default());
        let market_id = MarketId(12);
        let mut market = MarketState::new(market_id, Side::Up, Duration::from_secs(600));
        market.book.bids.push(L2Level {
            price: dec!(0.49),
            size: dec!(200),
        });
        market.book.asks.push(L2Level {
            price: dec!(0.50),
            size: dec!(200),
        });
        market.model_score = 0.6;
        market.fair_value = 0.505;
        market.edge_buy = 0.015;
        market.microprice = 0.499;
        market.trade_intensity = 5.0;
        market.wall_persistence_score = 0.7;
        market.liquidity_void_score = 0.05;
        market.cancel_skew = 0.0;
        market.imbalance_5lvl = 0.3;
        market.maker_fee_bps = 40.0;
        market.taker_fee_bps = 80.0;
        let mut tracked = TrackedMarket::placeholder(market_id);
        tracked.state = market.clone();
        let mut markets = HashMap::new();
        markets.insert(market_id, tracked);
        let portfolio = Portfolio::with_starting_cash(20.0);
        let ctx = StrategyContext {
            market_id,
            markets: &markets,
            portfolio: &portfolio,
            now: Instant::now(),
        };
        let (surface, note) = strategy.plan(ctx, &market);
        assert!(surface.intents.is_empty());
        assert!(note.unwrap().contains("net edge below cost floor"));
    }

    #[test]
    fn strong_entry_crosses_ask() {
        let strategy = SweepStrategy::new(SweepProfile::default());
        let market_id = MarketId(13);
        let mut market = MarketState::new(market_id, Side::Up, Duration::from_secs(600));
        market.book.bids.push(L2Level {
            price: dec!(0.50),
            size: dec!(200),
        });
        market.book.asks.push(L2Level {
            price: dec!(0.51),
            size: dec!(200),
        });
        market.model_score = 1.0;
        market.fair_value = 0.56;
        market.edge_buy = 0.05;
        market.microprice = 0.509;
        market.trade_intensity = 6.0;
        market.wall_persistence_score = 0.8;
        market.liquidity_void_score = 0.03;
        market.cancel_skew = 0.0;
        market.imbalance_5lvl = 0.4;
        let mut tracked = TrackedMarket::placeholder(market_id);
        tracked.state = market.clone();
        let mut markets = HashMap::new();
        markets.insert(market_id, tracked);
        let portfolio = Portfolio::with_starting_cash(20.0);
        let ctx = StrategyContext {
            market_id,
            markets: &markets,
            portfolio: &portfolio,
            now: Instant::now(),
        };
        let (surface, _) = strategy.plan(ctx, &market);
        assert_eq!(surface.intents.len(), 1);
        assert_eq!(surface.intents[0].price, 0.51);
        assert!(surface.intents[0].aggressive);
    }

    #[test]
    fn strong_entry_bypasses_weak_queue() {
        let strategy = SweepStrategy::new(SweepProfile::default());
        let market_id = MarketId(14);
        let mut market = MarketState::new(market_id, Side::Up, Duration::from_secs(600));
        market.book.bids.push(L2Level {
            price: dec!(0.50),
            size: dec!(200),
        });
        market.book.asks.push(L2Level {
            price: dec!(0.51),
            size: dec!(20),
        });
        market.model_score = 1.0;
        market.fair_value = 0.56;
        market.edge_buy = 0.05;
        market.microprice = 0.508;
        market.trade_intensity = 0.1;
        market.wall_persistence_score = 0.18;
        market.liquidity_void_score = 0.03;
        market.cancel_skew = 0.8;
        market.imbalance_5lvl = 0.4;
        let mut tracked = TrackedMarket::placeholder(market_id);
        tracked.state = market.clone();
        let mut markets = HashMap::new();
        markets.insert(market_id, tracked);
        let portfolio = Portfolio::with_starting_cash(20.0);
        let ctx = StrategyContext {
            market_id,
            markets: &markets,
            portfolio: &portfolio,
            now: Instant::now(),
        };
        let (surface, note) = strategy.plan(ctx, &market);
        assert_eq!(surface.intents.len(), 1);
        assert!(surface.intents[0].aggressive);
        assert!(note.unwrap().contains("buy qty"));
    }

    #[test]
    fn borderline_setup_is_blocked_by_opportunity_score() {
        let cfg = SweepProfile {
            min_opportunity_score: 0.9,
            aggressive_entry_score: 0.95,
            ..SweepProfile::default()
        };
        let strategy = SweepStrategy::new(cfg);
        let market_id = MarketId(15);
        let mut market = MarketState::new(market_id, Side::Up, Duration::from_secs(600));
        market.book.bids.push(L2Level {
            price: dec!(0.50),
            size: dec!(60),
        });
        market.book.asks.push(L2Level {
            price: dec!(0.51),
            size: dec!(60),
        });
        market.model_score = 0.6;
        market.fair_value = 0.56;
        market.edge_buy = 0.03;
        market.microprice = 0.509;
        market.trade_intensity = 2.0;
        market.wall_persistence_score = 0.6;
        market.liquidity_void_score = 0.05;
        market.cancel_skew = 0.0;
        market.imbalance_5lvl = 0.2;
        let mut tracked = TrackedMarket::placeholder(market_id);
        tracked.state = market.clone();
        let mut markets = HashMap::new();
        markets.insert(market_id, tracked);
        let portfolio = Portfolio::with_starting_cash(20.0);
        let ctx = StrategyContext {
            market_id,
            markets: &markets,
            portfolio: &portfolio,
            now: Instant::now(),
        };
        let (surface, note) = strategy.plan(ctx, &market);
        assert!(surface.intents.is_empty());
        assert!(note.unwrap().contains("opportunity score weak"));
    }

    #[test]
    fn stale_position_uses_aggressive_exit() {
        let strategy = SweepStrategy::new(SweepProfile::default());
        let market_id = MarketId(8);
        let now = Instant::now();
        let mut market = MarketState::new(market_id, Side::Up, Duration::from_secs(300));
        market.book.last_update = now;
        market.book.bids.push(L2Level {
            price: dec!(0.53),
            size: dec!(10),
        });
        market.book.asks.push(L2Level {
            price: dec!(0.54),
            size: dec!(10),
        });
        market.fair_value = 0.52;
        market.edge_sell = -0.02;
        market.wall_persistence_score = 0.1;
        let mut tracked = TrackedMarket::placeholder(market_id);
        tracked.state = market.clone();
        let mut markets = HashMap::new();
        markets.insert(market_id, tracked);
        let mut portfolio = Portfolio::with_starting_cash(10.0);
        let ts = now - Duration::from_secs(40);
        portfolio.apply_fill(market_id, OrderAction::Buy, 0.50, 2.0, 0.0, ts);
        portfolio.observe_fair(market_id, 0.58, ts);
        let ctx = StrategyContext {
            market_id,
            markets: &markets,
            portfolio: &portfolio,
            now: now + Duration::from_secs(60),
        };
        let (surface, _) = strategy.plan(ctx, &market);
        assert!(surface.intents[0].aggressive);
        assert_eq!(surface.intents[0].price, 0.53);
    }

    #[test]
    fn reversal_does_not_block_normal_entry() {
        let mut cfg = SweepProfile::default();
        cfg.reversal.enabled = true;
        let strategy = SweepStrategy::new(cfg);
        let market_id = MarketId(21);
        let now = Instant::now();
        let market = reversal_market(market_id, Side::Up, 240);
        let tracked = reversal_tracked(market.clone(), now, 30, 0.20, 0.0);
        let mut markets = HashMap::new();
        markets.insert(market_id, tracked);
        let portfolio = Portfolio::with_starting_cash(20.0);
        let ctx = StrategyContext {
            market_id,
            markets: &markets,
            portfolio: &portfolio,
            now,
        };
        let (surface, note) = strategy.plan(ctx, &market);
        assert_eq!(surface.intents.len(), 1);
        assert!(!note.unwrap().contains("opening scan"));
    }

    #[test]
    fn reversal_entry_crosses_after_bounce() {
        let mut cfg = SweepProfile::default();
        cfg.reversal.enabled = true;
        let strategy = SweepStrategy::new(cfg);
        let market_id = MarketId(22);
        let now = Instant::now();
        let market = reversal_market(market_id, Side::Up, 180);
        let tracked = reversal_tracked(market.clone(), now, 90, 0.22, 0.65);
        let mut markets = HashMap::new();
        markets.insert(market_id, tracked);
        let portfolio = Portfolio::with_starting_cash(20.0);
        let ctx = StrategyContext {
            market_id,
            markets: &markets,
            portfolio: &portfolio,
            now,
        };
        let (surface, note) = strategy.plan(ctx, &market);
        assert_eq!(surface.intents.len(), 1);
        assert!(surface.intents[0].aggressive);
        assert_eq!(surface.intents[0].ttl_ms, 800);
        assert!(note.unwrap().contains("rev"));
    }

    #[test]
    fn impulse_entry_crosses_on_shock() {
        let mut cfg = SweepProfile::default();
        cfg.min_model_score = 5.0;
        cfg.min_opportunity_score = 5.0;
        cfg.max_pair_ask_sum = 1.10;
        cfg.impulse.enabled = true;
        let strategy = SweepStrategy::new(cfg);
        let market_id = MarketId(30);
        let now = Instant::now();
        let mut market = reversal_market(market_id, Side::Up, 180);
        market.underlying.ret_250ms = 0.0025;
        market.underlying.ret_1s = 0.0035;
        market.underlying.accel = 0.0012;
        let tracked = priced_tracked(market.clone(), now, &[0.50, 0.52, 0.54]);
        let mut markets = HashMap::new();
        markets.insert(market_id, tracked);
        let portfolio = Portfolio::with_starting_cash(20.0);
        let ctx = StrategyContext { market_id, markets: &markets, portfolio: &portfolio, now };
        let (surface, note) = strategy.plan(ctx, &market);
        assert_eq!(surface.intents.len(), 1);
        assert_eq!(surface.intents[0].price, 0.50);
        assert!(surface.intents[0].aggressive);
        assert!(note.unwrap().contains("impulse buy"));
    }

    #[test]
    fn impulse_fade_exit_hits_bid() {
        let mut cfg = SweepProfile::default();
        cfg.impulse.enabled = true;
        let strategy = SweepStrategy::new(cfg);
        let market_id = MarketId(31);
        let now = Instant::now();
        let mut market = reversal_market(market_id, Side::Up, 180);
        market.book.bids[0].price = dec!(0.62);
        market.book.asks[0].price = dec!(0.63);
        market.fair_value = 0.60;
        market.imbalance_5lvl = 0.45;
        market.wall_persistence_score = 0.5;
        market.underlying.ret_250ms = 0.0025;
        market.underlying.ret_1s = 0.0035;
        market.underlying.accel = 0.0010;
        let tracked = priced_tracked(market.clone(), now, &[0.54, 0.58, 0.62]);
        let mut markets = HashMap::new();
        markets.insert(market_id, tracked);
        let mut portfolio = Portfolio::with_starting_cash(20.0);
        portfolio.apply_fill(
            market_id,
            OrderAction::Buy,
            0.50,
            2.0,
            0.0,
            now - Duration::from_secs(1),
        );
        let ctx = StrategyContext { market_id, markets: &markets, portfolio: &portfolio, now };
        let (surface, note) = strategy.plan(ctx, &market);
        assert_eq!(surface.intents.len(), 1);
        assert_eq!(surface.intents[0].action, OrderAction::Sell);
        assert_eq!(surface.intents[0].price, 0.62);
        assert!(note.unwrap().contains("fade exit"));
    }

    #[test]
    fn impulse_fade_entry_buys_pair() {
        let mut cfg = SweepProfile::default();
        cfg.min_model_score = 5.0;
        cfg.min_opportunity_score = 5.0;
        cfg.impulse.enabled = true;
        let strategy = SweepStrategy::new(cfg);
        let now = Instant::now();
        let up_id = MarketId(32);
        let dn_id = MarketId(33);
        let mut up = reversal_market(up_id, Side::Up, 180);
        up.book.bids[0].price = dec!(0.62);
        up.book.asks[0].price = dec!(0.63);
        up.fair_value = 0.60;
        up.imbalance_5lvl = 0.45;
        up.wall_persistence_score = 0.5;
        up.underlying.ret_250ms = 0.0025;
        up.underlying.ret_1s = 0.0035;
        up.underlying.accel = 0.0010;
        let mut dn = reversal_market(dn_id, Side::Down, 180);
        dn.book.bids[0].price = dec!(0.39);
        dn.book.asks[0].price = dec!(0.40);
        dn.fair_value = 0.46;
        dn.edge_buy = 0.07;
        let up_tracked = paired_tracked(up.clone(), now, &[0.54, 0.58, 0.62], "BTC");
        let dn_tracked = paired_tracked(dn.clone(), now, &[0.44, 0.42, 0.40], "BTC");
        let mut markets = HashMap::new();
        markets.insert(up_id, up_tracked);
        markets.insert(dn_id, dn_tracked);
        let portfolio = Portfolio::with_starting_cash(20.0);
        let ctx = StrategyContext { market_id: dn_id, markets: &markets, portfolio: &portfolio, now };
        let (surface, note) = strategy.plan(ctx, &dn);
        assert_eq!(surface.intents.len(), 1);
        assert_eq!(surface.intents[0].action, OrderAction::Buy);
        assert_eq!(surface.intents[0].price, 0.40);
        assert!(note.unwrap().contains("fade buy"));
    }

    fn reversal_market(market_id: MarketId, side: Side, expiry: u64) -> MarketState {
        let mut market = MarketState::new(market_id, side, Duration::from_secs(expiry));
        market.book.bids.push(L2Level {
            price: dec!(0.49),
            size: dec!(120),
        });
        market.book.asks.push(L2Level {
            price: dec!(0.50),
            size: dec!(120),
        });
        market.model_score = 1.0;
        market.fair_value = 0.56;
        market.edge_buy = 0.06;
        market.microprice = 0.499;
        market.trade_intensity = 5.0;
        market.wall_persistence_score = 0.8;
        market.liquidity_void_score = 0.05;
        market.cancel_skew = 0.0;
        market.imbalance_5lvl = 0.4;
        market
    }

    fn reversal_tracked(
        market: MarketState,
        now: Instant,
        age_secs: u64,
        low: f64,
        wick_bias: f64,
    ) -> TrackedMarket {
        let mut tracked = TrackedMarket::placeholder(market.market_id);
        tracked.state = market;
        tracked.runtime.symbol = "BTC".to_string();
        tracked.runtime.window_label = "12:00-12:05".to_string();
        tracked.runtime.window_secs = 300;
        tracked.runtime.open_at = Some(now - Duration::from_secs(age_secs));
        tracked.runtime.prev_wick_bias = wick_bias;
        tracked.runtime.opening = OpeningRange {
            open_px: Some(0.25),
            high_px: 0.27,
            low_px: low,
            last_px: 0.245,
            done: age_secs >= 75,
        };
        tracked
    }

    fn priced_tracked(market: MarketState, now: Instant, pxs: &[f64]) -> TrackedMarket {
        paired_tracked(market, now, pxs, "BTC")
    }

    fn paired_tracked(
        market: MarketState,
        now: Instant,
        pxs: &[f64],
        symbol: &str,
    ) -> TrackedMarket {
        let mut tracked = TrackedMarket::placeholder(market.market_id);
        tracked.state = market;
        tracked.runtime.symbol = symbol.to_string();
        tracked.runtime.window_label = "12:00-12:05".to_string();
        tracked.runtime.title = "BTC window".to_string();
        for (idx, px) in pxs.iter().enumerate() {
            let age = (pxs.len() - idx - 1) as u64 * 500;
            tracked
                .runtime
                .price_history
                .push_back((now - Duration::from_millis(age), *px));
        }
        tracked
    }
}
