use crate::domain::market::book::best_bid;
use crate::domain::market::types::{
    MarketState, OrderAction, OrderIntent, OrderSurface, PositionState,
};
use std::env;

#[derive(Debug, Clone)]
pub struct RiskLimits {
    pub inv_limit: f64,
    pub base_qty: f64,
    pub max_loss: f64,
}

impl Default for RiskLimits {
    fn default() -> Self {
        Self {
            inv_limit: 100.0,
            base_qty: 10.0,
            max_loss: 0.03,
        }
    }
}

impl RiskLimits {
    pub fn from_env() -> Self {
        let default = Self::default();
        let limits = Self {
            inv_limit: env_f64("INV_LIMIT", default.inv_limit),
            base_qty: env_f64("BASE_QTY", default.base_qty),
            max_loss: env_f64("MAX_LOSS_PER_MARKET", default.max_loss),
        };
        limits.validate();
        limits
    }

    fn validate(&self) {
        assert!(self.inv_limit > 0.0, "inv_limit must be positive");
        assert!(self.base_qty > 0.0, "base_qty must be positive");
        assert!(self.max_loss > 0.0, "max_loss must be positive");
    }
}

pub fn apply_risk_controls(
    market: &MarketState,
    position: Option<&PositionState>,
    mut surface: OrderSurface,
    limits: &RiskLimits,
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

fn env_f64(key: &str, default: f64) -> f64 {
    env::var(key)
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::market::types::{L2Level, MarketId, Side};
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
            &RiskLimits::default(),
            true,
        );

        assert_eq!(surface.intents.len(), 1);
        assert_eq!(surface.intents[0].price, 0.41);
    }
}
