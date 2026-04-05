use super::quote::decimal_to_f64;
use super::types::{L2Level, OrderBook};
use std::time::Instant;

pub fn apply_snapshot(book: &mut OrderBook, bids: Vec<L2Level>, asks: Vec<L2Level>, ts: Instant) {
    book.bids = norm_bids(bids);
    book.asks = norm_asks(asks);
    book.last_update = ts;
}

pub fn apply_delta(book: &mut OrderBook, bids: Vec<L2Level>, asks: Vec<L2Level>, ts: Instant) {
    apply_snapshot(book, bids, asks, ts);
}

pub fn best_bid(book: &OrderBook) -> f64 {
    book.bids
        .first()
        .map(|l| decimal_to_f64(l.price))
        .unwrap_or(0.0)
}

pub fn best_ask(book: &OrderBook) -> f64 {
    book.asks
        .first()
        .map(|l| decimal_to_f64(l.price))
        .unwrap_or(0.0)
}

pub fn spread_ticks(book: &OrderBook, tick_size: f64) -> f64 {
    let bid = best_bid(book);
    let ask = best_ask(book);
    if bid <= 0.0 || ask <= 0.0 || ask < bid {
        return 0.0;
    }
    (ask - bid) / tick_size.max(1e-9)
}

pub fn microprice(book: &OrderBook) -> f64 {
    match (book.bids.first(), book.asks.first()) {
        (Some(bid), Some(ask)) => {
            let bid_px = decimal_to_f64(bid.price);
            let ask_px = decimal_to_f64(ask.price);
            let bid_sz = decimal_to_f64(bid.size);
            let ask_sz = decimal_to_f64(ask.size);
            let denom = bid_sz + ask_sz;
            if denom <= 0.0 {
                (bid_px + ask_px) / 2.0
            } else {
                ((ask_px * bid_sz) + (bid_px * ask_sz)) / denom
            }
        }
        _ => 0.0,
    }
}

pub fn imbalance(levels: &[L2Level]) -> f64 {
    levels.iter().map(|l| decimal_to_f64(l.size)).sum()
}

pub fn top_imbalance(book: &OrderBook) -> f64 {
    let bid = book
        .bids
        .first()
        .map(|l| decimal_to_f64(l.size))
        .unwrap_or(0.0);
    let ask = book
        .asks
        .first()
        .map(|l| decimal_to_f64(l.size))
        .unwrap_or(0.0);
    signed_ratio(bid, ask)
}

pub fn five_level_imbalance(book: &OrderBook) -> f64 {
    let bid = imbalance(&book.bids.iter().take(5).cloned().collect::<Vec<_>>());
    let ask = imbalance(&book.asks.iter().take(5).cloned().collect::<Vec<_>>());
    signed_ratio(bid, ask)
}

pub fn depth_slope(levels: &[L2Level]) -> f64 {
    if levels.len() < 2 {
        return 0.0;
    }

    let n = levels.len() as f64;
    let mean_x = (n - 1.0) / 2.0;
    let mean_y = levels.iter().map(|l| decimal_to_f64(l.size)).sum::<f64>() / n;

    let mut numer = 0.0;
    let mut denom = 0.0;
    for (idx, level) in levels.iter().enumerate() {
        let x = idx as f64;
        let y = decimal_to_f64(level.size);
        numer += (x - mean_x) * (y - mean_y);
        denom += (x - mean_x).powi(2);
    }

    if denom <= 0.0 {
        0.0
    } else {
        numer / denom
    }
}

pub fn liquidity_void_score(book: &OrderBook) -> f64 {
    let visible_depth = imbalance(&book.bids.iter().take(5).cloned().collect::<Vec<_>>())
        + imbalance(&book.asks.iter().take(5).cloned().collect::<Vec<_>>());
    (1.0 / (1.0 + visible_depth)).clamp(0.0, 1.0)
}

fn signed_ratio(bid: f64, ask: f64) -> f64 {
    let total = bid + ask;
    if total <= 0.0 {
        0.0
    } else {
        ((bid - ask) / total).clamp(-1.0, 1.0)
    }
}

fn norm_bids(mut levels: Vec<L2Level>) -> Vec<L2Level> {
    levels.retain(|l| l.size > 0.into());
    levels.sort_by(|a, b| b.price.cmp(&a.price));
    levels
}

fn norm_asks(mut levels: Vec<L2Level>) -> Vec<L2Level> {
    levels.retain(|l| l.size > 0.into());
    levels.sort_by(|a, b| a.price.cmp(&b.price));
    levels
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn snapshot_normalizes_book_sides() {
        let mut book = OrderBook::default();
        let bids = vec![
            L2Level {
                price: dec!(0.94),
                size: dec!(2),
            },
            L2Level {
                price: dec!(0.96),
                size: dec!(1),
            },
            L2Level {
                price: dec!(0.95),
                size: dec!(0),
            },
        ];
        let asks = vec![
            L2Level {
                price: dec!(0.99),
                size: dec!(4),
            },
            L2Level {
                price: dec!(0.97),
                size: dec!(3),
            },
            L2Level {
                price: dec!(0.98),
                size: dec!(0),
            },
        ];

        apply_snapshot(&mut book, bids, asks, Instant::now());

        assert_eq!(best_bid(&book), 0.96);
        assert_eq!(best_ask(&book), 0.97);
        assert_eq!(book.bids.len(), 2);
        assert_eq!(book.asks.len(), 2);
    }
}
