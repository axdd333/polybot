use super::book;
use super::types::MarketState;

#[derive(Debug, Clone, Default)]
pub struct FeatureVector {
    pub ret_z_1s: f64,
    pub ret_z_5s: f64,
    pub accel: f64,
    pub vol_short: f64,
    pub spread_ticks: f64,
    pub microprice_gap: f64,
    pub imbalance_top: f64,
    pub imbalance_5lvl: f64,
    pub depth_slope: f64,
    pub cancel_skew: f64,
    pub trade_intensity: f64,
    pub burstiness: f64,
    pub cross_window_torsion: f64,
    pub liquidity_void_score: f64,
    pub wall_persistence_score: f64,
    pub expiry_pressure: f64,
}

pub fn compute(market: &MarketState) -> FeatureVector {
    let bid = book::best_bid(&market.book);
    let ask = book::best_ask(&market.book);
    let mid = if bid > 0.0 && ask > 0.0 {
        (bid + ask) / 2.0
    } else {
        0.0
    };
    let micro_gap = if mid > 0.0 {
        ((market.microprice - mid) / mid).clamp(-1.0, 1.0)
    } else {
        0.0
    };
    let depth_slope = (market.depth_slope_bid - market.depth_slope_ask).clamp(-10.0, 10.0);
    let cancel_skew = (market.cancel_rate_bid - market.cancel_rate_ask).clamp(-5.0, 5.0);
    let expiry_secs = market.time_to_expiry.as_secs_f64();
    let expiry_pressure = (1.0 / (1.0 + expiry_secs / 30.0)).clamp(0.0, 1.0);

    FeatureVector {
        ret_z_1s: zscore(market.underlying.ret_1s, market.underlying.vol_5s),
        ret_z_5s: zscore(market.underlying.ret_5s, market.underlying.vol_15s),
        accel: market.underlying.accel,
        vol_short: market.underlying.vol_5s,
        spread_ticks: market.spread_ticks,
        microprice_gap: micro_gap,
        imbalance_top: market.imbalance_top,
        imbalance_5lvl: market.imbalance_5lvl,
        depth_slope,
        cancel_skew,
        trade_intensity: market.trade_intensity,
        burstiness: market.burstiness,
        cross_window_torsion: market.cross_window_torsion,
        liquidity_void_score: market.liquidity_void_score,
        wall_persistence_score: market.wall_persistence_score,
        expiry_pressure,
    }
}

fn zscore(ret: f64, vol: f64) -> f64 {
    if vol <= 1e-9 {
        0.0
    } else {
        (ret / vol).clamp(-5.0, 5.0)
    }
}
