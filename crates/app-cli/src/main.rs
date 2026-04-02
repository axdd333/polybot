use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use polymarket_adapter::spawn_live_feeds;
use std::path::PathBuf;
use std::time::Duration;
use strategy_sweep::build_engine;
use tokio::sync::mpsc;
use trading_core::config::{load_profile, AppProfile, RunMode};
use trading_core::events::NormalizedEvent;
use trading_core::replay::{read_recorded_events, EventRecorder};

#[derive(Parser)]
#[command(name = "app-cli")]
#[command(about = "Operator CLI for the rebuilt Polymarket platform")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Live {
        #[arg(long, default_value = "config/live.toml")]
        profile: PathBuf,
    },
    Replay {
        #[arg(long)]
        events: PathBuf,
        #[arg(long, default_value = "config/replay.toml")]
        profile: PathBuf,
    },
    Backtest {
        #[arg(long)]
        events: PathBuf,
        #[arg(long, default_value = "config/backtest.toml")]
        profile: PathBuf,
    },
    Record {
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value_t = 60)]
        duration_secs: u64,
        #[arg(long, default_value = "config/live.toml")]
        profile: PathBuf,
    },
    InspectConfig {
        #[arg(long, default_value = "config/live.toml")]
        profile: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Live { profile } => app_tui::run(load_profile(profile)?).await,
        Command::Replay { events, profile } => run_offline(load_profile(profile)?, events, RunMode::Replay),
        Command::Backtest { events, profile } => run_offline(load_profile(profile)?, events, RunMode::Backtest),
        Command::Record {
            output,
            duration_secs,
            profile,
        } => run_record(load_profile(profile)?, output, duration_secs).await,
        Command::InspectConfig { profile } => {
            let profile = load_profile(profile)?;
            println!("{}", serde_json::to_string_pretty(&profile)?);
            Ok(())
        }
    }
}

fn run_offline(profile: AppProfile, events_path: PathBuf, mode: RunMode) -> Result<()> {
    let mut engine = build_engine(&profile, mode);
    for event in read_recorded_events(events_path)? {
        engine.apply_event(event);
        engine.refresh_dirty_markets();
    }

    let snapshot = engine.snapshot();
    let total_pnl = snapshot.equity - profile.sweep.starting_cash;
    println!(
        "mode={:?} cash=${:.2} equity=${:.2} total_pnl=${:.2} realized=${:.2} unrealized=${:.2} open_positions={}",
        mode,
        snapshot.cash,
        snapshot.equity,
        total_pnl,
        snapshot.realized_pnl,
        snapshot.unrealized_pnl,
        snapshot.open_positions,
    );
    if let Some(market) = snapshot.markets.first() {
        println!(
            "top_market={} {} {} ask={} fair={} edge={:+.1}c",
            market.symbol,
            market.window_label,
            match market.side {
                trading_core::market::types::Side::Up => "U",
                trading_core::market::types::Side::Down => "D",
            },
            trading_core::market::quote::cents_or_dash(market.best_ask),
            trading_core::market::quote::cents_or_dash(market.fair_value),
            market.edge_buy * 100.0,
        );
    }
    if total_pnl <= 0.0 {
        bail!(
            "{mode:?} finished non-positive: total_pnl=${total_pnl:.2}, realized=${:.2}, unrealized=${:.2}",
            snapshot.realized_pnl,
            snapshot.unrealized_pnl
        );
    }
    Ok(())
}

async fn run_record(profile: AppProfile, output: PathBuf, duration_secs: u64) -> Result<()> {
    let mut recorder = EventRecorder::create(&output)?;
    let (tx, mut rx) = mpsc::channel::<NormalizedEvent>(1024);
    let handles = spawn_live_feeds(profile.adapter, tx);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(duration_secs);

    loop {
        tokio::select! {
            maybe_event = rx.recv() => {
                let Some(event) = maybe_event else { break; };
                recorder.record(&event)?;
            }
            _ = tokio::time::sleep_until(deadline) => {
                break;
            }
        }
    }

    recorder.flush()?;
    for handle in handles {
        handle.abort();
    }
    println!("recorded normalized events to {}", output.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backtest_fixture_stays_profitable() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../");
        let profile = load_profile(root.join("config/backtest.toml")).unwrap();
        let events = root.join("data/fixtures/sample_positive.ndjson");
        assert!(run_offline(profile, events, RunMode::Backtest).is_ok());
    }
}
