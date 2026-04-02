use trading_core::config::RiskProfile;
use trading_core::market::book::best_bid;
use trading_core::market::types::{MarketState, OrderAction, OrderIntent, OrderSurface, PositionState};
use trading_core::portfolio::Portfolio;
use trading_core::traits::RiskPolicy;

#[derive(Debug, Clone)]
pub struct SweepRiskPolicy {
    limits: RiskProfile,
}

impl SweepRiskPolicy {
    pub fn new(limits: RiskProfile) -> Self {
        Self { limits }
    }
}

impl RiskPolicy for SweepRiskPolicy {
    fn apply(
        &self,
        market: &MarketState,
        position: Option<&PositionState>,
        surface: OrderSurface,
        portfolio: &Portfolio,
    ) -> OrderSurface {
        apply_risk_controls(market, position, surface, &self.limits, portfolio.kill_switch)
    }
}

pub fn apply_risk_controls(
    market: &MarketState,
    position: Option<&PositionState>,
    mut surface: OrderSurface,
    limits: &RiskProfile,
    kill_switch: bool,
) -> OrderSurface {
    if kill_switch {
        return exit_surface(market, position);
    }

    let remaining = limits.inv_limit - position.map(|p| p.qty.abs()).unwrap_or(0.0);
    if remaining <= 0.0 {
        surface.intents.clear();
        return surface;
    }

    for intent in &mut surface.intents {
        intent.qty = intent.qty.min(remaining);
    }
    surface.intents.retain(|intent| intent.qty > 0.0);
    surface
}

fn exit_surface(market: &MarketState, position: Option<&PositionState>) -> OrderSurface {
    let qty = position.map(|p| p.qty.abs()).unwrap_or(0.0);
    if qty <= 0.0 {
        return OrderSurface::default();
    }

    let price = best_bid(&market.book).max(0.01);
    OrderSurface {
        intents: vec![OrderIntent {
            market_id: market.market_id,
            side: market.side,
            action: OrderAction::Sell,
            price,
            qty,
            ttl_ms: 100,
            aggressive: true,
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trading_core::market::types::{L2Level, MarketId, Side};
    use rust_decimal_macros::dec;
    use std::time::Duration;

    #[test]
    fn exit_surface_uses_bid_for_sell_price() {
        let mut market = MarketState::new(MarketId(7), Side::Up, Duration::from_secs(30));
        market.book.bids.push(L2Level {
            price: dec!(0.41),
            size: dec!(15),
        });
        market.book.asks.push(L2Level {
            price: dec!(0.47),
            size: dec!(8),
        });
        let position = PositionState {
            qty: 5.0,
            ..PositionState::default()
        };

        let surface = apply_risk_controls(
            &market,
            Some(&position),
            OrderSurface::default(),
            &RiskProfile::default(),
            true,
        );

        assert_eq!(surface.intents.len(), 1);
        assert_eq!(surface.intents[0].price, 0.41);
    }
}
