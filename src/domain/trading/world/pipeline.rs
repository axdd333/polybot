use super::{TrackedMarket, World, PLAN_NOTE_COOLDOWN_SECS};
use crate::app::events::Event;
use crate::domain::analyzer::{self, TradeObservation};
use crate::domain::market::types::{MarketId, OrderAction, OrderIntent, OrderSurface, Side};
use crate::domain::market::{book, features, model, quote};
use crate::domain::trading::executor::{FillContext, RecentTrade};
use crate::domain::trading::{planner, risk};
use std::time::{Duration, Instant};

impl World {
    pub fn apply_event(&mut self, event: Event) {
        match event {
            Event::UnderlyingTick { symbol, px, ts } => self.apply_underlying_tick(symbol, px, ts),
            Event::BookSnapshot {
                market_id,
                bids,
                asks,
                ts,
            } => {
                let tracked = self.ensure_market(market_id);
                book::apply_snapshot(&mut tracked.state.book, bids, asks, ts);
                analyzer::update_book_runtime(&mut tracked.state, &mut tracked.runtime, ts);
                self.dirty.insert(market_id);
            }
            Event::BookDelta {
                market_id,
                bids,
                asks,
                ts,
            } => {
                let tracked = self.ensure_market(market_id);
                book::apply_delta(&mut tracked.state.book, bids, asks, ts);
                analyzer::update_book_runtime(&mut tracked.state, &mut tracked.runtime, ts);
                self.dirty.insert(market_id);
            }
            Event::TradePrint {
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
                self.dirty.insert(market_id);
            }
            Event::ExpiryUpdate {
                market_id,
                time_to_expiry,
            } => {
                let tracked = self.ensure_market(market_id);
                tracked.state.time_to_expiry = time_to_expiry;
                tracked.state.expiry_anchored_at = Instant::now();
                self.dirty.insert(market_id);
            }
            Event::TimerFast | Event::TimerSlow => {
                self.dirty.extend(self.markets.keys().copied());
            }
        }
    }

    pub async fn refresh_dirty_markets(&mut self) {
        let dirty: Vec<MarketId> = self.dirty.drain().collect();
        for market_id in dirty {
            self.recompute_features(market_id);
            self.classify_regime(market_id);
            self.price_fair_value(market_id);
            self.plan_orders(market_id);
            self.apply_risk(market_id);
            self.execute(market_id).await;
        }
    }

    fn ensure_market(&mut self, market_id: MarketId) -> &mut TrackedMarket {
        self.markets
            .entry(market_id)
            .or_insert_with(|| TrackedMarket::new(market_id, Side::Up, Duration::from_secs(300)))
    }

    fn apply_underlying_tick(&mut self, symbol: String, px: f64, ts: Instant) {
        let state = analyzer::update_underlying_state(
            self.underlying_tapes.entry(symbol.clone()).or_default(),
            px,
            ts,
        );

        for (&market_id, tracked) in &mut self.markets {
            if tracked.runtime.symbol != symbol {
                continue;
            }

            tracked.runtime.cross_window.m5_score = state.ret_1s;
            tracked.runtime.cross_window.m15_score = state.ret_5s;
            tracked.runtime.cross_window.torsion = (state.ret_1s - state.ret_5s).clamp(-2.0, 2.0);
            tracked.state.underlying = state.clone();
            tracked.state.cross_window_torsion = tracked.runtime.cross_window.torsion;
            self.dirty.insert(market_id);
        }
    }

    pub fn recompute_features(&mut self, market_id: MarketId) {
        let Some(tracked) = self.markets.get_mut(&market_id) else {
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
        self.portfolio.mark_price(market_id, mark);
    }

    pub fn classify_regime(&mut self, market_id: MarketId) {
        let Some(tracked) = self.markets.get_mut(&market_id) else {
            return;
        };
        let Some(features) = tracked.features.as_ref() else {
            return;
        };
        tracked.state.regime = model::classify_regime(features);
    }

    pub fn price_fair_value(&mut self, market_id: MarketId) {
        let Some(tracked) = self.markets.get_mut(&market_id) else {
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
        analyzer::push_history(
            &mut tracked.runtime.edge_history,
            now,
            tracked.state.edge_buy,
        );
    }

    pub fn plan_orders(&mut self, market_id: MarketId) {
        let Some(tracked) = self.markets.get(&market_id) else {
            return;
        };

        let market = &tracked.state;
        let position = self.portfolio.position(market_id);
        let current_qty = position.map(|p| p.qty.abs()).unwrap_or(0.0);
        let best_ask = book::best_ask(&market.book);
        let best_bid = book::best_bid(&market.book);
        let now = market.book.last_update;
        let entry_gate = self.evaluate_entry_gate(market_id, best_bid, best_ask);
        let ticket_qty = if best_ask > 0.0 {
            (self.strategy.ticket_dollars / best_ask).min((self.portfolio.cash / best_ask).max(0.0))
        } else {
            0.0
        };

        let (surface, note) = if current_qty > 0.0 {
            let exit_target = planner::maker_exit_target(
                position
                    .map(|p| p.avg_px)
                    .unwrap_or(self.strategy.take_profit_price),
                self.strategy.take_profit_price,
            );
            (
                OrderSurface {
                    intents: vec![OrderIntent {
                        market_id,
                        side: market.side,
                        action: OrderAction::Sell,
                        price: exit_target,
                        qty: current_qty,
                        ttl_ms: 5_000,
                        aggressive: false,
                    }],
                },
                Some(if best_bid >= exit_target {
                    format!(
                        "sell armed qty {:.1} target {}",
                        current_qty,
                        quote::cents_or_dash(exit_target)
                    )
                } else {
                    format!("waiting for {} exit", quote::cents_or_dash(exit_target))
                }),
            )
        } else if !quote::valid_live_quote(best_bid, best_ask) {
            (
                OrderSurface::default(),
                Some("blocked: invalid quote".to_string()),
            )
        } else if let Some(reason) = entry_gate {
            (OrderSurface::default(), Some(reason))
        } else if best_ask > self.strategy.max_entry_price {
            (
                OrderSurface::default(),
                Some(format!(
                    "blocked: ask {} > {}",
                    quote::cents_or_dash(best_ask),
                    quote::cents_or_dash(self.strategy.max_entry_price)
                )),
            )
        } else if market.edge_buy < self.strategy.min_edge_to_buy {
            (
                OrderSurface::default(),
                Some(format!(
                    "blocked: edge {:+.1}c < {:+.1}c",
                    market.edge_buy * 100.0,
                    self.strategy.min_edge_to_buy * 100.0
                )),
            )
        } else if !planner::entry_signal_ok(best_bid, best_ask, market.microprice, market.edge_buy)
        {
            (
                OrderSurface::default(),
                Some("blocked: weak micro/edge signal".to_string()),
            )
        } else if ticket_qty > 0.0 {
            (
                OrderSurface {
                    intents: vec![OrderIntent {
                        market_id,
                        side: market.side,
                        action: OrderAction::Buy,
                        price: best_ask,
                        qty: ticket_qty.min(self.risk_limits.inv_limit),
                        ttl_ms: 150,
                        aggressive: true,
                    }],
                },
                Some(format!(
                    "buy armed qty {:.1} ask {} ticket ${:.2}",
                    ticket_qty,
                    quote::cents_or_dash(best_ask),
                    self.strategy.ticket_dollars
                )),
            )
        } else if best_ask <= 0.0 {
            (
                OrderSurface::default(),
                Some("blocked: no live ask".to_string()),
            )
        } else {
            (
                OrderSurface::default(),
                Some("blocked: insufficient cash".to_string()),
            )
        };

        if let Some(tracked) = self.markets.get_mut(&market_id) {
            tracked.planned_surface = surface;
        }
        if let Some(note) = note {
            self.maybe_log_plan_note(market_id, now, &note);
        }
    }

    pub fn apply_risk(&mut self, market_id: MarketId) {
        let Some(tracked) = self.markets.get(&market_id) else {
            return;
        };
        let position = self.portfolio.position(market_id);
        let filtered = risk::apply_risk_controls(
            &tracked.state,
            position,
            tracked.planned_surface.clone(),
            &self.risk_limits,
            self.portfolio.kill_switch,
        );

        if let Some(tracked) = self.markets.get_mut(&market_id) {
            tracked.planned_surface = filtered;
        }
    }

    pub async fn execute(&mut self, market_id: MarketId) {
        let Some(tracked) = self.markets.get(&market_id) else {
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

        self.portfolio.replace_surface(market_id, surface.clone());
        let ctx = FillContext {
            best_bid,
            best_ask,
            recent_trades,
        };
        let mut filled = Vec::new();

        for intent in &surface.intents {
            if let Some(fill_price) = self.executor.try_fill(intent, &ctx) {
                let filled_qty = self.portfolio.apply_fill(
                    market_id,
                    intent.action,
                    fill_price,
                    intent.qty,
                    now,
                );
                if filled_qty > 0.0 {
                    filled.push(format!(
                        "{} {:.1} @ {:.3}",
                        match intent.action {
                            OrderAction::Buy => "BUY",
                            OrderAction::Sell => "SELL",
                        },
                        filled_qty,
                        fill_price
                    ));
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
                    .unwrap_or(self.strategy.take_profit_price)
            ),
            self.strategy.ticket_dollars,
            surface.intents.len(),
            if filled.is_empty() {
                "none".to_string()
            } else {
                filled.join(", ")
            }
        );
        self.journal.push(summary);

        let splits_now = self.portfolio.stream_splits;
        if splits_now > 0
            && self.portfolio.realized_pnl >= splits_now as f64 - 0.001
            && !self
                .journal
                .iter()
                .rev()
                .take(5)
                .any(|line| line.starts_with("SPLIT"))
        {
            self.journal.push(format!(
                "SPLIT #{splits_now} - stream doubled -> now {} active streams  realized=${:.2}",
                splits_now + 100,
                self.portfolio.realized_pnl,
            ));
        }

        self.trim_journal();
    }

    pub(super) fn evaluate_entry_gate(
        &self,
        market_id: MarketId,
        best_bid: f64,
        best_ask: f64,
    ) -> Option<String> {
        if !quote::has_tight_spread(best_bid, best_ask, self.strategy.max_spread) {
            return Some(format!(
                "blocked: spread {} > {}",
                quote::spread_cents_label(best_bid, best_ask),
                quote::cents_or_dash(self.strategy.max_spread)
            ));
        }

        let pair = self.pair_market_ids(market_id);
        if pair.len() <= 1 {
            return None;
        }

        if pair.iter().copied().any(|other_id| {
            other_id != market_id && self.portfolio.inventory_for(other_id).abs() > 0.0
        }) {
            return Some("blocked: paired side already open".to_string());
        }

        let pair_quotes: Vec<_> = pair
            .iter()
            .filter_map(|id| {
                self.markets.get(id).map(|tracked| {
                    (
                        *id,
                        book::best_bid(&tracked.state.book),
                        book::best_ask(&tracked.state.book),
                        tracked.state.edge_buy,
                    )
                })
            })
            .collect();

        let valid_asks: Vec<f64> = pair_quotes
            .iter()
            .filter(|(_, bid, ask, _)| quote::valid_live_quote(*bid, *ask))
            .map(|(_, _, ask, _)| *ask)
            .collect();

        if valid_asks.len() == 2 {
            let pair_ask_sum: f64 = valid_asks.iter().sum();
            if pair_ask_sum > self.strategy.max_pair_ask_sum {
                return Some(format!(
                    "blocked: pair asks {:.0}c > {:.0}c",
                    pair_ask_sum * 100.0,
                    self.strategy.max_pair_ask_sum * 100.0
                ));
            }
        }

        let preferred = pair_quotes
            .iter()
            .filter(|(_, bid, ask, _)| {
                quote::has_tight_spread(*bid, *ask, self.strategy.max_spread)
            })
            .max_by(|a, b| {
                a.3.partial_cmp(&b.3)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
            })
            .map(|(id, _, _, edge)| (*id, *edge));

        match preferred {
            Some((preferred_id, _)) if preferred_id != market_id => {
                Some("blocked: weaker side in pair".to_string())
            }
            Some((_, edge)) if edge <= 0.0 => Some("blocked: no positive pair edge".to_string()),
            Some(_) => None,
            None => Some("blocked: no tradable side in pair".to_string()),
        }
    }

    fn pair_market_ids(&self, market_id: MarketId) -> Vec<MarketId> {
        let Some(tracked) = self.markets.get(&market_id) else {
            return vec![market_id];
        };

        let mut pair: Vec<MarketId> = self
            .markets
            .iter()
            .filter(|(_, other)| {
                other.runtime.symbol == tracked.runtime.symbol
                    && other.runtime.window_label == tracked.runtime.window_label
                    && other.runtime.title == tracked.runtime.title
            })
            .map(|(id, _)| *id)
            .collect();
        pair.sort_by_key(|id| id.0);
        pair
    }

    pub(crate) fn momentum_signal(&self, market_id: MarketId) -> Option<(bool, f64)> {
        let tracked = self.markets.get(&market_id)?;
        let ret1 = tracked.state.underlying.ret_1s;
        let ret5 = tracked.state.underlying.ret_5s;
        let vol = tracked.state.underlying.vol_5s.max(1e-6);
        let signal = (ret1 * 0.65 + ret5 * 0.35) / vol;
        if signal.abs() < 0.5 {
            return None;
        }
        Some((signal > 0.0, signal.abs()))
    }

    fn deployed_notional(&self) -> f64 {
        self.markets
            .iter()
            .map(|(id, tracked)| {
                let qty = self.portfolio.inventory_for(*id).abs();
                qty * analyzer::liquidation_mark(&tracked.state.book, tracked.state.fair_value)
                    .max(0.0)
            })
            .sum()
    }

    pub(crate) fn portfolio_equity(&self) -> f64 {
        self.portfolio.cash + self.deployed_notional()
    }

    fn maybe_log_plan_note(&mut self, market_id: MarketId, now: Instant, note: &str) {
        let Some(tracked) = self.markets.get_mut(&market_id) else {
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

        self.journal.push(format!(
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
        if self.journal.len() > 256 {
            let drain = self.journal.len() - 256;
            self.journal.drain(0..drain);
        }
    }
}
