use crate::engine::TradingEngine;
use crate::market::types::{MarketId, MarketState, OrderSurface, PositionState};
use crate::portfolio::Portfolio;
use crate::snapshot::{MarketSnapshot, WorldSnapshot};
use crate::state::TrackedMarket;
use std::collections::HashMap;

pub struct StrategyContext<'a> {
    pub market_id: MarketId,
    pub markets: &'a HashMap<MarketId, TrackedMarket>,
    pub portfolio: &'a Portfolio,
}

pub trait Strategy: Send + Sync {
    fn plan(&self, ctx: StrategyContext<'_>, market: &MarketState) -> (OrderSurface, Option<String>);
    fn sort_key(&self, market: &MarketSnapshot) -> f64;
    fn ticket_dollars(&self) -> f64;
    fn entry_threshold(&self) -> f64;
    fn exit_threshold(&self) -> f64;
    fn max_spread(&self) -> f64;
    fn paper_real_mode(&self) -> bool;
}

pub trait RiskPolicy: Send + Sync {
    fn apply(
        &self,
        market: &MarketState,
        position: Option<&PositionState>,
        surface: OrderSurface,
        portfolio: &Portfolio,
    ) -> OrderSurface;
}

pub trait SnapshotProjector: Send + Sync {
    fn project(&self, engine: &TradingEngine, strategy: &dyn Strategy) -> WorldSnapshot;
}
