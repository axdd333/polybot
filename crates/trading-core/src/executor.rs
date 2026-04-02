use crate::market::types::{OrderAction, OrderIntent};

/// Minimal trade data needed for simulated fill decisions.
pub struct RecentTrade {
    pub price: f64,
    pub size: f64,
}

/// Market context required to decide whether an order fills.
pub struct FillContext {
    pub best_bid: f64,
    pub best_ask: f64,
    pub recent_trades: Vec<RecentTrade>,
}

/// Pluggable execution backend — swap between simulated and live fills.
pub trait Executor: Send + Sync {
    /// Returns the fill price if the intent would fill, or `None`.
    fn try_fill(&self, intent: &OrderIntent, ctx: &FillContext) -> Option<f64>;
}

// ---------------------------------------------------------------------------
// Simulated fills (paper trading)
// ---------------------------------------------------------------------------

pub struct PaperExecutor;

impl Executor for PaperExecutor {
    fn try_fill(&self, intent: &OrderIntent, ctx: &FillContext) -> Option<f64> {
        match intent.action {
            OrderAction::Buy if intent.aggressive && ctx.best_ask > 0.0 => Some(ctx.best_ask),
            OrderAction::Buy if ctx.best_ask > 0.0 && ctx.best_ask <= intent.price => {
                Some(ctx.best_ask)
            }
            OrderAction::Buy => ctx
                .recent_trades
                .iter()
                .find(|t| t.price <= intent.price && t.size > 0.0)
                .map(|t| t.price),
            OrderAction::Sell if intent.aggressive && ctx.best_bid > 0.0 => Some(ctx.best_bid),
            OrderAction::Sell if ctx.best_bid > 0.0 && ctx.best_bid >= intent.price => {
                Some(ctx.best_bid)
            }
            OrderAction::Sell => ctx
                .recent_trades
                .iter()
                .find(|t| t.price >= intent.price && t.size > 0.0)
                .map(|t| t.price),
        }
    }
}

// ---------------------------------------------------------------------------
// Live execution stub (wallet-backed orders via Polymarket SDK)
// ---------------------------------------------------------------------------

pub struct NoopLiveOrderGateway {
    pub enabled: bool,
}

impl Default for NoopLiveOrderGateway {
    fn default() -> Self {
        Self { enabled: false }
    }
}

impl Executor for NoopLiveOrderGateway {
    fn try_fill(&self, _intent: &OrderIntent, _ctx: &FillContext) -> Option<f64> {
        if !self.enabled {
            return None;
        }
        None
    }
}
