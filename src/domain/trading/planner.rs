use super::snapshot::MarketSnapshot;

/// Check whether the microprice + edge signal supports entry.
pub fn entry_signal_ok(best_bid: f64, best_ask: f64, micro: f64, edge_buy: f64) -> bool {
    if best_ask <= 0.0 {
        return false;
    }
    if edge_buy > 0.005 {
        return true;
    }
    if micro <= 0.0 {
        return false;
    }
    micro >= (best_ask - 0.0025) && micro > best_bid
}

/// Compute the maker exit target price for an existing position.
pub fn maker_exit_target(avg_entry: f64, base_take_profit: f64) -> f64 {
    (avg_entry + 0.01).clamp(base_take_profit, 0.99)
}

/// Sorting key for market display/priority — positions first, then cheapest asks.
pub fn market_sort_key(market: &MarketSnapshot, entry_threshold: f64) -> f64 {
    if market.position_qty > 0.0 {
        10_000.0 - market.avg_entry
    } else if crate::domain::market::quote::valid_live_quote(market.best_bid, market.best_ask)
        && market.best_ask <= entry_threshold
    {
        1_000.0 - market.best_ask
    } else if crate::domain::market::quote::valid_live_quote(market.best_bid, market.best_ask) {
        100.0 - market.best_ask
    } else {
        -999.0
    }
}
