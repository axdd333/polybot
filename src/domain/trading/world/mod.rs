mod pipeline;
mod projection;
#[cfg(test)]
mod tests;

use crate::domain::analyzer::{MarketRuntime, UnderlyingPoint};
use crate::domain::config::{ModelWeights, SweepStrategyConfig};
use crate::domain::market::features::FeatureVector;
use crate::domain::market::types::{MarketId, MarketState, OrderSurface, Side};
use crate::domain::trading::executor::{ExecutionBackend, LiveExecutor, SimulatedExecutor};
use crate::domain::trading::portfolio::Portfolio;
use crate::domain::trading::risk::RiskLimits;
use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::time::{Duration, Instant};

const PNL_TARGET_5M: f64 = 1.50;
const PNL_WINDOW_SECS: u64 = 300;
const PLAN_NOTE_COOLDOWN_SECS: u64 = 5;

#[derive(Debug, Clone)]
struct TrackedMarket {
    state: MarketState,
    runtime: MarketRuntime,
    features: Option<FeatureVector>,
    planned_surface: OrderSurface,
}

impl TrackedMarket {
    fn new(market_id: MarketId, side: Side, time_to_expiry: Duration) -> Self {
        Self {
            state: MarketState::new(market_id, side, time_to_expiry),
            runtime: MarketRuntime::default(),
            features: None,
            planned_surface: OrderSurface::default(),
        }
    }
}

pub struct World {
    markets: HashMap<MarketId, TrackedMarket>,
    underlying_tapes: HashMap<String, VecDeque<UnderlyingPoint>>,
    dirty: HashSet<MarketId>,
    pub portfolio: Portfolio,
    pub risk_limits: RiskLimits,
    pub strategy: SweepStrategyConfig,
    model_weights: ModelWeights,
    executor: Box<dyn ExecutionBackend>,
    pub journal: Vec<String>,
}

impl std::fmt::Debug for World {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("World")
            .field("markets", &self.markets.len())
            .field("cash", &self.portfolio.cash)
            .field("journal", &self.journal.len())
            .finish()
    }
}

impl World {
    pub fn new() -> Self {
        let strategy = SweepStrategyConfig::from_env();
        let risk_limits = RiskLimits::from_env();
        let mut journal = Vec::new();
        let executor = select_executor(&strategy, &mut journal);

        if env::var("SWEEP_MAX_SPREAD").is_err() && strategy.max_spread <= 0.03 {
            journal.push(format!(
                "diag: sweep max spread is {:.0}c; 1c x 99c books will stay blocked",
                strategy.max_spread * 100.0
            ));
        }

        Self {
            markets: HashMap::new(),
            underlying_tapes: HashMap::new(),
            dirty: HashSet::new(),
            portfolio: Portfolio::with_starting_cash(strategy.starting_cash),
            risk_limits,
            strategy,
            model_weights: ModelWeights::default(),
            executor,
            journal,
        }
    }

    pub fn with_executor(executor: Box<dyn ExecutionBackend>) -> Self {
        let mut world = Self::new();
        world.executor = executor;
        world
    }

    pub fn seed_market(
        &mut self,
        market_id: MarketId,
        title: impl Into<String>,
        symbol: impl Into<String>,
        window_label: impl Into<String>,
        end_label: impl Into<String>,
        side: Side,
        time_to_expiry: Duration,
    ) {
        let tracked = self
            .markets
            .entry(market_id)
            .or_insert_with(|| TrackedMarket::new(market_id, side, time_to_expiry));
        tracked.state.side = side;
        tracked.state.time_to_expiry = time_to_expiry;
        tracked.state.expiry_anchored_at = Instant::now();
        tracked.runtime.title = title.into();
        tracked.runtime.symbol = symbol.into();
        tracked.runtime.window_label = window_label.into();
        tracked.runtime.end_label = end_label.into();
        self.dirty.insert(market_id);
    }

    pub fn prune_markets(&mut self, active_market_ids: &HashSet<MarketId>) {
        let stale: Vec<MarketId> = self
            .markets
            .keys()
            .copied()
            .filter(|id| {
                !active_market_ids.contains(id) && self.portfolio.inventory_for(*id).abs() <= 0.0
            })
            .collect();

        for id in stale {
            self.markets.remove(&id);
            self.dirty.remove(&id);
            self.portfolio.drop_market(id);
        }
    }
}

fn select_executor(
    strategy: &SweepStrategyConfig,
    journal: &mut Vec<String>,
) -> Box<dyn ExecutionBackend> {
    let live = LiveExecutor::from_env();
    if !strategy.paper_real_mode && live.enabled {
        journal.push("mode: live execution enabled".to_string());
        Box::new(live)
    } else {
        journal.push("mode: simulated fills only; no live orders are submitted".to_string());
        Box::new(SimulatedExecutor)
    }
}

#[cfg(test)]
fn scaled_target_qty(base_qty: f64, inv_limit: f64, stream_splits: u32) -> f64 {
    if base_qty <= 0.0 || inv_limit <= 0.0 {
        return 0.0;
    }
    let stream_multiplier = 1.0 + (stream_splits as f64 / 100.0);
    (base_qty * stream_multiplier).min(inv_limit)
}
