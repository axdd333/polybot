use trading_core::config::{RegimeAdjust, SweepProfile};
use trading_core::market::book;
use trading_core::market::quote;
use trading_core::market::types::{MarketState, Regime, Side};
use trading_core::snapshot::MarketSnapshot;

#[derive(Debug, Clone, Copy)]
pub struct EdgeSummary {
    pub fair_gap: f64,
    pub net_buy: f64,
    pub net_sell_maker: f64,
    pub net_sell_taker: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct QueueQuality {
    pub maker_ok: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct ExitPlan {
    pub price: f64,
    pub ttl_ms: u64,
    pub aggressive: bool,
    pub reason: &'static str,
}

pub fn entry_signal_ok(best_bid: f64, best_ask: f64, micro: f64, edge: f64) -> bool {
    if best_ask <= 0.0 {
        return false;
    }
    if edge > 0.006 {
        return true;
    }
    if micro <= 0.0 {
        return false;
    }
    micro >= (best_ask - 0.0025) && micro > best_bid
}

pub fn directional_score(side: Side, score: f64) -> f64 {
    match side {
        Side::Up => score,
        Side::Down => -score,
    }
}

pub fn regime_adjust(cfg: &SweepProfile, regime: Regime) -> RegimeAdjust {
    match regime {
        Regime::Continuation => cfg.regime.continuation.clone(),
        Regime::Reversion => cfg.regime.reversion.clone(),
        Regime::Chop => cfg.regime.chop.clone(),
        Regime::Burst => cfg.regime.burst.clone(),
        Regime::ExpiryPinch => cfg.regime.expiry_pinch.clone(),
    }
}

pub fn edge_summary(cfg: &SweepProfile, market: &MarketState) -> EdgeSummary {
    let ask = book::best_ask(&market.book);
    let bid = book::best_bid(&market.book);
    let fair = market.fair_value;
    let buy = quote_cost(ask, market.taker_fee_bps + cfg.entry_slippage_bps);
    let sell_taker = quote_cost(fair, market.taker_fee_bps + cfg.exit_slippage_bps);
    EdgeSummary {
        fair_gap: fair - ask - buy - sell_taker,
        net_buy: fair - ask - buy - sell_taker,
        net_sell_maker: bid - fair - quote_cost(bid, market.maker_fee_bps),
        net_sell_taker: bid - fair - quote_cost(bid, market.taker_fee_bps + cfg.exit_slippage_bps),
    }
}

pub fn entry_cost_basis(ask: f64, taker_fee_bps: f64, entry_slippage_bps: f64) -> f64 {
    ask + quote_cost(ask, taker_fee_bps + entry_slippage_bps)
}

pub fn maker_fill_quality(cfg: &SweepProfile, market: &MarketState) -> QueueQuality {
    let ask_sz = level_size(&market.book.asks);
    let bid_sz = level_size(&market.book.bids);
    let trade = (market.trade_intensity / cfg.min_queue_trade_intensity.max(0.1)).min(2.0);
    let wall = market.wall_persistence_score.clamp(0.0, 1.0);
    let depth = (bid_sz.min(ask_sz) / (bid_sz.max(ask_sz) + 1.0)).clamp(0.0, 1.0);
    let cancel = (1.0 - market.cancel_skew.abs()).clamp(0.0, 1.0);
    let score = (trade * 0.35) + (wall * 0.25) + (depth * 0.2) + (cancel * 0.2);
    QueueQuality {
        maker_ok: score >= cfg.min_queue_fill_score,
    }
}

pub fn min_profitable_exit(avg_entry: f64, maker_fee_bps: f64, min_exit_roi: f64) -> f64 {
    let keep = 1.0 - maker_fee_bps.max(0.0) / 10_000.0;
    let need = avg_entry.max(0.0) * (1.0 + min_exit_roi.max(0.0));
    (need / keep.max(0.0001)).clamp(0.01, 0.99)
}

pub fn maker_exit_target(
    avg_entry: f64,
    exit_ceil: f64,
    fair: f64,
    maker_fee_bps: f64,
    min_exit_roi: f64,
) -> f64 {
    let floor = min_profitable_exit(avg_entry, maker_fee_bps, min_exit_roi);
    floor.max(fair - 0.002).min(exit_ceil).clamp(0.01, 0.99)
}

pub fn entry_has_exit_headroom(cfg: &SweepProfile, market: &MarketState) -> bool {
    let ask = book::best_ask(&market.book);
    if ask <= 0.0 {
        return false;
    }
    let entry = entry_cost_basis(ask, market.taker_fee_bps, cfg.entry_slippage_bps);
    let cap = cfg.take_profit_price.clamp(0.01, 0.99);
    let net = cap - quote_cost(cap, market.maker_fee_bps);
    net >= entry + market.min_tick_size * 0.5
}

pub fn entry_has_fast_exit(cfg: &SweepProfile, market: &MarketState) -> bool {
    let ask = book::best_ask(&market.book);
    if ask <= 0.0 {
        return false;
    }
    let entry = entry_cost_basis(ask, market.taker_fee_bps, cfg.entry_slippage_bps);
    let buf = market.min_tick_size.max(cfg.min_fair_gap_after_cost) * 0.5;
    market.fair_value >= entry + buf
}

pub fn exit_plan(
    cfg: &SweepProfile,
    market: &MarketState,
    avg_entry: f64,
    unrealized: f64,
    mfe: f64,
    stale_fair: bool,
    expired: bool,
) -> ExitPlan {
    let net = edge_summary(cfg, market);
    let queue = maker_fill_quality(cfg, market);
    let target = snap_sell(
        maker_exit_target(
            avg_entry,
            cfg.take_profit_price,
            market.fair_value,
            market.maker_fee_bps,
            cfg.min_exit_roi,
        ),
        market.min_tick_size,
    );
    let green = taker_green(cfg, market, avg_entry);
    if expired || market.expiry_pressure >= 0.9 {
        return hit(book::best_bid(&market.book), "expiry");
    }
    if stop_out(cfg, market, avg_entry, unrealized, stale_fair) {
        return hit(book::best_bid(&market.book), "stop");
    }
    if should_press_winner(cfg, market, queue, green, stale_fair) {
        return rest(target, "press");
    }
    if green && net.net_sell_taker <= cfg.scratch_edge {
        return hit(book::best_bid(&market.book), "scratch");
    }
    if green && (micro_flip(market) || stale_fair) {
        return hit(book::best_bid(&market.book), "rollover");
    }
    if green && trailing_stop(unrealized, mfe, cfg.trailing_drawdown_frac) {
        return hit(book::best_bid(&market.book), "trail");
    }
    if queue.maker_ok && net.net_sell_maker >= cfg.min_net_edge_sell {
        return rest(target, "passive");
    }
    if green {
        return hit(book::best_bid(&market.book), "green");
    }
    rest(target, "hold")
}

fn stop_out(
    cfg: &SweepProfile,
    market: &MarketState,
    avg_entry: f64,
    unrealized: f64,
    stale_fair: bool,
) -> bool {
    let bid = book::best_bid(&market.book);
    let loss = avg_entry - bid;
    let lim = avg_entry * (cfg.min_exit_roi * 2.0).max(0.02);
    unrealized < 0.0
        && loss >= lim
        && (stale_fair || micro_flip(market) || market.fair_value < avg_entry)
}

fn should_press_winner(
    cfg: &SweepProfile,
    market: &MarketState,
    queue: QueueQuality,
    green: bool,
    stale_fair: bool,
) -> bool {
    let bid = book::best_bid(&market.book);
    let edge = market.fair_value - bid;
    green
        && !stale_fair
        && queue.maker_ok
        && edge >= cfg.winner_hold_edge
        && market.wall_persistence_score >= cfg.winner_hold_wall_score
        && market.imbalance_5lvl >= 0.0
}

fn trailing_stop(unrealized: f64, mfe: f64, frac: f64) -> bool {
    mfe > 0.0 && unrealized < mfe * (1.0 - frac.clamp(0.05, 0.95))
}

fn micro_flip(market: &MarketState) -> bool {
    market.imbalance_5lvl < -0.12
        || market.wall_persistence_score < 0.12
        || market.microprice < book::best_bid(&market.book)
}

fn hit(price: f64, reason: &'static str) -> ExitPlan {
    ExitPlan {
        price: price.max(0.01),
        ttl_ms: 120,
        aggressive: true,
        reason,
    }
}

fn rest(price: f64, reason: &'static str) -> ExitPlan {
    ExitPlan {
        price,
        ttl_ms: 2_500,
        aggressive: false,
        reason,
    }
}

fn snap_sell(price: f64, tick: f64) -> f64 {
    if tick <= 0.0 {
        return price.clamp(0.01, 0.99);
    }
    let steps = (price / tick).ceil();
    (steps * tick).clamp(0.01, 0.99)
}

fn quote_cost(px: f64, bps: f64) -> f64 {
    px.max(0.0) * bps.max(0.0) / 10_000.0
}

fn taker_green(cfg: &SweepProfile, market: &MarketState, avg_entry: f64) -> bool {
    let bid = book::best_bid(&market.book);
    let fee = quote_cost(bid, market.taker_fee_bps + cfg.exit_slippage_bps);
    let buf = market.min_tick_size.max(0.001) / 10.0;
    bid - fee > avg_entry + buf
}

fn level_size(levels: &[trading_core::market::types::L2Level]) -> f64 {
    levels
        .first()
        .map(|lvl| quote::decimal_to_f64(lvl.size))
        .unwrap_or(0.0)
}

/// Sorting key for market display/priority — positions first, then cheapest asks.
pub fn market_sort_key(market: &MarketSnapshot, entry_threshold: f64) -> f64 {
    if market.position_qty > 0.0 {
        10_000.0 - market.avg_entry
    } else if trading_core::market::quote::valid_live_quote(market.best_bid, market.best_ask)
        && market.best_ask <= entry_threshold
    {
        1_000.0 - market.best_ask
    } else if trading_core::market::quote::valid_live_quote(market.best_bid, market.best_ask) {
        100.0 - market.best_ask
    } else {
        -999.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use std::time::Duration;
    use trading_core::market::types::{L2Level, MarketId};

    #[test]
    fn exit_target_respects_cap() {
        let px = maker_exit_target(0.98, 0.96, 0.99, 0.0, 0.03);
        assert!((px - 0.96).abs() < 1e-9);
    }

    #[test]
    fn exit_target_covers_fee_and_roi() {
        let px = maker_exit_target(0.50, 0.96, 0.51, 20.0, 0.03);
        assert!(px >= 0.516);
        assert!(px < 0.517);
    }

    #[test]
    fn sell_target_snaps_to_tick() {
        let px = snap_sell(0.766, 0.01);
        assert!((px - 0.77).abs() < 1e-9);
    }

    #[test]
    fn capped_exit_headroom_allows_near_cap_entry() {
        let cfg = SweepProfile::default();
        let mut market = MarketState::new(MarketId(3), Side::Up, Duration::from_secs(60));
        market.book.bids.push(L2Level {
            price: dec!(0.96),
            size: dec!(5),
        });
        market.book.asks.push(L2Level {
            price: dec!(0.97),
            size: dec!(5),
        });
        assert!(entry_has_exit_headroom(&cfg, &market));
    }

    #[test]
    fn capped_exit_headroom_blocks_when_cap_cannot_clear_cost() {
        let cfg = SweepProfile::default();
        let mut market = MarketState::new(MarketId(4), Side::Up, Duration::from_secs(60));
        market.book.bids.push(L2Level {
            price: dec!(0.98),
            size: dec!(5),
        });
        market.book.asks.push(L2Level {
            price: dec!(0.989),
            size: dec!(5),
        });
        assert!(!entry_has_exit_headroom(&cfg, &market));
    }

    #[test]
    fn fee_edge_turns_negative_when_costs_dominate() {
        let cfg = SweepProfile::default();
        let mut market = MarketState::new(MarketId(1), Side::Up, Duration::from_secs(60));
        market.book.bids.push(L2Level {
            price: dec!(0.50),
            size: dec!(5),
        });
        market.book.asks.push(L2Level {
            price: dec!(0.51),
            size: dec!(5),
        });
        market.fair_value = 0.512;
        market.taker_fee_bps = 40.0;
        let edge = edge_summary(&cfg, &market);
        assert!(edge.net_buy < 0.0);
    }

    #[test]
    fn poor_queue_holds_until_green() {
        let cfg = SweepProfile::default();
        let mut market = MarketState::new(MarketId(1), Side::Up, Duration::from_secs(60));
        market.book.bids.push(L2Level {
            price: dec!(0.49),
            size: dec!(1),
        });
        market.book.asks.push(L2Level {
            price: dec!(0.55),
            size: dec!(30),
        });
        market.fair_value = 0.57;
        let plan = exit_plan(&cfg, &market, 0.5, -0.01, 0.02, false, false);
        assert!(!plan.aggressive);
        assert_eq!(plan.reason, "hold");
    }

    #[test]
    fn stale_fair_does_not_dump_red_position() {
        let cfg = SweepProfile::default();
        let mut market = MarketState::new(MarketId(1), Side::Up, Duration::from_secs(60));
        market.book.bids.push(L2Level {
            price: dec!(0.50),
            size: dec!(5),
        });
        market.book.asks.push(L2Level {
            price: dec!(0.51),
            size: dec!(5),
        });
        market.fair_value = 0.48;
        let plan = exit_plan(&cfg, &market, 0.52, -0.02, 0.01, true, false);
        assert!(!plan.aggressive);
        assert!(matches!(plan.reason, "hold" | "passive"));
    }

    #[test]
    fn winner_with_maker_tail_is_pressed() {
        let cfg = SweepProfile::default();
        let mut market = MarketState::new(MarketId(1), Side::Up, Duration::from_secs(60));
        market.book.bids.push(L2Level {
            price: dec!(0.58),
            size: dec!(20),
        });
        market.book.asks.push(L2Level {
            price: dec!(0.59),
            size: dec!(20),
        });
        market.fair_value = 0.63;
        market.trade_intensity = 8.0;
        market.wall_persistence_score = 0.8;
        market.cancel_skew = 0.0;
        market.imbalance_5lvl = 0.3;
        let plan = exit_plan(&cfg, &market, 0.5, 0.06, 0.08, false, false);
        assert!(!plan.aggressive);
        assert_eq!(plan.reason, "press");
    }

    #[test]
    fn red_thesis_break_hits_stop() {
        let cfg = SweepProfile::default();
        let mut market = MarketState::new(MarketId(2), Side::Up, Duration::from_secs(60));
        market.book.bids.push(L2Level {
            price: dec!(0.44),
            size: dec!(20),
        });
        market.book.asks.push(L2Level {
            price: dec!(0.45),
            size: dec!(20),
        });
        market.fair_value = 0.43;
        market.wall_persistence_score = 0.05;
        market.imbalance_5lvl = -0.2;
        market.microprice = 0.43;
        let plan = exit_plan(&cfg, &market, 0.5, -0.06, 0.01, false, false);
        assert!(plan.aggressive);
        assert_eq!(plan.reason, "stop");
    }
}
