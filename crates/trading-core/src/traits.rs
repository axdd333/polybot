use crate::market::types::{MarketId, MarketState, OrderSurface, PositionState};
use crate::portfolio::Portfolio;
use crate::snapshot::MarketSnapshot;
use crate::state::TrackedMarket;
use std::collections::HashMap;
use std::time::Instant;

pub struct StrategyContext<'a> {
    pub market_id: MarketId,
    pub markets: &'a HashMap<MarketId, TrackedMarket>,
    pub portfolio: &'a Portfolio,
    pub now: Instant,
}

pub trait Strategy: Send + Sync {
    fn plan(
        &self,
        ctx: StrategyContext<'_>,
        market: &MarketState,
    ) -> (OrderSurface, Option<String>);
    fn sort_key(&self, market: &MarketSnapshot) -> f64;
}

pub trait RiskPolicy: Send + Sync {
    fn apply(
        &self,
        ctx: StrategyContext<'_>,
        market: &MarketState,
        position: Option<&PositionState>,
        surface: OrderSurface,
    ) -> OrderSurface;
}
