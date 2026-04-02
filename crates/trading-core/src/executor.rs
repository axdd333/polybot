use async_trait::async_trait;
use crate::market::types::{OrderAction, OrderIntent};
use crate::state::PendingLiveOrder;

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

pub struct ExecutionRequest<'a> {
    pub market_id: crate::market::types::MarketId,
    pub token_id: &'a str,
    pub condition_id: &'a str,
    pub surface: &'a crate::market::types::OrderSurface,
    pub pending: Option<&'a PendingLiveOrder>,
    pub ctx: &'a FillContext,
}

#[derive(Debug, Clone)]
pub enum ExecutionReport {
    PaperFill {
        action: OrderAction,
        price: f64,
        qty: f64,
    },
    LiveOrderAccepted {
        order_id: String,
        action: OrderAction,
        price: f64,
        qty: f64,
    },
    LiveOrderCancelled {
        order_id: String,
    },
    LiveOrderRejected {
        reason: String,
    },
}

/// Pluggable execution backend — swap between simulated and live fills.
#[async_trait]
pub trait Executor: Send + Sync {
    async fn execute(&self, request: ExecutionRequest<'_>) -> anyhow::Result<Vec<ExecutionReport>>;
}

// ---------------------------------------------------------------------------
// Simulated fills (paper trading)
// ---------------------------------------------------------------------------

pub struct PaperExecutor;

#[async_trait]
impl Executor for PaperExecutor {
    async fn execute(&self, request: ExecutionRequest<'_>) -> anyhow::Result<Vec<ExecutionReport>> {
        let mut reports = Vec::new();
        for intent in &request.surface.intents {
            if let Some(fill_price) = try_fill(intent, request.ctx) {
                reports.push(ExecutionReport::PaperFill {
                    action: intent.action,
                    price: fill_price,
                    qty: intent.qty,
                });
            }
        }
        Ok(reports)
    }
}

fn try_fill(intent: &OrderIntent, ctx: &FillContext) -> Option<f64> {
    match intent.action {
        OrderAction::Buy if intent.aggressive && ctx.best_ask > 0.0 => Some(ctx.best_ask),
        OrderAction::Buy if ctx.best_ask > 0.0 && ctx.best_ask <= intent.price => Some(ctx.best_ask),
        OrderAction::Buy => ctx
            .recent_trades
            .iter()
            .find(|t| t.price <= intent.price && t.size > 0.0)
            .map(|t| t.price),
        OrderAction::Sell if intent.aggressive && ctx.best_bid > 0.0 => Some(ctx.best_bid),
        OrderAction::Sell if ctx.best_bid > 0.0 && ctx.best_bid >= intent.price => Some(ctx.best_bid),
        OrderAction::Sell => ctx
            .recent_trades
            .iter()
            .find(|t| t.price >= intent.price && t.size > 0.0)
            .map(|t| t.price),
    }
}
