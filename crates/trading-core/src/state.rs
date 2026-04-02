use crate::analyzer::{MarketRuntime, UnderlyingPoint};
use crate::config::RunMode;
use crate::market::features::FeatureVector;
use crate::market::types::{InstrumentId, MarketId, MarketState, OrderAction, OrderSurface, Side};
use crate::portfolio::Portfolio;
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct TrackedMarket {
    pub instrument_id: InstrumentId,
    pub condition_id: String,
    pub token_id: String,
    pub state: MarketState,
    pub runtime: MarketRuntime,
    pub features: Option<FeatureVector>,
    pub planned_surface: OrderSurface,
}

impl TrackedMarket {
    pub fn new(
        instrument_id: InstrumentId,
        condition_id: String,
        token_id: String,
        side: Side,
        time_to_expiry: Duration,
    ) -> Self {
        Self {
            instrument_id: instrument_id.clone(),
            condition_id,
            token_id,
            state: MarketState::new(instrument_id.market, side, time_to_expiry),
            runtime: MarketRuntime::default(),
            features: None,
            planned_surface: OrderSurface::default(),
        }
    }

    pub fn placeholder(market_id: MarketId) -> Self {
        Self::new(
            InstrumentId::placeholder(market_id),
            format!("condition:placeholder:{}", market_id.0),
            format!("token:placeholder:{}", market_id.0),
            Side::Up,
            Duration::from_secs(300),
        )
    }
}

#[derive(Debug, Default)]
pub struct MarketStore {
    pub markets: HashMap<MarketId, TrackedMarket>,
    pub dirty: HashSet<MarketId>,
}

#[derive(Debug, Default)]
pub struct UnderlyingStore {
    pub tapes: HashMap<String, VecDeque<UnderlyingPoint>>,
}

#[derive(Debug)]
pub struct RunState {
    pub mode: RunMode,
    pub journal: Vec<String>,
    pub live_orders: HashMap<MarketId, PendingLiveOrder>,
}

impl RunState {
    pub fn new(mode: RunMode) -> Self {
        Self {
            mode,
            journal: Vec::new(),
            live_orders: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PendingLiveOrder {
    pub order_id: String,
    pub action: OrderAction,
    pub price: f64,
    pub qty: f64,
    pub size_matched: f64,
}

#[derive(Debug)]
pub struct EngineState {
    pub markets: MarketStore,
    pub underlyings: UnderlyingStore,
    pub portfolio: Portfolio,
    pub run: RunState,
}

impl EngineState {
    pub fn new(mode: RunMode, starting_cash: f64) -> Self {
        Self {
            markets: MarketStore::default(),
            underlyings: UnderlyingStore::default(),
            portfolio: Portfolio::with_starting_cash(starting_cash),
            run: RunState::new(mode),
        }
    }
}
