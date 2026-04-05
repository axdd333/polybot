use super::features::FeatureVector;
use super::types::{Regime, Side};
use crate::config::ModelWeights;

pub fn score(f: &FeatureVector, w: &ModelWeights) -> f64 {
    w.ret_z_1s * signed_scale(f.ret_z_1s, 2.5)
        + w.accel * signed_scale(f.accel, f.vol_short.max(0.0005) * 4.0)
        + w.microprice_gap * signed_scale(f.microprice_gap, 0.01)
        + w.imbalance_5lvl * f.imbalance_5lvl.clamp(-1.0, 1.0)
        + w.trade_intensity * unit_scale(f.trade_intensity, 8.0)
        + w.cross_window_torsion * signed_scale(f.cross_window_torsion, 1.0)
        + w.wall_persistence_score * unit_scale(f.wall_persistence_score, 5.0)
        + w.vol_short * unit_scale(f.vol_short, 0.01)
        + w.spread_ticks * unit_scale(f.spread_ticks, 5.0)
        + w.liquidity_void_score * f.liquidity_void_score.clamp(0.0, 1.0)
}

pub fn classify_regime(f: &FeatureVector) -> Regime {
    if f.expiry_pressure > 0.85 {
        return Regime::ExpiryPinch;
    }
    if f.accel.abs() > 2.0 && f.trade_intensity > 1.5 {
        return Regime::Burst;
    }
    if f.ret_z_1s.signum() == f.microprice_gap.signum() && f.imbalance_5lvl.abs() > 0.25 {
        return Regime::Continuation;
    }
    if f.ret_z_1s.signum() != f.microprice_gap.signum() && f.vol_short < 1.2 {
        return Regime::Reversion;
    }
    Regime::Chop
}

pub fn fair_value_prob(score: f64, scale: f64) -> f64 {
    1.0 / (1.0 + (-(score / scale.max(0.1))).exp())
}

pub fn fair_value_for_side(side: Side, score: f64, scale: f64) -> f64 {
    let base = fair_value_prob(score, scale).clamp(0.005, 0.995);
    match side {
        Side::Up => base,
        Side::Down => 1.0 - base,
    }
}

pub fn anchored_fair_value(
    side: Side,
    score: f64,
    bid: f64,
    ask: f64,
    expiry_pressure: f64,
    w: &ModelWeights,
) -> f64 {
    let raw = fair_value_for_side(side, score, w.score_scale);
    if bid <= 0.0 || ask <= 0.0 || ask < bid {
        return raw;
    }
    let mid = (bid + ask) / 2.0;
    let spread = (ask - bid).max(0.0);
    let band =
        (w.fair_base_band + spread * w.fair_spread_mult + expiry_pressure * w.fair_base_band)
            .clamp(w.fair_base_band, w.fair_max_band.max(w.fair_base_band));
    let bias = ((raw - 0.5) * 2.0).clamp(-1.0, 1.0);
    (mid + bias * band).clamp(0.005, 0.995)
}

pub fn edge_to_buy(fair: f64, best_ask: f64) -> f64 {
    fair - best_ask
}

pub fn edge_to_sell(fair: f64, best_bid: f64) -> f64 {
    best_bid - fair
}

fn signed_scale(value: f64, scale: f64) -> f64 {
    (value / scale.max(1e-6)).clamp(-1.0, 1.0)
}

fn unit_scale(value: f64, scale: f64) -> f64 {
    (value / scale.max(1e-6)).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ModelWeights;

    #[test]
    fn sigmoid_stays_bounded() {
        assert!(fair_value_prob(10.0, 2.5) < 1.0);
        assert!(fair_value_prob(-10.0, 2.5) > 0.0);
    }

    #[test]
    fn expiry_pinch_takes_precedence() {
        let f = FeatureVector {
            expiry_pressure: 0.9,
            ..FeatureVector::default()
        };
        assert!(matches!(classify_regime(&f), Regime::ExpiryPinch));
    }

    #[test]
    fn score_uses_weights() {
        let w = ModelWeights::default();
        let f = FeatureVector {
            ret_z_1s: 1.0,
            ..FeatureVector::default()
        };
        let s = score(&f, &w);
        assert!(s > 0.0);
    }

    #[test]
    fn anchored_fair_stays_near_book() {
        let w = ModelWeights::default();
        let fair = anchored_fair_value(Side::Up, 9.0, 0.01, 0.02, 0.05, &w);
        assert!(fair < 0.07);
    }

    #[test]
    fn anchored_fair_moves_with_book_context() {
        let w = ModelWeights::default();
        let fair = anchored_fair_value(Side::Up, 0.8, 0.96, 0.97, 0.05, &w);
        assert!(fair > 0.965);
    }
}
