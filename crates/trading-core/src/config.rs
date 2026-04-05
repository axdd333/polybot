use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    #[default]
    Live,
    Replay,
    Backtest,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    #[default]
    Paper,
    Live,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WalletSignatureType {
    #[default]
    Eoa,
    Proxy,
    GnosisSafe,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetConfig {
    pub name: String,
    pub oracle: Option<String>,
    #[serde(default)]
    pub rtds_symbol: String,
    pub slug_prefixes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdapterProfile {
    pub assets: Vec<AssetConfig>,
    pub clob_ws_enabled: bool,
    pub rtds_enabled: bool,
    pub chainlink_fallback_enabled: bool,
    pub universe_refresh_secs: u64,
    pub polygon_rpc_url: String,
    #[serde(default = "default_allowed_window_mins")]
    pub allowed_window_mins: Vec<u64>,
    #[serde(default = "default_max_markets")]
    pub max_markets: usize,
    pub max_future_secs: Option<u64>,
}

impl Default for AdapterProfile {
    fn default() -> Self {
        Self {
            assets: vec![
                AssetConfig {
                    name: "BTC".to_string(),
                    oracle: Some("0xc907E116054Ad103354f2D350FD2514433D57F6f".to_string()),
                    rtds_symbol: "btcusdt".to_string(),
                    slug_prefixes: vec!["bitcoin".to_string(), "btc".to_string()],
                },
                AssetConfig {
                    name: "ETH".to_string(),
                    oracle: None,
                    rtds_symbol: "ethusdt".to_string(),
                    slug_prefixes: vec!["ethereum".to_string(), "eth".to_string()],
                },
                AssetConfig {
                    name: "SOL".to_string(),
                    oracle: None,
                    rtds_symbol: "solusdt".to_string(),
                    slug_prefixes: vec!["solana".to_string(), "sol".to_string()],
                },
                AssetConfig {
                    name: "DOGE".to_string(),
                    oracle: None,
                    rtds_symbol: "dogeusdt".to_string(),
                    slug_prefixes: vec!["dogecoin".to_string(), "doge".to_string()],
                },
            ],
            clob_ws_enabled: true,
            rtds_enabled: true,
            chainlink_fallback_enabled: true,
            universe_refresh_secs: 20,
            polygon_rpc_url: "https://polygon.drpc.org".to_string(),
            allowed_window_mins: default_allowed_window_mins(),
            max_markets: default_max_markets(),
            max_future_secs: None,
        }
    }
}

fn default_allowed_window_mins() -> Vec<u64> {
    vec![5, 15]
}

fn default_max_markets() -> usize {
    96
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveProfile {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub clob_host: String,
    #[serde(default)]
    pub ws_host: String,
    #[serde(default)]
    pub private_key_env: String,
    #[serde(default)]
    pub signature_type: WalletSignatureType,
    pub funder: Option<String>,
}

impl Default for LiveProfile {
    fn default() -> Self {
        Self {
            enabled: false,
            clob_host: "https://clob.polymarket.com/".to_string(),
            ws_host: "wss://ws-subscriptions-clob.polymarket.com".to_string(),
            private_key_env: "POLYMARKET_PRIVATE_KEY".to_string(),
            signature_type: WalletSignatureType::Eoa,
            funder: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionProfile {
    #[serde(default)]
    pub mode: ExecutionMode,
    #[serde(default)]
    pub paper: PaperProfile,
    #[serde(default)]
    pub live: LiveProfile,
}

impl Default for ExecutionProfile {
    fn default() -> Self {
        Self {
            mode: ExecutionMode::Paper,
            paper: PaperProfile::default(),
            live: LiveProfile::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaperProfile {
    pub latency_ms: u64,
    pub stale_after_ms: u64,
    pub rest_queue_mult: f64,
}

impl Default for PaperProfile {
    fn default() -> Self {
        Self {
            latency_ms: 90,
            stale_after_ms: 1_500,
            rest_queue_mult: 1.05,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SweepProfile {
    pub starting_cash: f64,
    pub ticket_dollars: f64,
    pub min_entry_price: f64,
    pub max_entry_price: f64,
    pub max_tick_frac: f64,
    pub take_profit_price: f64,
    pub min_exit_roi: f64,
    #[serde(default = "default_hold_green_secs")]
    pub hold_green_secs: u64,
    pub min_edge_to_buy: f64,
    pub max_spread: f64,
    pub max_pair_ask_sum: f64,
    pub max_open_positions: usize,
    pub paper_real_mode: bool,
    pub min_model_score: f64,
    pub min_fair_gap_after_cost: f64,
    pub min_net_edge_buy: f64,
    pub min_net_edge_sell: f64,
    pub entry_slippage_bps: f64,
    pub exit_slippage_bps: f64,
    pub no_new_entry_expiry_secs: u64,
    pub reduce_size_expiry_secs: u64,
    pub max_hold_secs: u64,
    pub max_stale_fair_secs: u64,
    pub min_fair_improve: f64,
    pub scratch_edge: f64,
    pub trailing_drawdown_frac: f64,
    pub max_void_score: f64,
    pub min_wall_score: f64,
    pub max_cancel_skew: f64,
    pub min_bid_size: f64,
    pub min_visible_depth: f64,
    pub min_queue_fill_score: f64,
    pub min_queue_trade_intensity: f64,
    pub min_opportunity_score: f64,
    pub aggressive_entry_score: f64,
    pub winner_hold_edge: f64,
    pub winner_hold_wall_score: f64,
    pub flow_target_per_min: f64,
    pub flow_window_secs: u64,
    pub min_cycle_rate_per_min: f64,
    pub maker_band_dist: f64,
    pub flow_reversion_edge: f64,
    pub flow_exit_edge: f64,
    pub flow_size_mult: f64,
    pub flow_max_hold_secs: u64,
    #[serde(default)]
    pub impulse: ImpulseProfile,
    #[serde(default)]
    pub reversal: ReversalProfile,
    pub regime: RegimeTuning,
}

impl Default for SweepProfile {
    fn default() -> Self {
        Self {
            starting_cash: 100.0,
            ticket_dollars: 2.0,
            min_entry_price: 0.05,
            max_entry_price: 0.99,
            max_tick_frac: 0.03,
            take_profit_price: 0.98,
            min_exit_roi: 0.03,
            hold_green_secs: 15,
            min_edge_to_buy: 0.01,
            max_spread: 0.03,
            max_pair_ask_sum: 1.02,
            max_open_positions: 3,
            paper_real_mode: true,
            min_model_score: 0.08,
            min_fair_gap_after_cost: 0.004,
            min_net_edge_buy: 0.003,
            min_net_edge_sell: -0.002,
            entry_slippage_bps: 8.0,
            exit_slippage_bps: 10.0,
            no_new_entry_expiry_secs: 45,
            reduce_size_expiry_secs: 120,
            max_hold_secs: 90,
            max_stale_fair_secs: 20,
            min_fair_improve: 0.003,
            scratch_edge: -0.004,
            trailing_drawdown_frac: 0.45,
            max_void_score: 0.68,
            min_wall_score: 0.18,
            max_cancel_skew: 0.9,
            min_bid_size: 25.0,
            min_visible_depth: 150.0,
            min_queue_fill_score: 0.28,
            min_queue_trade_intensity: 2.0,
            min_opportunity_score: 0.20,
            aggressive_entry_score: 0.72,
            winner_hold_edge: 0.003,
            winner_hold_wall_score: 0.22,
            flow_target_per_min: 0.10,
            flow_window_secs: 300,
            min_cycle_rate_per_min: 0.40,
            maker_band_dist: 0.08,
            flow_reversion_edge: 0.002,
            flow_exit_edge: 0.0015,
            flow_size_mult: 0.6,
            flow_max_hold_secs: 30,
            impulse: ImpulseProfile::default(),
            reversal: ReversalProfile::default(),
            regime: RegimeTuning::default(),
        }
    }
}

fn default_hold_green_secs() -> u64 {
    15
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ImpulseProfile {
    pub enabled: bool,
    pub min_ret_250ms: f64,
    pub min_ret_1s: f64,
    pub min_accel: f64,
    pub min_trade_intensity: f64,
    pub size_mult: f64,
    pub max_hold_secs: u64,
    pub min_overshoot_edge: f64,
    pub velocity_window_ms: u64,
    pub min_velocity: f64,
    pub min_peak_imbalance: f64,
    pub min_peak_wall_score: f64,
    pub fade_reentry_edge: f64,
    pub entry_ttl_ms: u64,
    pub fade_ttl_ms: u64,
}

impl Default for ImpulseProfile {
    fn default() -> Self {
        Self {
            enabled: false,
            min_ret_250ms: 0.0012,
            min_ret_1s: 0.0020,
            min_accel: 0.0004,
            min_trade_intensity: 2.5,
            size_mult: 1.25,
            max_hold_secs: 8,
            min_overshoot_edge: 0.006,
            velocity_window_ms: 800,
            min_velocity: 0.015,
            min_peak_imbalance: 0.22,
            min_peak_wall_score: 0.18,
            fade_reentry_edge: 0.008,
            entry_ttl_ms: 120,
            fade_ttl_ms: 120,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ReversalProfile {
    pub enabled: bool,
    pub capture_secs: u64,
    pub start_secs: u64,
    pub end_secs: u64,
    pub min_drawdown_bps: f64,
    pub min_rebound_bps: f64,
    pub min_rebound_frac: f64,
    pub min_wick_bias: f64,
    pub min_signal_score: f64,
    pub ttl_ms: u64,
}

impl Default for ReversalProfile {
    fn default() -> Self {
        Self {
            enabled: false,
            capture_secs: 75,
            start_secs: 75,
            end_secs: 150,
            min_drawdown_bps: 6.0,
            min_rebound_bps: 2.0,
            min_rebound_frac: 0.35,
            min_wick_bias: 0.08,
            min_signal_score: 0.55,
            ttl_ms: 800,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RiskProfile {
    pub inv_limit: f64,
    pub base_qty: f64,
    pub max_loss: f64,
    pub max_loss_5m: f64,
    pub asset_notional_limit: f64,
    pub regime_notional_limit: f64,
    pub corr_bucket_notional_limit: f64,
}

impl Default for RiskProfile {
    fn default() -> Self {
        Self {
            inv_limit: 100.0,
            base_qty: 10.0,
            max_loss: 0.03,
            max_loss_5m: 0.02,
            asset_notional_limit: 25.0,
            regime_notional_limit: 40.0,
            corr_bucket_notional_limit: 25.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RegimeTuning {
    pub continuation: RegimeAdjust,
    pub reversion: RegimeAdjust,
    pub chop: RegimeAdjust,
    pub burst: RegimeAdjust,
    pub expiry_pinch: RegimeAdjust,
}

impl Default for RegimeTuning {
    fn default() -> Self {
        Self {
            continuation: RegimeAdjust {
                edge_mult: 0.85,
                spread_mult: 1.15,
                ticket_mult: 1.2,
                exit_only: false,
            },
            reversion: RegimeAdjust::default(),
            chop: RegimeAdjust {
                edge_mult: 1.35,
                spread_mult: 0.7,
                ticket_mult: 0.6,
                exit_only: false,
            },
            burst: RegimeAdjust {
                edge_mult: 0.75,
                spread_mult: 1.2,
                ticket_mult: 1.35,
                exit_only: false,
            },
            expiry_pinch: RegimeAdjust {
                edge_mult: 2.0,
                spread_mult: 0.5,
                ticket_mult: 0.0,
                exit_only: true,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RegimeAdjust {
    pub edge_mult: f64,
    pub spread_mult: f64,
    pub ticket_mult: f64,
    pub exit_only: bool,
}

impl Default for RegimeAdjust {
    fn default() -> Self {
        Self {
            edge_mult: 1.0,
            spread_mult: 1.0,
            ticket_mult: 1.0,
            exit_only: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
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
    pub score_scale: f64,
    pub fair_base_band: f64,
    pub fair_spread_mult: f64,
    pub fair_max_band: f64,
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
            score_scale: 1.0,
            fair_base_band: 0.03,
            fair_spread_mult: 1.5,
            fair_max_band: 0.12,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppProfile {
    #[serde(default)]
    pub mode: RunMode,
    pub record_path: Option<String>,
    #[serde(default)]
    pub execution: ExecutionProfile,
    #[serde(default)]
    pub adapter: AdapterProfile,
    #[serde(default)]
    pub sweep: SweepProfile,
    #[serde(default)]
    pub risk: RiskProfile,
    #[serde(default)]
    pub model_weights: ModelWeights,
}

impl Default for AppProfile {
    fn default() -> Self {
        Self {
            mode: RunMode::Live,
            record_path: None,
            execution: ExecutionProfile::default(),
            adapter: AdapterProfile::default(),
            sweep: SweepProfile::default(),
            risk: RiskProfile::default(),
            model_weights: ModelWeights::default(),
        }
    }
}

pub fn load_profile(path: impl AsRef<Path>) -> Result<AppProfile> {
    let path = path.as_ref();
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read profile {}", path.display()))?;
    let profile: AppProfile = toml::from_str(&raw)
        .with_context(|| format!("failed to parse profile {}", path.display()))?;
    Ok(profile)
}
