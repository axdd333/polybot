use crate::analyzer::{self, TradeObservation};
use crate::config::{ModelWeights, RunMode};
use crate::events::{LiveOrderStatus, MarketDiscovered, NormalizedEvent};
use crate::executor::{ExecutionReport, ExecutionRequest, Executor, FillContext, RecentTrade};
use crate::market::types::{MarketId, OrderAction, Side};
use crate::market::{book, features, model, quote};
use crate::state::{EngineState, TrackedMarket};
use crate::traits::{RiskPolicy, SnapshotProjector, Strategy, StrategyContext};
use std::time::{Duration, Instant};

const PNL_TARGET_5M: f64 = 1.50;
const PNL_WINDOW_SECS: u64 = 300;
const PLAN_NOTE_COOLDOWN_SECS: u64 = 5;

pub struct EngineParts {
    pub mode: RunMode,
    pub starting_cash: f64,
    pub model_weights: ModelWeights,
    pub strategy: Box<dyn Strategy>,
    pub risk_policy: Box<dyn RiskPolicy>,
    pub executor: Box<dyn Executor>,
    pub projector: Box<dyn SnapshotProjector>,
}

pub struct TradingEngine {
    pub state: EngineState,
    pub model_weights: ModelWeights,
    strategy: Box<dyn Strategy>,
    risk_policy: Box<dyn RiskPolicy>,
    executor: Box<dyn Executor>,
    projector: Box<dyn SnapshotProjector>,
}

impl TradingEngine {
    pub fn new(parts: EngineParts) -> Self {
        Self {
            state: EngineState::new(parts.mode, parts.starting_cash),
            model_weights: parts.model_weights,
            strategy: parts.strategy,
            risk_policy: parts.risk_policy,
            executor: parts.executor,
            projector: parts.projector,
        }
    }

    pub fn apply_event(&mut self, event: NormalizedEvent) {
        match event {
            NormalizedEvent::UnderlyingTick { symbol, px, ts } => {
                self.apply_underlying_tick(symbol, px, ts)
            }
            NormalizedEvent::BookSnapshot {
                market_id,
                bids,
                asks,
                ts,
            } => {
                let tracked = self.ensure_market(market_id);
                book::apply_snapshot(&mut tracked.state.book, bids, asks, ts);
                analyzer::update_book_runtime(&mut tracked.state, &mut tracked.runtime, ts);
                self.state.markets.dirty.insert(market_id);
            }
            NormalizedEvent::BookDelta {
                market_id,
                bids,
                asks,
                ts,
            } => {
                let tracked = self.ensure_market(market_id);
                book::apply_delta(&mut tracked.state.book, bids, asks, ts);
                analyzer::update_book_runtime(&mut tracked.state, &mut tracked.runtime, ts);
                self.state.markets.dirty.insert(market_id);
            }
            NormalizedEvent::TradePrint {
                market_id,
                price,
                size,
                ts,
            } => {
                let tracked = self.ensure_market(market_id);
                tracked
                    .runtime
                    .trade_tape
                    .push_back(TradeObservation { ts, price, size });
                analyzer::trim_trade_tape(&mut tracked.runtime.trade_tape, ts);
                self.state.markets.dirty.insert(market_id);
            }
            NormalizedEvent::MarketDiscovered { market, .. } => self.register_market(market),
            NormalizedEvent::MarketExpired { market_id, .. } => self.expire_market(market_id),
            NormalizedEvent::LiveOrderUpdate {
                market_id,
                order_id,
                status,
                size_matched,
                ..
            } => self.apply_live_order_update(market_id, order_id, status, size_matched),
            NormalizedEvent::LiveTrade {
                market_id,
                order_id,
                action,
                price,
                qty,
                ..
            } => self.apply_live_trade(market_id, order_id, action, price, qty),
            NormalizedEvent::TimerTick { .. } => {
                self.state
                    .markets
                    .dirty
                    .extend(self.state.markets.markets.keys().copied());
            }
        }
    }

    pub async fn refresh_dirty_markets(&mut self) {
        let dirty: Vec<MarketId> = self.state.markets.dirty.drain().collect();
        for market_id in dirty {
            self.recompute_features(market_id);
            self.classify_regime(market_id);
            self.price_fair_value(market_id);
            self.plan_orders(market_id);
            self.apply_risk(market_id);
            self.execute(market_id).await;
        }
    }

    pub fn snapshot(&self) -> crate::snapshot::WorldSnapshot {
        self.projector.project(self, self.strategy.as_ref())
    }

    pub fn strategy(&self) -> &dyn Strategy {
        self.strategy.as_ref()
    }

    pub fn realized_pnl_5m(&self, now: Instant) -> f64 {
        self.state
            .portfolio
            .realized_over_window(now, Duration::from_secs(PNL_WINDOW_SECS))
    }

    pub fn pnl_shortfall_5m(&self, now: Instant) -> f64 {
        (PNL_TARGET_5M - self.realized_pnl_5m(now)).max(0.0)
    }

    pub fn momentum_signal(&self, market_id: MarketId) -> Option<(bool, f64)> {
        let tracked = self.state.markets.markets.get(&market_id)?;
        let ret1 = tracked.state.underlying.ret_1s;
        let ret5 = tracked.state.underlying.ret_5s;
        let vol = tracked.state.underlying.vol_5s.max(1e-6);
        let signal = (ret1 * 0.65 + ret5 * 0.35) / vol;
        if signal.abs() < 0.5 {
            return None;
        }
        Some((signal > 0.0, signal.abs()))
    }

    pub fn portfolio_equity(&self) -> f64 {
        self.state.portfolio.cash + self.deployed_notional()
    }

    fn ensure_market(&mut self, market_id: MarketId) -> &mut TrackedMarket {
        self.state
            .markets
            .markets
            .entry(market_id)
            .or_insert_with(|| TrackedMarket::placeholder(market_id))
    }

    fn register_market(&mut self, market: MarketDiscovered) {
        let market_id = market.instrument_id.market;
        let tracked = self
            .state
            .markets
            .markets
            .entry(market_id)
            .or_insert_with(|| {
                TrackedMarket::new(
                    market.instrument_id.clone(),
                    market.condition_id.clone(),
                    market.token_id.clone(),
                    market.side,
                    Duration::from_secs(market.time_to_expiry_secs),
                )
            });

        tracked.instrument_id = market.instrument_id;
        tracked.condition_id = market.condition_id;
        tracked.token_id = market.token_id;
        tracked.state.side = market.side;
        tracked.state.time_to_expiry = Duration::from_secs(market.time_to_expiry_secs);
        tracked.state.expiry_anchored_at = Instant::now();
        tracked.runtime.title = market.title;
        tracked.runtime.symbol = market.symbol;
        tracked.runtime.window_label = market.window_label;
        tracked.runtime.end_label = market.end_label;
        self.state.markets.dirty.insert(market_id);
    }

    fn expire_market(&mut self, market_id: MarketId) {
        if self.state.portfolio.inventory_for(market_id).abs() > 0.0 {
            if let Some(tracked) = self.state.markets.markets.get_mut(&market_id) {
                tracked.state.time_to_expiry = Duration::ZERO;
                tracked.state.expiry_anchored_at = Instant::now();
            }
            self.state.markets.dirty.insert(market_id);
            return;
        }

        self.state.markets.markets.remove(&market_id);
        self.state.markets.dirty.remove(&market_id);
        self.state.portfolio.drop_market(market_id);
    }

    fn apply_underlying_tick(&mut self, symbol: String, px: f64, ts: Instant) {
        let state = analyzer::update_underlying_state(
            self.state
                .underlyings
                .tapes
                .entry(symbol.clone())
                .or_default(),
            px,
            ts,
        );

        for (&market_id, tracked) in &mut self.state.markets.markets {
            if tracked.runtime.symbol != symbol {
                continue;
            }

            tracked.runtime.cross_window.m5_score = state.ret_1s;
            tracked.runtime.cross_window.m15_score = state.ret_5s;
            tracked.runtime.cross_window.torsion = (state.ret_1s - state.ret_5s).clamp(-2.0, 2.0);
            tracked.state.underlying = state.clone();
            tracked.state.cross_window_torsion = tracked.runtime.cross_window.torsion;
            self.state.markets.dirty.insert(market_id);
        }
    }

    fn recompute_features(&mut self, market_id: MarketId) {
        let Some(tracked) = self.state.markets.markets.get_mut(&market_id) else {
            return;
        };

        tracked.state.spread_ticks = book::spread_ticks(&tracked.state.book, 0.01);
        tracked.state.microprice = book::microprice(&tracked.state.book);
        tracked.state.imbalance_top = book::top_imbalance(&tracked.state.book);
        tracked.state.imbalance_5lvl = book::five_level_imbalance(&tracked.state.book);
        tracked.state.depth_slope_bid = book::depth_slope(&tracked.state.book.bids);
        tracked.state.depth_slope_ask = book::depth_slope(&tracked.state.book.asks);
        tracked.state.cross_window_torsion = tracked.runtime.cross_window.torsion;
        tracked.state.liquidity_void_score = book::liquidity_void_score(&tracked.state.book);
        tracked.state.wall_persistence_score = tracked.runtime.wall_persistence_score;

        let now = tracked.state.book.last_update;
        let (intensity, burstiness) = analyzer::trade_metrics(&tracked.runtime.trade_tape, now);
        tracked.state.trade_intensity = intensity;
        tracked.state.burstiness = burstiness;

        let mark = analyzer::liquidation_mark(&tracked.state.book, tracked.state.fair_value);
        let feature = features::compute(&tracked.state);
        analyzer::push_history(
            &mut tracked.runtime.flow_history,
            now,
            feature.trade_intensity,
        );
        analyzer::push_history(
            &mut tracked.runtime.micro_history,
            now,
            feature.microprice_gap,
        );
        tracked.features = Some(feature);
        self.state.portfolio.mark_price(market_id, mark);
    }

    fn classify_regime(&mut self, market_id: MarketId) {
        let Some(tracked) = self.state.markets.markets.get_mut(&market_id) else {
            return;
        };
        let Some(features) = tracked.features.as_ref() else {
            return;
        };
        tracked.state.regime = model::classify_regime(features);
    }

    fn price_fair_value(&mut self, market_id: MarketId) {
        let Some(tracked) = self.state.markets.markets.get_mut(&market_id) else {
            return;
        };
        let Some(features) = tracked.features.as_ref() else {
            return;
        };

        let score = model::score(features, &self.model_weights);
        let fair = model::fair_value_for_side(tracked.state.side, score);
        let bid = book::best_bid(&tracked.state.book);
        let ask = book::best_ask(&tracked.state.book);

        tracked.state.model_score = score;
        tracked.state.fair_value = fair;
        tracked.state.edge_buy = if quote::valid_live_quote(bid, ask) {
            model::edge_to_buy(fair, ask)
        } else {
            0.0
        };
        tracked.state.edge_sell = if bid > 0.0 {
            model::edge_to_sell(fair, bid)
        } else {
            0.0
        };

        let now = tracked.state.book.last_update;
        analyzer::push_history(&mut tracked.runtime.fair_history, now, fair);
        analyzer::push_history(&mut tracked.runtime.edge_history, now, tracked.state.edge_buy);
    }

    fn plan_orders(&mut self, market_id: MarketId) {
        let Some(tracked) = self.state.markets.markets.get(&market_id) else {
            return;
        };

        let ctx = StrategyContext {
            market_id,
            markets: &self.state.markets.markets,
            portfolio: &self.state.portfolio,
        };
        let (surface, note) = self.strategy.plan(ctx, &tracked.state);
        let now = tracked.state.book.last_update;

        if let Some(tracked) = self.state.markets.markets.get_mut(&market_id) {
            tracked.planned_surface = surface;
        }
        if let Some(note) = note {
            self.maybe_log_plan_note(market_id, now, &note);
        }
    }

    fn apply_risk(&mut self, market_id: MarketId) {
        let Some(tracked) = self.state.markets.markets.get(&market_id) else {
            return;
        };
        let position = self.state.portfolio.position(market_id);
        let filtered = self.risk_policy.apply(
            &tracked.state,
            position,
            tracked.planned_surface.clone(),
            &self.state.portfolio,
        );

        if let Some(tracked) = self.state.markets.markets.get_mut(&market_id) {
            tracked.planned_surface = filtered;
        }
    }

    async fn execute(&mut self, market_id: MarketId) {
        let Some(tracked) = self.state.markets.markets.get(&market_id) else {
            return;
        };

        let surface = tracked.planned_surface.clone();
        let best_bid = book::best_bid(&tracked.state.book);
        let best_ask = book::best_ask(&tracked.state.book);
        let now = tracked.state.book.last_update;
        let side = tracked.state.side;
        let symbol = tracked.runtime.symbol.clone();
        let window = tracked.runtime.window_label.clone();
        let recent_trades: Vec<RecentTrade> = tracked
            .runtime
            .trade_tape
            .iter()
            .filter(|t| now.duration_since(t.ts) <= Duration::from_secs(1))
            .map(|t| RecentTrade {
                price: t.price,
                size: t.size,
            })
            .collect();

        self.state
            .portfolio
            .replace_surface(market_id, surface.clone());
        let ctx = FillContext {
            best_bid,
            best_ask,
            recent_trades,
        };
        let pending = self.state.run.live_orders.get(&market_id);
        let reports = match self
            .executor
            .execute(ExecutionRequest {
                market_id,
                token_id: &tracked.token_id,
                condition_id: &tracked.condition_id,
                surface: &surface,
                pending,
                ctx: &ctx,
            })
            .await
        {
            Ok(reports) => reports,
            Err(err) => {
                self.state
                    .run
                    .journal
                    .push(format!("exec error {} {}: {}", symbol, window, err));
                return;
            }
        };

        let mut filled = Vec::new();
        for report in reports {
            match report {
                ExecutionReport::PaperFill { action, price, qty } => {
                    let filled_qty = self
                        .state
                        .portfolio
                        .apply_fill(market_id, action, price, qty, now);
                    if filled_qty > 0.0 {
                        filled.push(format!(
                            "{} {:.1} @ {:.3}",
                            match action {
                                OrderAction::Buy => "BUY",
                                OrderAction::Sell => "SELL",
                            },
                            filled_qty,
                            price
                        ));
                    }
                }
                ExecutionReport::LiveOrderAccepted {
                    order_id,
                    action,
                    price,
                    qty,
                } => {
                    self.state.run.live_orders.insert(
                        market_id,
                        crate::state::PendingLiveOrder {
                            order_id: order_id.clone(),
                            action,
                            price,
                            qty,
                            size_matched: 0.0,
                        },
                    );
                    self.state.run.journal.push(format!(
                        "live order accepted {} {} {} {} x {:.1} id={}",
                        symbol,
                        window,
                        match action {
                            OrderAction::Buy => "BUY",
                            OrderAction::Sell => "SELL",
                        },
                        quote::cents_or_dash(price),
                        qty,
                        order_id
                    ));
                }
                ExecutionReport::LiveOrderCancelled { order_id } => {
                    self.state.run.live_orders.remove(&market_id);
                    self.state
                        .run
                        .journal
                        .push(format!("live order cancelled {} {} id={}", symbol, window, order_id));
                }
                ExecutionReport::LiveOrderRejected { reason } => {
                    self.state
                        .run
                        .journal
                        .push(format!("live order rejected {} {}: {}", symbol, window, reason));
                }
            }
        }

        if surface.intents.is_empty() && filled.is_empty() {
            return;
        }

        let summary = format!(
            "{} {} {} | ask {} target {} ticket ${:.2} | queued {} | fills {}",
            symbol,
            window,
            match side {
                Side::Up => "U",
                Side::Down => "D",
            },
            quote::cents_or_dash(best_ask),
            quote::cents_or_dash(
                surface
                    .intents
                    .iter()
                    .find(|i| matches!(i.action, OrderAction::Sell))
                    .map(|i| i.price)
                    .unwrap_or(self.strategy.exit_threshold())
            ),
            self.strategy.ticket_dollars(),
            surface.intents.len(),
            if filled.is_empty() {
                "none".to_string()
            } else {
                filled.join(", ")
            }
        );
        self.state.run.journal.push(summary);
        self.trim_journal();
    }

    fn apply_live_order_update(
        &mut self,
        market_id: MarketId,
        order_id: String,
        status: LiveOrderStatus,
        size_matched: f64,
    ) {
        if let Some(pending) = self.state.run.live_orders.get_mut(&market_id) {
            if pending.order_id == order_id {
                pending.size_matched = size_matched.max(pending.size_matched);
                if matches!(
                    status,
                    LiveOrderStatus::Cancelled
                        | LiveOrderStatus::Filled
                        | LiveOrderStatus::Rejected
                ) {
                    self.state.run.live_orders.remove(&market_id);
                }
            }
        }
    }

    fn apply_live_trade(
        &mut self,
        market_id: MarketId,
        order_id: Option<String>,
        action: OrderAction,
        price: f64,
        qty: f64,
    ) {
        let filled_qty = self
            .state
            .portfolio
            .apply_fill(market_id, action, price, qty, Instant::now());
        if let Some(order_id) = order_id {
            if let Some(pending) = self.state.run.live_orders.get_mut(&market_id) {
                if pending.order_id == order_id {
                    pending.size_matched += filled_qty;
                    if pending.size_matched + 1e-9 >= pending.qty {
                        self.state.run.live_orders.remove(&market_id);
                    }
                }
            }
        }
    }

    fn deployed_notional(&self) -> f64 {
        self.state
            .markets
            .markets
            .iter()
            .map(|(id, tracked)| {
                let qty = self.state.portfolio.inventory_for(*id).abs();
                qty * analyzer::liquidation_mark(&tracked.state.book, tracked.state.fair_value).max(0.0)
            })
            .sum()
    }

    fn maybe_log_plan_note(&mut self, market_id: MarketId, now: Instant, note: &str) {
        let Some(tracked) = self.state.markets.markets.get_mut(&market_id) else {
            return;
        };

        let should_log = tracked
            .runtime
            .last_plan_note
            .as_deref()
            .map(|last| last != note)
            .unwrap_or(true)
            || tracked
                .runtime
                .last_plan_note_at
                .map(|ts| now.duration_since(ts) >= Duration::from_secs(PLAN_NOTE_COOLDOWN_SECS))
                .unwrap_or(true);
        if !should_log {
            return;
        }

        self.state.run.journal.push(format!(
            "{} {} {} | {}",
            tracked.runtime.symbol,
            tracked.runtime.window_label,
            match tracked.state.side {
                Side::Up => "U",
                Side::Down => "D",
            },
            note
        ));
        tracked.runtime.last_plan_note = Some(note.to_string());
        tracked.runtime.last_plan_note_at = Some(now);
        self.trim_journal();
    }

    fn trim_journal(&mut self) {
        if self.state.run.journal.len() > 256 {
            let drain = self.state.run.journal.len() - 256;
            self.state.run.journal.drain(0..drain);
        }
    }
}
