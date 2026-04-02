use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    Live,
    Replay,
    Backtest,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    Paper,
    Live,
}

impl Default for ExecutionMode {
    fn default() -> Self {
        Self::Paper
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WalletSignatureType {
    Eoa,
    Proxy,
    GnosisSafe,
}

impl Default for WalletSignatureType {
    fn default() -> Self {
        Self::Eoa
    }
}

impl Default for RunMode {
    fn default() -> Self {
        Self::Live
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetConfig {
    pub name: String,
    pub oracle: String,
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
}

impl Default for AdapterProfile {
    fn default() -> Self {
        Self {
            assets: vec![AssetConfig {
                name: "BTC".to_string(),
                oracle: "0xc907E116054Ad103354f2D350FD2514433D57F6f".to_string(),
                rtds_symbol: "btcusdt".to_string(),
                slug_prefixes: vec!["bitcoin".to_string(), "btc".to_string()],
            }],
            clob_ws_enabled: true,
            rtds_enabled: true,
            chainlink_fallback_enabled: true,
            universe_refresh_secs: 20,
            polygon_rpc_url: "https://polygon.drpc.org".to_string(),
        }
    }
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
    pub live: LiveProfile,
}

impl Default for ExecutionProfile {
    fn default() -> Self {
        Self {
            mode: ExecutionMode::Paper,
            live: LiveProfile::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweepProfile {
    pub starting_cash: f64,
    pub ticket_dollars: f64,
    pub max_entry_price: f64,
    pub take_profit_price: f64,
    pub min_edge_to_buy: f64,
    pub max_spread: f64,
    pub max_pair_ask_sum: f64,
    pub paper_real_mode: bool,
}

impl Default for SweepProfile {
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskProfile {
    pub inv_limit: f64,
    pub base_qty: f64,
    pub max_loss: f64,
}

impl Default for RiskProfile {
    fn default() -> Self {
        Self {
            inv_limit: 100.0,
            base_qty: 10.0,
            max_loss: 0.03,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
