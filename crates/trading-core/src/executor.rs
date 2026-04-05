use crate::config::PaperProfile;
use crate::market::book;
use crate::market::quote::decimal_to_f64;
use crate::market::types::{MarketId, OrderAction, OrderBook, OrderIntent};
use crate::state::PendingLiveOrder;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct RecentTrade {
    pub price: f64,
    pub size: f64,
    pub ts: Instant,
}

pub struct FillContext {
    pub book: OrderBook,
    pub recent_trades: Vec<RecentTrade>,
    pub now: Instant,
    pub min_tick_size: f64,
    pub min_order_size: f64,
    pub maker_fee_bps: f64,
    pub taker_fee_bps: f64,
    pub accepting_orders: bool,
}

pub struct ExecutionRequest<'a> {
    pub market_id: MarketId,
    pub token_id: &'a str,
    pub condition_id: &'a str,
    pub surface: &'a crate::market::types::OrderSurface,
    pub pending: Option<&'a PendingLiveOrder>,
    pub ctx: &'a FillContext,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaperOrderStatus {
    Submitted,
    Live,
    PartiallyFilled,
    Filled,
    Cancelled,
    Expired,
    Rejected,
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiquidityRole {
    Maker,
    Taker,
}

#[derive(Debug, Clone)]
pub enum ExecutionReport {
    PaperFill {
        order_id: String,
        action: OrderAction,
        price: f64,
        qty: f64,
        fee: f64,
        role: LiquidityRole,
    },
    PaperOrderAccepted {
        order_id: String,
        status: PaperOrderStatus,
    },
    PaperOrderUpdate {
        order_id: String,
        status: PaperOrderStatus,
        filled_qty: f64,
        remaining_qty: f64,
        note: String,
    },
    PaperOrderRejected {
        reason: String,
    },
    PaperLog {
        line: String,
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

#[async_trait]
pub trait Executor: Send + Sync {
    async fn execute(&self, request: ExecutionRequest<'_>) -> anyhow::Result<Vec<ExecutionReport>>;
}

pub struct PaperExecutor {
    cfg: PaperProfile,
    state: Mutex<PaperState>,
}

#[derive(Default)]
struct PaperState {
    seq: u64,
    orders: HashMap<MarketId, PaperOrder>,
}

#[derive(Clone)]
struct PaperOrder {
    id: String,
    intent: OrderIntent,
    placed_at: Instant,
    arrive_at: Instant,
    expire_at: Instant,
    last_eval_at: Instant,
    open_qty: f64,
    filled_qty: f64,
    queue_ahead: f64,
    status: PaperOrderStatus,
}

impl PaperExecutor {
    pub fn new(cfg: PaperProfile) -> Self {
        Self {
            cfg,
            state: Mutex::new(PaperState::default()),
        }
    }
}

#[async_trait]
impl Executor for PaperExecutor {
    async fn execute(&self, request: ExecutionRequest<'_>) -> anyhow::Result<Vec<ExecutionReport>> {
        let mut state = self.state.lock().expect("paper state mutex poisoned");
        let mut out = Vec::new();
        let desired = single_intent(request.surface, &mut out);

        advance_order(
            &self.cfg,
            &mut state.orders,
            request.market_id,
            request.ctx,
            &mut out,
        );

        match (state.orders.get(&request.market_id).cloned(), desired) {
            (Some(cur), Some(next)) if same_order(&cur.intent, &next) => {}
            (Some(cur), Some(next)) => {
                cancel_order(&mut state.orders, request.market_id, &cur.id, &mut out);
                accept_order(&self.cfg, &mut state, request, next, &mut out);
            }
            (Some(cur), None) => {
                cancel_order(&mut state.orders, request.market_id, &cur.id, &mut out);
            }
            (None, Some(next)) => {
                accept_order(&self.cfg, &mut state, request, next, &mut out);
            }
            (None, None) => {}
        }

        Ok(out)
    }
}

fn single_intent(
    surface: &crate::market::types::OrderSurface,
    out: &mut Vec<ExecutionReport>,
) -> Option<OrderIntent> {
    if surface.intents.len() <= 1 {
        return surface.intents.first().cloned();
    }

    out.push(ExecutionReport::PaperOrderRejected {
        reason: "paper exchange supports one active order per market".to_string(),
    });
    None
}

fn accept_order(
    cfg: &PaperProfile,
    state: &mut PaperState,
    request: ExecutionRequest<'_>,
    intent: OrderIntent,
    out: &mut Vec<ExecutionReport>,
) {
    if let Some(reason) = validate_intent(cfg, request.ctx, &intent) {
        out.push(ExecutionReport::PaperOrderRejected { reason });
        return;
    }

    state.seq += 1;
    let now = request.ctx.now;
    let order_id = format!("paper-{}-{}", request.market_id.0, state.seq);
    let order = PaperOrder {
        id: order_id.clone(),
        intent,
        placed_at: now,
        arrive_at: now + Duration::from_millis(cfg.latency_ms),
        expire_at: now + Duration::from_millis(cfg.latency_ms.max(1)),
        last_eval_at: now,
        open_qty: request.surface.intents[0].qty,
        filled_qty: 0.0,
        queue_ahead: queued_ahead(request.ctx, &request.surface.intents[0], cfg),
        status: PaperOrderStatus::Submitted,
    };

    let mut order = order;
    if !order.intent.aggressive {
        let ttl = Duration::from_millis(order.intent.ttl_ms.max(cfg.latency_ms));
        order.expire_at = order.placed_at + ttl;
    }
    state.orders.insert(request.market_id, order.clone());

    out.push(ExecutionReport::PaperOrderAccepted {
        order_id: order_id.clone(),
        status: PaperOrderStatus::Submitted,
    });
    out.push(ExecutionReport::PaperLog {
        line: format!(
            "decision {} bid {:.3} ask {:.3} px {:.3} qty {:.1} lat {}ms",
            order_id,
            book::best_bid(&request.ctx.book),
            book::best_ask(&request.ctx.book),
            order.intent.price,
            order.intent.qty,
            cfg.latency_ms
        ),
    });
}

fn validate_intent(cfg: &PaperProfile, ctx: &FillContext, intent: &OrderIntent) -> Option<String> {
    if !ctx.accepting_orders {
        return Some("exchange not accepting orders".to_string());
    }
    if ctx.now.duration_since(ctx.book.last_update) > Duration::from_millis(cfg.stale_after_ms) {
        return Some("book stale at decision".to_string());
    }
    if intent.qty + 1e-9 < ctx.min_order_size {
        return Some(format!(
            "order qty {:.3} below min {:.3}",
            intent.qty, ctx.min_order_size
        ));
    }
    if !tick_valid(intent.price, ctx.min_tick_size) {
        return Some(format!(
            "price {:.3} violates tick {:.3}",
            intent.price, ctx.min_tick_size
        ));
    }
    None
}

fn advance_order(
    cfg: &PaperProfile,
    orders: &mut HashMap<MarketId, PaperOrder>,
    market_id: MarketId,
    ctx: &FillContext,
    out: &mut Vec<ExecutionReport>,
) {
    let Some(mut order) = orders.remove(&market_id) else {
        return;
    };

    if ctx.now.duration_since(ctx.book.last_update) > Duration::from_millis(cfg.stale_after_ms) {
        order.status = PaperOrderStatus::Stale;
        out.push(ExecutionReport::PaperOrderUpdate {
            order_id: order.id,
            status: order.status,
            filled_qty: order.filled_qty,
            remaining_qty: order.open_qty,
            note: "arrival rejected on stale book".to_string(),
        });
        return;
    }

    if ctx.now < order.arrive_at {
        orders.insert(market_id, order);
        return;
    }

    if order.status == PaperOrderStatus::Submitted {
        order.status = PaperOrderStatus::Live;
        out.push(ExecutionReport::PaperLog {
            line: format!(
                "arrival {} bid {:.3} ask {:.3} age {}ms",
                order.id,
                book::best_bid(&ctx.book),
                book::best_ask(&ctx.book),
                ctx.now.duration_since(ctx.book.last_update).as_millis()
            ),
        });
    }

    if order.intent.aggressive {
        fill_taker(&mut order, ctx, out);
    } else {
        fill_resting(cfg, &mut order, ctx, out);
    }

    order.last_eval_at = ctx.now;
    if order.open_qty <= 1e-9 {
        order.status = PaperOrderStatus::Filled;
        out.push(ExecutionReport::PaperOrderUpdate {
            order_id: order.id,
            status: order.status,
            filled_qty: order.filled_qty,
            remaining_qty: 0.0,
            note: "order done".to_string(),
        });
        return;
    }

    if order.intent.aggressive || ctx.now >= order.expire_at {
        order.status = PaperOrderStatus::Expired;
        out.push(ExecutionReport::PaperOrderUpdate {
            order_id: order.id,
            status: order.status,
            filled_qty: order.filled_qty,
            remaining_qty: order.open_qty,
            note: "remaining qty expired".to_string(),
        });
        return;
    }

    if order.filled_qty > 0.0 {
        order.status = PaperOrderStatus::PartiallyFilled;
    }
    orders.insert(market_id, order);
}

fn fill_taker(order: &mut PaperOrder, ctx: &FillContext, out: &mut Vec<ExecutionReport>) {
    let fills = sweep_book(
        &ctx.book,
        order.intent.action,
        order.intent.price,
        order.open_qty,
    );
    let fee_bps = ctx.taker_fee_bps;
    let mut done = 0.0;

    for (price, qty) in fills {
        let fee = notional(price, qty) * fee_bps / 10_000.0;
        order.filled_qty += qty;
        order.open_qty -= qty;
        done += qty;
        out.push(fill_report(
            &order.id,
            order.intent.action,
            price,
            qty,
            fee,
            LiquidityRole::Taker,
        ));
    }

    if done > 0.0 {
        out.push(ExecutionReport::PaperOrderUpdate {
            order_id: order.id.clone(),
            status: PaperOrderStatus::PartiallyFilled,
            filled_qty: order.filled_qty,
            remaining_qty: order.open_qty.max(0.0),
            note: "depth walk complete".to_string(),
        });
    }
}

fn fill_resting(
    cfg: &PaperProfile,
    order: &mut PaperOrder,
    ctx: &FillContext,
    out: &mut Vec<ExecutionReport>,
) {
    let mut traded = traded_qty_since(order, ctx);
    if traded <= 0.0 {
        return;
    }

    let queue = order.queue_ahead.min(traded);
    order.queue_ahead -= queue;
    traded -= queue;
    if traded <= 0.0 {
        return;
    }

    let qty = traded.min(order.open_qty);
    let fee = notional(order.intent.price, qty) * ctx.maker_fee_bps / 10_000.0;
    order.filled_qty += qty;
    order.open_qty -= qty;
    out.push(fill_report(
        &order.id,
        order.intent.action,
        order.intent.price,
        qty,
        fee,
        LiquidityRole::Maker,
    ));
    out.push(ExecutionReport::PaperLog {
        line: format!(
            "resting {} queue {:.1} fill {:.1} mult {:.2}",
            order.id, order.queue_ahead, qty, cfg.rest_queue_mult
        ),
    });
}

fn traded_qty_since(order: &PaperOrder, ctx: &FillContext) -> f64 {
    ctx.recent_trades
        .iter()
        .filter(|t| t.ts > order.last_eval_at)
        .filter(|t| trade_hits_order(t.price, order))
        .map(|t| t.size.max(0.0))
        .sum()
}

fn trade_hits_order(price: f64, order: &PaperOrder) -> bool {
    match order.intent.action {
        OrderAction::Buy => price <= order.intent.price + 1e-9,
        OrderAction::Sell => price >= order.intent.price - 1e-9,
    }
}

fn sweep_book(book: &OrderBook, action: OrderAction, limit: f64, qty: f64) -> Vec<(f64, f64)> {
    let mut rem = qty.max(0.0);
    let mut out = Vec::new();
    let levels = match action {
        OrderAction::Buy => &book.asks,
        OrderAction::Sell => &book.bids,
    };

    for level in levels {
        if rem <= 1e-9 {
            break;
        }
        let px = decimal_to_f64(level.price);
        let sz = decimal_to_f64(level.size).max(0.0);
        if sz <= 0.0 || !within_limit(action, px, limit) {
            break;
        }
        let take = sz.min(rem);
        out.push((px, take));
        rem -= take;
    }

    out
}

fn within_limit(action: OrderAction, px: f64, limit: f64) -> bool {
    match action {
        OrderAction::Buy => px <= limit + 1e-9,
        OrderAction::Sell => px >= limit - 1e-9,
    }
}

fn fill_report(
    order_id: &str,
    action: OrderAction,
    price: f64,
    qty: f64,
    fee: f64,
    role: LiquidityRole,
) -> ExecutionReport {
    ExecutionReport::PaperFill {
        order_id: order_id.to_string(),
        action,
        price,
        qty,
        fee,
        role,
    }
}

fn cancel_order(
    orders: &mut HashMap<MarketId, PaperOrder>,
    market_id: MarketId,
    order_id: &str,
    out: &mut Vec<ExecutionReport>,
) {
    if let Some(order) = orders.remove(&market_id) {
        out.push(ExecutionReport::PaperOrderUpdate {
            order_id: order_id.to_string(),
            status: PaperOrderStatus::Cancelled,
            filled_qty: order.filled_qty,
            remaining_qty: order.open_qty,
            note: "order canceled".to_string(),
        });
    }
}

fn same_order(a: &OrderIntent, b: &OrderIntent) -> bool {
    a.action == b.action
        && (a.price - b.price).abs() < 1e-9
        && (a.qty - b.qty).abs() < 1e-9
        && a.aggressive == b.aggressive
}

fn queued_ahead(ctx: &FillContext, intent: &OrderIntent, cfg: &PaperProfile) -> f64 {
    let side = match intent.action {
        OrderAction::Buy => &ctx.book.bids,
        OrderAction::Sell => &ctx.book.asks,
    };
    let shown = side
        .iter()
        .find(|level| (decimal_to_f64(level.price) - intent.price).abs() < 1e-9)
        .map(|level| decimal_to_f64(level.size))
        .unwrap_or(0.0);
    shown * cfg.rest_queue_mult.max(0.0)
}

fn tick_valid(price: f64, tick: f64) -> bool {
    if tick <= 0.0 {
        return true;
    }
    let steps = (price / tick).round();
    ((steps * tick) - price).abs() <= 1e-6
}

fn notional(price: f64, qty: f64) -> f64 {
    price.max(0.0) * qty.max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::types::{L2Level, OrderSurface, Side};
    use rust_decimal_macros::dec;

    #[test]
    fn aggressive_order_waits_for_arrival() {
        let cfg = PaperProfile {
            latency_ms: 200,
            stale_after_ms: 1_000,
            rest_queue_mult: 1.0,
        };
        let now = Instant::now();
        let ctx = ctx(now, now);
        let surface = OrderSurface {
            intents: vec![intent(OrderAction::Buy, 0.53, 3.0, true)],
        };
        let order = surface.intents[0].clone();
        let req = req(&ctx, &surface);
        let mut state = PaperState::default();
        let mut out = Vec::new();

        accept_order(&cfg, &mut state, req, order, &mut out);
        assert_eq!(state.orders.len(), 1);
        out.clear();

        advance_order(&cfg, &mut state.orders, MarketId(7), &ctx, &mut out);
        assert!(out.is_empty());
        assert!(state.orders.contains_key(&MarketId(7)));
    }

    #[test]
    fn taker_walks_depth_and_charges_fee() {
        let cfg = PaperProfile {
            latency_ms: 10,
            stale_after_ms: 1_000,
            rest_queue_mult: 1.0,
        };
        let placed_at = Instant::now();
        let decision = ctx(placed_at, placed_at);
        let surface = OrderSurface {
            intents: vec![intent(OrderAction::Buy, 0.52, 7.0, true)],
        };
        let order = surface.intents[0].clone();
        let req = req(&decision, &surface);
        let mut state = PaperState::default();
        let mut out = Vec::new();

        accept_order(&cfg, &mut state, req, order, &mut out);
        out.clear();
        let arrive = ctx(placed_at, placed_at + Duration::from_millis(20));
        advance_order(&cfg, &mut state.orders, MarketId(7), &arrive, &mut out);

        let fills: Vec<_> = out
            .iter()
            .filter_map(|item| match item {
                ExecutionReport::PaperFill {
                    price, qty, fee, ..
                } => Some((*price, *qty, *fee)),
                _ => None,
            })
            .collect();
        assert_eq!(fills.len(), 2);
        assert_eq!(fills[0].0, 0.51);
        assert_eq!(fills[0].1, 5.0);
        assert_eq!(fills[1].0, 0.52);
        assert_eq!(fills[1].1, 2.0);
        assert!(fills[0].2 > 0.0);
        assert!(!state.orders.contains_key(&MarketId(7)));
    }

    #[test]
    fn stale_book_invalidates_order() {
        let cfg = PaperProfile {
            latency_ms: 10,
            stale_after_ms: 5,
            rest_queue_mult: 1.0,
        };
        let placed_at = Instant::now();
        let ctx0 = ctx(placed_at, placed_at);
        let surface = OrderSurface {
            intents: vec![intent(OrderAction::Buy, 0.51, 2.0, true)],
        };
        let order = surface.intents[0].clone();
        let req = req(&ctx0, &surface);
        let mut state = PaperState::default();
        let mut out = Vec::new();

        accept_order(&cfg, &mut state, req, order, &mut out);
        out.clear();

        let ctx1 = ctx(placed_at, placed_at + Duration::from_millis(50));
        advance_order(&cfg, &mut state.orders, MarketId(7), &ctx1, &mut out);

        assert!(matches!(
            out.last(),
            Some(ExecutionReport::PaperOrderUpdate {
                status: PaperOrderStatus::Stale,
                ..
            })
        ));
        assert!(!state.orders.contains_key(&MarketId(7)));
    }

    #[test]
    fn resting_order_needs_trade_flow() {
        let cfg = PaperProfile {
            latency_ms: 10,
            stale_after_ms: 1_000,
            rest_queue_mult: 1.0,
        };
        let placed_at = Instant::now();
        let mut base = ctx(placed_at, placed_at);
        base.book.bids = vec![L2Level {
            price: dec!(0.49),
            size: dec!(3),
        }];
        let surface = OrderSurface {
            intents: vec![intent(OrderAction::Buy, 0.49, 2.0, false)],
        };
        let order = surface.intents[0].clone();
        let req = req(&base, &surface);
        let mut state = PaperState::default();
        let mut out = Vec::new();

        accept_order(&cfg, &mut state, req, order, &mut out);
        out.clear();

        let mut no_trade = ctx(placed_at, placed_at + Duration::from_millis(20));
        no_trade.book = base.book.clone();
        advance_order(&cfg, &mut state.orders, MarketId(7), &no_trade, &mut out);
        assert!(out
            .iter()
            .all(|item| { !matches!(item, ExecutionReport::PaperFill { .. }) }));

        out.clear();
        let mut trade_ctx = ctx(placed_at, placed_at + Duration::from_millis(40));
        trade_ctx.book = base.book.clone();
        trade_ctx.recent_trades = vec![RecentTrade {
            price: 0.49,
            size: 5.0,
            ts: placed_at + Duration::from_millis(30),
        }];
        advance_order(&cfg, &mut state.orders, MarketId(7), &trade_ctx, &mut out);
        assert!(out
            .iter()
            .any(|item| { matches!(item, ExecutionReport::PaperFill { qty, .. } if *qty > 0.0) }));
    }

    fn req<'a>(ctx: &'a FillContext, surface: &'a OrderSurface) -> ExecutionRequest<'a> {
        ExecutionRequest {
            market_id: MarketId(7),
            token_id: "7",
            condition_id: "cond",
            surface,
            pending: None,
            ctx,
        }
    }

    fn intent(action: OrderAction, price: f64, qty: f64, aggressive: bool) -> OrderIntent {
        OrderIntent {
            market_id: MarketId(7),
            side: Side::Up,
            action,
            price,
            qty,
            ttl_ms: 5_000,
            aggressive,
        }
    }

    fn ctx(book_ts: Instant, now: Instant) -> FillContext {
        FillContext {
            book: OrderBook {
                bids: vec![
                    L2Level {
                        price: dec!(0.49),
                        size: dec!(6),
                    },
                    L2Level {
                        price: dec!(0.48),
                        size: dec!(4),
                    },
                ],
                asks: vec![
                    L2Level {
                        price: dec!(0.51),
                        size: dec!(5),
                    },
                    L2Level {
                        price: dec!(0.52),
                        size: dec!(6),
                    },
                ],
                last_update: book_ts,
            },
            recent_trades: Vec::new(),
            now,
            min_tick_size: 0.01,
            min_order_size: 1.0,
            maker_fee_bps: 10.0,
            taker_fee_bps: 20.0,
            accepting_orders: true,
        }
    }
}
