use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;

pub fn decimal_to_f64(value: Decimal) -> f64 {
    value.to_f64().unwrap_or(0.0)
}

pub fn valid_live_quote(best_bid: f64, best_ask: f64) -> bool {
    best_ask > 0.0 && (best_bid <= 0.0 || best_ask >= best_bid)
}

pub fn has_tight_spread(best_bid: f64, best_ask: f64, max_spread: f64) -> bool {
    valid_live_quote(best_bid, best_ask) && best_bid > 0.0 && (best_ask - best_bid) <= max_spread
}

pub fn spread_cents_label(best_bid: f64, best_ask: f64) -> String {
    if !valid_live_quote(best_bid, best_ask) || best_bid <= 0.0 {
        "n/a".to_string()
    } else {
        format!("{:.1}c", (best_ask - best_bid) * 100.0)
    }
}

pub fn cents_label(value: f64) -> String {
    format!("{:.0}c", (value * 100.0).clamp(0.0, 100.0))
}

pub fn cents_or_dash(value: f64) -> String {
    if value > 0.0 {
        cents_label(value)
    } else {
        "--".to_string()
    }
}
