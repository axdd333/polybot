use std::env;

#[derive(Clone, Debug)]
pub struct SweepStrategyConfig {
    pub starting_cash: f64,
    pub ticket_dollars: f64,
    pub max_entry_price: f64,
    pub take_profit_price: f64,
    pub min_edge_to_buy: f64,
    pub max_spread: f64,
    pub max_pair_ask_sum: f64,
    pub paper_real_mode: bool,
}

impl Default for SweepStrategyConfig {
    fn default() -> Self {
        Self {
            starting_cash: 100.0,
            ticket_dollars: 2.0,
            max_entry_price: 0.99,
            take_profit_price: 0.98,
            min_edge_to_buy: 0.01,
            max_spread: 0.03,
            max_pair_ask_sum: 1.02,
            paper_real_mode: true,
        }
    }
}

impl SweepStrategyConfig {
    pub fn from_env() -> Self {
        let default = Self::default();
        let config = Self {
            starting_cash: env_f64("SWEEP_STARTING_CASH", default.starting_cash),
            ticket_dollars: env_f64("SWEEP_TICKET_DOLLARS", default.ticket_dollars),
            max_entry_price: env_f64("SWEEP_MAX_ENTRY_PRICE", default.max_entry_price),
            take_profit_price: env_f64("SWEEP_TAKE_PROFIT_PRICE", default.take_profit_price),
            min_edge_to_buy: env_f64(
                "SWEEP_MIN_EDGE_TO_BUY",
                env_f64("MIN_EDGE_TO_JOIN", default.min_edge_to_buy),
            ),
            max_spread: env_f64(
                "SWEEP_MAX_SPREAD",
                env_f64("MAX_SPREAD_TICKS", default.max_spread / 0.01) * 0.01,
            ),
            max_pair_ask_sum: env_f64("SWEEP_MAX_PAIR_ASK_SUM", default.max_pair_ask_sum),
            paper_real_mode: env_bool(
                "SWEEP_PAPER_REAL_MODE",
                env_bool("PAPER_MODE", default.paper_real_mode),
            ),
        };
        config.validate();
        config
    }

    fn validate(&self) {
        assert!(self.starting_cash > 0.0, "starting_cash must be positive");
        assert!(self.ticket_dollars > 0.0, "ticket_dollars must be positive");
        assert!(
            self.max_entry_price > 0.0 && self.max_entry_price <= 1.0,
            "max_entry_price must be in (0, 1]"
        );
        assert!(
            self.take_profit_price > 0.0 && self.take_profit_price <= 1.0,
            "take_profit_price must be in (0, 1]"
        );
        assert!(
            self.max_spread > 0.0 && self.max_spread <= 1.0,
            "max_spread must be in (0, 1]"
        );
        assert!(
            self.max_pair_ask_sum > 0.0,
            "max_pair_ask_sum must be positive"
        );
    }
}

/// Weights for the linear scoring model used in regime classification and fair value.
/// All values are applied to the corresponding field of `FeatureVector` in `model::score()`.
#[derive(Clone, Debug)]
pub struct ModelWeights {
    pub ret_z_1s: f64,
    pub accel: f64,
    pub microprice_gap: f64,
    pub imbalance_5lvl: f64,
    pub trade_intensity: f64,
    pub cross_window_torsion: f64,
    pub wall_persistence_score: f64,
    pub vol_short: f64,
    pub spread_ticks: f64,
    pub liquidity_void_score: f64,
}

impl Default for ModelWeights {
    fn default() -> Self {
        Self {
            ret_z_1s: 0.28,
            accel: 0.16,
            microprice_gap: 0.18,
            imbalance_5lvl: 0.14,
            trade_intensity: 0.10,
            cross_window_torsion: 0.12,
            wall_persistence_score: 0.10,
            vol_short: -0.14,
            spread_ticks: -0.12,
            liquidity_void_score: -0.10,
        }
    }
}

fn env_f64(key: &str, default: f64) -> f64 {
    env::var(key)
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(default)
}

fn env_bool(key: &str, default: bool) -> bool {
    env::var(key)
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(default)
}
