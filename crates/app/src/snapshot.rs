use std::time::Duration;
use trading_core::analyzer;
use trading_core::config::SweepProfile;
use trading_core::engine::TradingEngine;
use trading_core::market::{book, quote};
use trading_core::snapshot::{MarketSnapshot, WorldSnapshot};

const JOURNAL_TAIL_MAX: usize = 256;

pub fn build_snapshot(engine: &TradingEngine, sweep: &SweepProfile) -> WorldSnapshot {
    let mut markets: Vec<MarketSnapshot> = engine
        .state
        .markets
        .markets
        .values()
        .map(|tracked| {
            let market = &tracked.state;
            let intents = tracked.planned_surface.clone().intents;
            let fair_series = analyzer::to_series(&tracked.runtime.fair_history);
            let edge_series = analyzer::to_series(&tracked.runtime.edge_history);
            let flow_series = analyzer::to_series(&tracked.runtime.flow_history);
            let micro_series = analyzer::to_series(&tracked.runtime.micro_history);
            let best_bid = book::best_bid(&market.book);
            let best_ask = book::best_ask(&market.book);
            let bid_size = market
                .book
                .bids
                .first()
                .map(|level| quote::decimal_to_f64(level.size))
                .unwrap_or(0.0);
            let ask_size = market
                .book
                .asks
                .first()
                .map(|level| quote::decimal_to_f64(level.size))
                .unwrap_or(0.0);
            let (position_qty, avg_entry, unrealized_pnl) = engine
                .state
                .portfolio
                .position(market.market_id)
                .map(|p| (p.qty, p.avg_px, p.unrealized))
                .unwrap_or((0.0, 0.0, 0.0));

            MarketSnapshot {
                market_id: market.market_id,
                title: tracked.runtime.title.clone(),
                symbol: tracked.runtime.symbol.clone(),
                window_label: tracked.runtime.window_label.clone(),
                end_label: tracked.runtime.end_label.clone(),
                side: market.side,
                regime: market.regime,
                score: market.model_score,
                fair_value: market.fair_value,
                edge_buy: market.edge_buy,
                edge_sell: market.edge_sell,
                best_bid,
                best_ask,
                bid_size,
                ask_size,
                spread_ticks: market.spread_ticks,
                microprice: market.microprice,
                trade_intensity: market.trade_intensity,
                burstiness: market.burstiness,
                torsion: market.cross_window_torsion,
                void_score: market.liquidity_void_score,
                wall_score: market.wall_persistence_score,
                expiry_secs: market
                    .time_to_expiry
                    .saturating_sub(market.expiry_anchored_at.elapsed())
                    .as_secs(),
                position_qty,
                avg_entry,
                unrealized_pnl,
                features: tracked.features.clone(),
                intents,
                fair_series,
                edge_series,
                flow_series,
                micro_series,
                maker_fee_bps: market.maker_fee_bps,
                taker_fee_bps: market.taker_fee_bps,
                min_tick_size: market.min_tick_size,
            }
        })
        .collect();

    markets.sort_by(|a, b| {
        crate::planner::market_sort_key(b, sweep.max_entry_price)
            .partial_cmp(&crate::planner::market_sort_key(a, sweep.max_entry_price))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let journal_tail: Vec<String> = engine
        .state
        .run
        .journal
        .iter()
        .rev()
        .take(JOURNAL_TAIL_MAX)
        .cloned()
        .collect();
    let equity = engine.portfolio_equity();
    let unrealized_pnl = engine.state.portfolio.total_unrealized();
    let snapshot_now = engine.state.now;
    let open_positions = markets
        .iter()
        .filter(|market| market.position_qty > 0.0)
        .count();
    let eligible_markets = engine
        .state
        .markets
        .markets
        .values()
        .filter(|tracked| {
            engine
                .state
                .portfolio
                .inventory_for(tracked.state.market_id)
                .abs()
                <= 0.0
                && !tracked.planned_surface.intents.is_empty()
        })
        .count();
    let (ref_symbol, ref_spot) = engine
        .state
        .underlyings
        .tapes
        .iter()
        .filter_map(|(symbol, tape)| tape.back().map(|point| (symbol.clone(), point.px)))
        .max_by(|a, b| a.0.cmp(&b.0))
        .unwrap_or_else(|| ("-".to_string(), 0.0));
    let signal_strength = engine
        .state
        .markets
        .markets
        .keys()
        .filter_map(|&market_id| engine.momentum_signal(market_id))
        .map(|(_, signal)| signal)
        .fold(0.0_f64, f64::max);
    let flow_window = Duration::from_secs(sweep.flow_window_secs);
    let flow_pnl_per_min = engine
        .state
        .portfolio
        .realized_per_min(snapshot_now, flow_window);
    let cycle_rate_per_min = engine
        .state
        .portfolio
        .closed_trades_per_min(snapshot_now, flow_window);
    let avg_hold_secs = engine
        .state
        .portfolio
        .avg_hold_secs(snapshot_now, flow_window);
    let recent_closed_pnl = engine
        .state
        .portfolio
        .recent_closed_pnl(snapshot_now, flow_window);

    WorldSnapshot {
        mode: engine.state.run.mode,
        markets,
        journal_tail,
        cash: engine.state.portfolio.cash,
        equity,
        realized_pnl: engine.state.portfolio.realized_pnl,
        unrealized_pnl,
        open_positions,
        kill_switch: engine.state.portfolio.kill_switch,
        stream_splits: engine.state.portfolio.stream_splits,
        ref_symbol,
        ref_spot,
        signal_strength,
        realized_pnl_5m: engine.realized_pnl_5m(snapshot_now),
        pnl_shortfall_5m: engine.pnl_shortfall_5m(snapshot_now),
        flow_pnl_per_min,
        cycle_rate_per_min,
        avg_hold_secs,
        recent_closed_pnl,
        eligible_markets,
        ticket_dollars: sweep.ticket_dollars,
        min_entry_threshold: sweep.min_entry_price,
        entry_threshold: sweep.max_entry_price,
        no_new_entry_expiry_secs: sweep.no_new_entry_expiry_secs,
        exit_threshold: sweep.take_profit_price,
        min_exit_roi: sweep.min_exit_roi,
        max_tick_frac: sweep.max_tick_frac,
        max_spread: sweep.max_spread,
        paper_real_mode: sweep.paper_real_mode,
        backtest: None,
    }
}
