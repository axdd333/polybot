use super::features::FeatureVector;
use super::types::{Regime, Side};
use crate::domain::config::ModelWeights;

pub fn score(f: &FeatureVector, w: &ModelWeights) -> f64 {
    w.ret_z_1s * f.ret_z_1s
        + w.accel * f.accel
        + w.microprice_gap * f.microprice_gap
        + w.imbalance_5lvl * f.imbalance_5lvl
        + w.trade_intensity * f.trade_intensity
        + w.cross_window_torsion * f.cross_window_torsion
        + w.wall_persistence_score * f.wall_persistence_score
        + w.vol_short * f.vol_short
        + w.spread_ticks * f.spread_ticks
        + w.liquidity_void_score * f.liquidity_void_score
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

pub fn fair_value_prob(score: f64) -> f64 {
    1.0 / (1.0 + (-score).exp())
}

pub fn fair_value_for_side(side: Side, score: f64) -> f64 {
    let base = fair_value_prob(score).clamp(0.005, 0.995);
    match side {
        Side::Up => base,
        Side::Down => 1.0 - base,
    }
}

pub fn edge_to_buy(fair: f64, best_ask: f64) -> f64 {
    fair - best_ask
}

pub fn edge_to_sell(fair: f64, best_bid: f64) -> f64 {
    best_bid - fair
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::config::ModelWeights;

    #[test]
    fn sigmoid_stays_bounded() {
        assert!(fair_value_prob(10.0) < 1.0);
        assert!(fair_value_prob(-10.0) > 0.0);
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
        assert!((s - w.ret_z_1s).abs() < 1e-9);
    }
}
