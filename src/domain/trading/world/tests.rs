use super::*;
use crate::domain::analyzer;
use crate::domain::market::types::{L2Level, MarketId, OrderAction, Side};
use crate::domain::trading::planner::entry_signal_ok;
use rust_decimal_macros::dec;
use std::time::Duration;

#[test]
fn entry_signal_allows_micro_near_ask() {
    assert!(entry_signal_ok(0.49, 0.50, 0.499, 0.0));
}

#[test]
fn entry_signal_allows_positive_edge_even_without_micro_push() {
    assert!(entry_signal_ok(0.49, 0.50, 0.495, 0.01));
}

#[test]
fn scaled_target_qty_grows_with_stream_splits() {
    assert_eq!(scaled_target_qty(10.0, 100.0, 0), 10.0);
    assert_eq!(scaled_target_qty(10.0, 100.0, 50), 15.0);
}

#[test]
fn scaled_target_qty_respects_inventory_limit() {
    assert_eq!(scaled_target_qty(10.0, 12.0, 50), 12.0);
}

#[test]
fn liquidation_mark_prefers_bid_for_long_inventory() {
    let mut world = World::new();
    let market_id = MarketId(42);
    world.seed_market(
        market_id,
        "BTC Window",
        "BTC",
        "12:00-12:05",
        "12:05",
        Side::Up,
        Duration::from_secs(300),
    );
    let tracked = world.markets.get_mut(&market_id).unwrap();
    tracked.state.book.bids.push(L2Level {
        price: dec!(0.38),
        size: dec!(10),
    });
    tracked.state.book.asks.push(L2Level {
        price: dec!(0.44),
        size: dec!(10),
    });

    assert_eq!(analyzer::liquidation_mark(&tracked.state.book, 0.50), 0.38);
}

#[test]
fn sweep_entry_skips_negative_edge_even_if_price_is_cheap() {
    let mut world = World::new();
    let market_id = MarketId(7);
    world.seed_market(
        market_id,
        "BTC Window",
        "BTC",
        "12:00-12:05",
        "12:05",
        Side::Up,
        Duration::from_secs(300),
    );
    let tracked = world.markets.get_mut(&market_id).unwrap();
    tracked.state.edge_buy = -0.20;
    tracked.state.book.bids.push(L2Level {
        price: dec!(0.94),
        size: dec!(10),
    });
    tracked.state.book.asks.push(L2Level {
        price: dec!(0.95),
        size: dec!(10),
    });

    world.plan_orders(market_id);
    assert!(world
        .markets
        .get(&market_id)
        .map(|tracked| tracked.planned_surface.intents.is_empty())
        .unwrap_or(true));
}

#[test]
fn sweep_entry_skips_prices_above_threshold() {
    let mut world = World::new();
    let market_id = MarketId(8);
    world.seed_market(
        market_id,
        "BTC Window",
        "BTC",
        "12:05-12:10",
        "12:10",
        Side::Down,
        Duration::from_secs(300),
    );
    let tracked = world.markets.get_mut(&market_id).unwrap();
    tracked.state.book.bids.push(L2Level {
        price: dec!(0.98),
        size: dec!(10),
    });
    tracked.state.book.asks.push(L2Level {
        price: dec!(0.991),
        size: dec!(10),
    });

    world.plan_orders(market_id);
    assert!(world
        .markets
        .get(&market_id)
        .map(|tracked| tracked.planned_surface.intents.is_empty())
        .unwrap_or(true));
}

#[test]
fn sweep_entry_allows_prices_above_ninety_five_cents() {
    let mut world = World::new();
    let market_id = MarketId(10);
    world.seed_market(
        market_id,
        "BTC Window",
        "BTC",
        "12:15-12:20",
        "12:20",
        Side::Down,
        Duration::from_secs(300),
    );
    let tracked = world.markets.get_mut(&market_id).unwrap();
    tracked.state.edge_buy = 0.03;
    tracked.state.book.bids.push(L2Level {
        price: dec!(0.96),
        size: dec!(10),
    });
    tracked.state.book.asks.push(L2Level {
        price: dec!(0.97),
        size: dec!(10),
    });

    world.plan_orders(market_id);
    let surface = &world.markets.get(&market_id).unwrap().planned_surface;
    assert_eq!(surface.intents.len(), 1);
    assert_eq!(surface.intents[0].action, OrderAction::Buy);
    assert_eq!(surface.intents[0].price, 0.97);
}

#[test]
fn sweep_skips_toxic_pair_when_both_sides_are_expensive() {
    let mut world = World::new();
    let up_id = MarketId(11);
    let down_id = MarketId(12);
    world.seed_market(
        up_id,
        "BTC Window",
        "BTC",
        "12:20-12:25",
        "12:25",
        Side::Up,
        Duration::from_secs(300),
    );
    world.seed_market(
        down_id,
        "BTC Window",
        "BTC",
        "12:20-12:25",
        "12:25",
        Side::Down,
        Duration::from_secs(300),
    );
    {
        let tracked = world.markets.get_mut(&up_id).unwrap();
        tracked.state.edge_buy = 0.01;
        tracked.state.book.bids.push(L2Level {
            price: dec!(0.98),
            size: dec!(10),
        });
        tracked.state.book.asks.push(L2Level {
            price: dec!(0.99),
            size: dec!(10),
        });
    }
    {
        let tracked = world.markets.get_mut(&down_id).unwrap();
        tracked.state.edge_buy = 0.02;
        tracked.state.book.bids.push(L2Level {
            price: dec!(0.98),
            size: dec!(10),
        });
        tracked.state.book.asks.push(L2Level {
            price: dec!(0.99),
            size: dec!(10),
        });
    }

    world.plan_orders(up_id);
    world.plan_orders(down_id);

    assert!(world
        .markets
        .get(&up_id)
        .unwrap()
        .planned_surface
        .intents
        .is_empty());
    assert!(world
        .markets
        .get(&down_id)
        .unwrap()
        .planned_surface
        .intents
        .is_empty());
}

#[test]
fn sweep_only_buys_preferred_side_per_pair() {
    let mut world = World::new();
    let up_id = MarketId(13);
    let down_id = MarketId(14);
    world.seed_market(
        up_id,
        "BTC Window",
        "BTC",
        "12:25-12:30",
        "12:30",
        Side::Up,
        Duration::from_secs(300),
    );
    world.seed_market(
        down_id,
        "BTC Window",
        "BTC",
        "12:25-12:30",
        "12:30",
        Side::Down,
        Duration::from_secs(300),
    );
    {
        let tracked = world.markets.get_mut(&up_id).unwrap();
        tracked.state.edge_buy = 0.03;
        tracked.state.book.bids.push(L2Level {
            price: dec!(0.96),
            size: dec!(10),
        });
        tracked.state.book.asks.push(L2Level {
            price: dec!(0.97),
            size: dec!(10),
        });
    }
    {
        let tracked = world.markets.get_mut(&down_id).unwrap();
        tracked.state.edge_buy = -0.01;
        tracked.state.book.bids.push(L2Level {
            price: dec!(0.02),
            size: dec!(10),
        });
        tracked.state.book.asks.push(L2Level {
            price: dec!(0.03),
            size: dec!(10),
        });
    }

    world.plan_orders(up_id);
    world.plan_orders(down_id);

    assert_eq!(
        world
            .markets
            .get(&up_id)
            .unwrap()
            .planned_surface
            .intents
            .len(),
        1
    );
    assert!(world
        .markets
        .get(&down_id)
        .unwrap()
        .planned_surface
        .intents
        .is_empty());
}

#[test]
fn sweep_skips_wide_spread_even_with_positive_edge() {
    let mut world = World::new();
    let market_id = MarketId(15);
    world.seed_market(
        market_id,
        "BTC Window",
        "BTC",
        "12:30-12:35",
        "12:35",
        Side::Up,
        Duration::from_secs(300),
    );
    let tracked = world.markets.get_mut(&market_id).unwrap();
    tracked.state.edge_buy = 0.02;
    tracked.state.book.bids.push(L2Level {
        price: dec!(0.01),
        size: dec!(10),
    });
    tracked.state.book.asks.push(L2Level {
        price: dec!(0.99),
        size: dec!(10),
    });

    world.plan_orders(market_id);

    assert!(world
        .markets
        .get(&market_id)
        .unwrap()
        .planned_surface
        .intents
        .is_empty());
}

#[tokio::test]
async fn sweep_exit_sells_full_position_at_ninety_eight_cents() {
    let mut world = World::new();
    let market_id = MarketId(9);
    world.seed_market(
        market_id,
        "BTC Window",
        "BTC",
        "12:10-12:15",
        "12:15",
        Side::Up,
        Duration::from_secs(300),
    );
    {
        let tracked = world.markets.get_mut(&market_id).unwrap();
        tracked.state.edge_buy = 0.03;
        tracked.state.book.bids.push(L2Level {
            price: dec!(0.94),
            size: dec!(10),
        });
        tracked.state.book.asks.push(L2Level {
            price: dec!(0.95),
            size: dec!(10),
        });
    }

    world.plan_orders(market_id);
    world.apply_risk(market_id);
    world.execute(market_id).await;

    {
        let tracked = world.markets.get_mut(&market_id).unwrap();
        tracked.state.book.bids.clear();
        tracked.state.book.asks.clear();
        tracked.state.book.bids.push(L2Level {
            price: dec!(0.98),
            size: dec!(10),
        });
        tracked.state.book.asks.push(L2Level {
            price: dec!(0.99),
            size: dec!(10),
        });
    }

    world.plan_orders(market_id);
    let surface = &world.markets.get(&market_id).unwrap().planned_surface;
    assert_eq!(surface.intents.len(), 1);
    assert_eq!(surface.intents[0].action, OrderAction::Sell);

    world.apply_risk(market_id);
    world.execute(market_id).await;

    assert!(world.portfolio.position(market_id).is_none());
    assert!(world.portfolio.realized_pnl > 0.0);
}
