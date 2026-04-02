mod planner;
mod projector;
mod risk;

use trading_core::config::{AppProfile, RunMode, SweepProfile};
use trading_core::engine::{EngineParts, TradingEngine};
use trading_core::executor::Executor;
use trading_core::market::book;
use trading_core::market::quote;
use trading_core::market::types::{MarketId, MarketState, OrderAction, OrderIntent, OrderSurface};
use trading_core::traits::{Strategy, StrategyContext};

pub use planner::{entry_signal_ok, maker_exit_target, market_sort_key};
pub use projector::SweepProjector;
pub use risk::SweepRiskPolicy;

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
        projector: Box::new(SweepProjector),
    })
}

#[derive(Debug, Clone)]
pub struct SweepStrategy {
    config: SweepProfile,
}

impl SweepStrategy {
    pub fn new(config: SweepProfile) -> Self {
        Self { config }
    }

    fn evaluate_entry_gate(
        &self,
        ctx: &StrategyContext<'_>,
        best_bid: f64,
        best_ask: f64,
    ) -> Option<String> {
        if !quote::has_tight_spread(best_bid, best_ask, self.config.max_spread) {
            return Some(format!(
                "blocked: spread {} > {}",
                quote::spread_cents_label(best_bid, best_ask),
                quote::cents_or_dash(self.config.max_spread)
            ));
        }

        let pair = self.pair_market_ids(ctx);
        if pair.len() <= 1 {
            return None;
        }

        if pair.iter().copied().any(|other_id| {
            other_id != ctx.market_id && ctx.portfolio.inventory_for(other_id).abs() > 0.0
        }) {
            return Some("blocked: paired side already open".to_string());
        }

        let pair_quotes: Vec<_> = pair
            .iter()
            .filter_map(|id| {
                ctx.markets.get(id).map(|tracked| {
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
            if pair_ask_sum > self.config.max_pair_ask_sum {
                return Some(format!(
                    "blocked: pair asks {:.0}c > {:.0}c",
                    pair_ask_sum * 100.0,
                    self.config.max_pair_ask_sum * 100.0
                ));
            }
        }

        let preferred = pair_quotes
            .iter()
            .filter(|(_, bid, ask, _)| quote::has_tight_spread(*bid, *ask, self.config.max_spread))
            .max_by(|a, b| {
                a.3.partial_cmp(&b.3)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
            })
            .map(|(id, _, _, edge)| (*id, *edge));

        match preferred {
            Some((preferred_id, _)) if preferred_id != ctx.market_id => {
                Some("blocked: weaker side in pair".to_string())
            }
            Some((_, edge)) if edge <= 0.0 => Some("blocked: no positive pair edge".to_string()),
            Some(_) => None,
            None => Some("blocked: no tradable side in pair".to_string()),
        }
    }

    fn pair_market_ids(&self, ctx: &StrategyContext<'_>) -> Vec<MarketId> {
        let Some(tracked) = ctx.markets.get(&ctx.market_id) else {
            return vec![ctx.market_id];
        };

        let mut pair: Vec<MarketId> = ctx
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
}

impl Strategy for SweepStrategy {
    fn plan(&self, ctx: StrategyContext<'_>, market: &MarketState) -> (OrderSurface, Option<String>) {
        let position = ctx.portfolio.position(ctx.market_id);
        let current_qty = position.map(|p| p.qty.abs()).unwrap_or(0.0);
        let best_ask = book::best_ask(&market.book);
        let best_bid = book::best_bid(&market.book);
        let entry_gate = self.evaluate_entry_gate(&ctx, best_bid, best_ask);
        let ticket_qty = if best_ask > 0.0 {
            (self.config.ticket_dollars / best_ask)
                .min((ctx.portfolio.cash / best_ask).max(0.0))
        } else {
            0.0
        };

        if current_qty > 0.0 {
            let exit_target = maker_exit_target(
                position
                    .map(|p| p.avg_px)
                    .unwrap_or(self.config.take_profit_price),
                self.config.take_profit_price,
            );
            let surface = OrderSurface {
                intents: vec![OrderIntent {
                    market_id: ctx.market_id,
                    side: market.side,
                    action: OrderAction::Sell,
                    price: exit_target,
                    qty: current_qty,
                    ttl_ms: 5_000,
                    aggressive: false,
                }],
            };
            let note = Some(if best_bid >= exit_target {
                format!(
                    "sell armed qty {:.1} target {}",
                    current_qty,
                    quote::cents_or_dash(exit_target)
                )
            } else {
                format!("waiting for {} exit", quote::cents_or_dash(exit_target))
            });
            return (surface, note);
        }

        if !quote::valid_live_quote(best_bid, best_ask) {
            return (OrderSurface::default(), Some("blocked: invalid quote".to_string()));
        }
        if let Some(reason) = entry_gate {
            return (OrderSurface::default(), Some(reason));
        }
        if best_ask > self.config.max_entry_price {
            return (
                OrderSurface::default(),
                Some(format!(
                    "blocked: ask {} > {}",
                    quote::cents_or_dash(best_ask),
                    quote::cents_or_dash(self.config.max_entry_price)
                )),
            );
        }
        if market.edge_buy < self.config.min_edge_to_buy {
            return (
                OrderSurface::default(),
                Some(format!(
                    "blocked: edge {:+.1}c < {:+.1}c",
                    market.edge_buy * 100.0,
                    self.config.min_edge_to_buy * 100.0
                )),
            );
        }
        if !entry_signal_ok(best_bid, best_ask, market.microprice, market.edge_buy) {
            return (
                OrderSurface::default(),
                Some("blocked: weak micro/edge signal".to_string()),
            );
        }
        if ticket_qty > 0.0 {
            return (
                OrderSurface {
                    intents: vec![OrderIntent {
                        market_id: ctx.market_id,
                        side: market.side,
                        action: OrderAction::Buy,
                        price: best_ask,
                        qty: ticket_qty,
                        ttl_ms: 150,
                        aggressive: true,
                    }],
                },
                Some(format!(
                    "buy armed qty {:.1} ask {} ticket ${:.2}",
                    ticket_qty,
                    quote::cents_or_dash(best_ask),
                    self.config.ticket_dollars
                )),
            );
        }
        if best_ask <= 0.0 {
            return (OrderSurface::default(), Some("blocked: no live ask".to_string()));
        }
        (
            OrderSurface::default(),
            Some("blocked: insufficient cash".to_string()),
        )
    }

    fn sort_key(&self, market: &trading_core::snapshot::MarketSnapshot) -> f64 {
        market_sort_key(market, self.config.max_entry_price)
    }

    fn ticket_dollars(&self) -> f64 {
        self.config.ticket_dollars
    }

    fn entry_threshold(&self) -> f64 {
        self.config.max_entry_price
    }

    fn exit_threshold(&self) -> f64 {
        self.config.take_profit_price
    }

    fn max_spread(&self) -> f64 {
        self.config.max_spread
    }

    fn paper_real_mode(&self) -> bool {
        self.config.paper_real_mode
    }
}
