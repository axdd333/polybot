use std::time::Duration;
use trading_core::config::RiskProfile;
use trading_core::market::book::{best_ask, best_bid};
use trading_core::market::types::{
    MarketState, OrderAction, OrderIntent, OrderSurface, PositionState,
};
use trading_core::traits::{RiskPolicy, StrategyContext};

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
        ctx: StrategyContext<'_>,
        market: &MarketState,
        position: Option<&PositionState>,
        surface: OrderSurface,
    ) -> OrderSurface {
        apply_risk_controls(ctx, market, position, surface, &self.limits)
    }
}

pub fn apply_risk_controls(
    ctx: StrategyContext<'_>,
    market: &MarketState,
    position: Option<&PositionState>,
    mut surface: OrderSurface,
    limits: &RiskProfile,
) -> OrderSurface {
    if breached_loss_limit(&ctx, market, limits) {
        return exit_surface(market, position);
    }
    if ctx.portfolio.kill_switch {
        return exit_surface(market, position);
    }
    let rem = limits.inv_limit - position.map(|p| p.qty.abs()).unwrap_or(0.0);
    if rem <= 0.0 {
        surface.intents.clear();
        return surface;
    }
    let budget = budget_left(ctx, market, limits);
    for intent in &mut surface.intents {
        if intent.action == OrderAction::Buy {
            let max_qty = (budget / intent.price.max(0.01)).max(0.0);
            intent.qty = intent.qty.min(rem).min(max_qty);
        } else {
            intent.qty = intent.qty.min(rem.max(intent.qty));
        }
    }
    surface.intents.retain(|intent| intent.qty > 0.0);
    surface
}

fn breached_loss_limit(
    ctx: &StrategyContext<'_>,
    _market: &MarketState,
    limits: &RiskProfile,
) -> bool {
    let eq = portfolio_equity(ctx);
    let dd = ctx.portfolio.drawdown_frac(eq);
    if dd >= limits.max_loss {
        return true;
    }
    let pnl_5m = ctx
        .portfolio
        .realized_over_window(ctx.now, Duration::from_secs(300));
    let loss_5m = (-pnl_5m / ctx.portfolio.starting_cash.max(1.0)).max(0.0);
    loss_5m >= limits.max_loss_5m
}

fn portfolio_equity(ctx: &StrategyContext<'_>) -> f64 {
    ctx.portfolio.cash
        + ctx
            .markets
            .values()
            .map(|m| notional(ctx, &m.state))
            .sum::<f64>()
}

fn budget_left(ctx: StrategyContext<'_>, market: &MarketState, limits: &RiskProfile) -> f64 {
    let asset = asset_load(&ctx, market);
    let regime = regime_load(&ctx, market);
    let bucket = bucket_load(&ctx, market);
    let asset_left = (limits.asset_notional_limit - asset).max(0.0);
    let regime_left = (limits.regime_notional_limit - regime).max(0.0);
    let bucket_left = (limits.corr_bucket_notional_limit - bucket).max(0.0);
    asset_left.min(regime_left).min(bucket_left)
}

fn asset_load(ctx: &StrategyContext<'_>, market: &MarketState) -> f64 {
    ctx.markets
        .values()
        .filter(|m| {
            m.runtime.symbol == symbol(ctx, market)
                && ctx.portfolio.inventory_for(m.state.market_id) > 0.0
        })
        .map(|m| notional(ctx, &m.state))
        .sum()
}

fn regime_load(ctx: &StrategyContext<'_>, market: &MarketState) -> f64 {
    ctx.markets
        .values()
        .filter(|m| {
            m.state.regime == market.regime && ctx.portfolio.inventory_for(m.state.market_id) > 0.0
        })
        .map(|m| notional(ctx, &m.state))
        .sum()
}

fn bucket_load(ctx: &StrategyContext<'_>, market: &MarketState) -> f64 {
    let key = symbol(ctx, market);
    ctx.markets
        .values()
        .filter(|m| m.runtime.symbol == key && ctx.portfolio.inventory_for(m.state.market_id) > 0.0)
        .map(|m| notional(ctx, &m.state))
        .sum()
}

fn symbol(ctx: &StrategyContext<'_>, market: &MarketState) -> String {
    ctx.markets
        .get(&market.market_id)
        .map(|m| m.runtime.symbol.clone())
        .unwrap_or_else(|| "UNKNOWN".to_string())
}

fn notional(ctx: &StrategyContext<'_>, market: &MarketState) -> f64 {
    let qty = ctx.portfolio.inventory_for(market.market_id).abs();
    let mark = best_bid(&market.book).max(best_ask(&market.book)).max(0.01);
    qty * mark
}

fn exit_surface(market: &MarketState, position: Option<&PositionState>) -> OrderSurface {
    let qty = position.map(|p| p.qty.abs()).unwrap_or(0.0);
    if qty <= 0.0 {
        return OrderSurface::default();
    }
    let price = best_bid(&market.book).max(0.01);
    let intent = OrderIntent {
        market_id: market.market_id,
        side: market.side,
        action: OrderAction::Sell,
        price,
        qty,
        ttl_ms: 100,
        aggressive: true,
    };
    OrderSurface {
        intents: vec![intent],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use std::collections::HashMap;
    use std::time::Duration;
    use trading_core::market::types::{L2Level, MarketId, Side};
    use trading_core::portfolio::Portfolio;
    use trading_core::state::TrackedMarket;

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
        let portfolio = Portfolio::with_starting_cash(10.0);
        let tracked = TrackedMarket::placeholder(MarketId(7));
        let mut markets = HashMap::new();
        markets.insert(MarketId(7), tracked);
        let ctx = StrategyContext {
            market_id: MarketId(7),
            markets: &markets,
            portfolio: &portfolio,
            now: market.book.last_update,
        };
        let surface = apply_risk_controls(
            ctx,
            &market,
            Some(&position),
            OrderSurface::default(),
            &RiskProfile::default(),
        );
        assert_eq!(surface.intents.len(), 0);
        let mut portfolio = Portfolio::with_starting_cash(10.0);
        portfolio.kill_switch = true;
        let ctx = StrategyContext {
            market_id: MarketId(7),
            markets: &markets,
            portfolio: &portfolio,
            now: market.book.last_update,
        };
        let surface = apply_risk_controls(
            ctx,
            &market,
            Some(&position),
            OrderSurface::default(),
            &RiskProfile::default(),
        );
        assert_eq!(surface.intents[0].price, 0.41);
    }

    #[test]
    fn asset_budget_scales_buy_qty_down() {
        let limits = RiskProfile {
            asset_notional_limit: 2.0,
            regime_notional_limit: 10.0,
            corr_bucket_notional_limit: 10.0,
            ..RiskProfile::default()
        };
        let market_id = MarketId(7);
        let mut market = MarketState::new(market_id, Side::Up, Duration::from_secs(30));
        market.book.bids.push(L2Level {
            price: dec!(0.50),
            size: dec!(15),
        });
        market.book.asks.push(L2Level {
            price: dec!(0.51),
            size: dec!(8),
        });
        let mut tracked = TrackedMarket::placeholder(market_id);
        tracked.runtime.symbol = "BTC".to_string();
        tracked.state = market.clone();
        let mut markets = HashMap::new();
        markets.insert(market_id, tracked);
        let portfolio = Portfolio::with_starting_cash(10.0);
        let ctx = StrategyContext {
            market_id,
            markets: &markets,
            portfolio: &portfolio,
            now: market.book.last_update,
        };
        let surface = OrderSurface {
            intents: vec![OrderIntent {
                market_id,
                side: Side::Up,
                action: OrderAction::Buy,
                price: 0.5,
                qty: 10.0,
                ttl_ms: 100,
                aggressive: true,
            }],
        };
        let out = apply_risk_controls(ctx, &market, None, surface, &limits);
        assert!(out.intents[0].qty <= 4.0);
    }

    #[test]
    fn rolling_loss_triggers_exit_only_mode() {
        let limits = RiskProfile {
            max_loss: 0.50,
            max_loss_5m: 0.01,
            ..RiskProfile::default()
        };
        let market_id = MarketId(7);
        let now = std::time::Instant::now();
        let mut market = MarketState::new(market_id, Side::Up, Duration::from_secs(30));
        market.book.last_update = now;
        market.book.bids.push(L2Level {
            price: dec!(0.41),
            size: dec!(15),
        });
        market.book.asks.push(L2Level {
            price: dec!(0.47),
            size: dec!(8),
        });
        let mut tracked = TrackedMarket::placeholder(market_id);
        tracked.state = market.clone();
        let mut markets = HashMap::new();
        markets.insert(market_id, tracked);
        let mut portfolio = Portfolio::with_starting_cash(100.0);
        portfolio.apply_fill(market_id, OrderAction::Buy, 0.60, 10.0, 0.0, now);
        portfolio.apply_fill(market_id, OrderAction::Sell, 0.30, 10.0, 0.0, now);
        let ctx = StrategyContext {
            market_id,
            markets: &markets,
            portfolio: &portfolio,
            now,
        };
        let surface = apply_risk_controls(ctx, &market, None, OrderSurface::default(), &limits);
        assert_eq!(surface.intents.len(), 0);
    }
}
